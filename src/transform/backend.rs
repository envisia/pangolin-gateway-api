//! Map pangolin's `http.services.{name}.loadBalancer.servers[]` lists to backend references.
//!
//! Pangolin emits servers as `{ url: "http://host:port" }`. We classify each
//! pangolin service by the kind of host(s) it carries and then either:
//!
//! * **IP literal(s)** — synthesize backend objects according to
//!   [`BackendKind`]: either a headless `Service` + `EndpointSlice` (portable
//!   across every Gateway API implementation) or an Envoy Gateway `Backend`
//!   CRD (Envoy Gateway only).
//! * **Kubernetes cluster DNS** (`<svc>.<ns>.svc[.cluster.local]`) — reference
//!   the existing `Service` directly. No synthesis in either mode, because the
//!   real Service already exists and is the right target.
//! * **Arbitrary FQDN** — only supported in `envoy-backend` mode (Envoy
//!   Gateway's `Backend` accepts FQDN endpoints natively). In `service` mode
//!   the entry is logged and dropped, since an `EndpointSlice` can't carry a
//!   hostname.

use std::collections::BTreeMap;
use std::net::IpAddr;

use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
use k8s_openapi::api::discovery::v1::{Endpoint, EndpointPort, EndpointSlice};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use tracing::warn;
use url::Url;

use crate::apply::{managed_metadata, owner_labels};
use crate::config::{BackendKind, Config};
use crate::envoy_gateway::{Backend, BackendEndpoint, BackendFqdn, BackendIp, BackendSpec};
use crate::pangolin::types::{LoadBalancerServer, Service as PangService};
use crate::transform::Desired;
use crate::transform::naming::prefixed_label;

pub type BackendIndex = BTreeMap<String, ResolvedBackend>;

/// Result of resolving a pangolin service to a Gateway API `backendRef` target.
#[derive(Debug, Clone)]
pub struct ResolvedBackend {
    pub group: String,
    pub kind: String,
    pub name: String,
    /// Cross-namespace reference — set when the backend lives outside the
    /// controller's namespace (then the HTTPRoute needs a ReferenceGrant).
    pub namespace: Option<String>,
    pub port: i32,
}

const PORT_NAME: &str = "default";

pub fn build_backends(
    cfg: &Config,
    services: &BTreeMap<String, PangService>,
    desired: &mut Desired,
) -> BackendIndex {
    let mut index = BackendIndex::new();

    for (pang_name, svc) in services {
        let Some(lb) = svc.load_balancer.as_ref() else {
            if svc.weighted.is_some() || svc.mirroring.is_some() {
                warn!(service = %pang_name, "weighted/mirroring services are not yet supported");
            }
            continue;
        };
        if lb.servers.is_empty() {
            warn!(service = %pang_name, "pangolin service has no servers; skipping");
            continue;
        }

        match classify(pang_name, &lb.servers, cfg.backend_kind) {
            Classification::Empty => continue,
            Classification::Ips { entries, port } => {
                let resolved = emit_ip_backend(cfg, pang_name, &entries, port, desired);
                index.insert(pang_name.clone(), resolved);
            }
            Classification::Fqdn { hostname, port } => {
                // classify() only returns Fqdn in EnvoyBackend mode.
                let resolved = emit_fqdn_backend(cfg, pang_name, &hostname, port, desired);
                index.insert(pang_name.clone(), resolved);
            }
            Classification::ClusterDns {
                service,
                namespace,
                port,
            } => {
                index.insert(
                    pang_name.clone(),
                    ResolvedBackend {
                        group: String::new(),
                        kind: "Service".into(),
                        name: service,
                        namespace: Some(namespace),
                        port,
                    },
                );
            }
        }
    }

    index
}

enum Classification {
    Empty,
    Ips {
        entries: Vec<IpAddr>,
        port: i32,
    },
    Fqdn {
        hostname: String,
        port: i32,
    },
    ClusterDns {
        service: String,
        namespace: String,
        port: i32,
    },
}

fn classify(pang_name: &str, servers: &[LoadBalancerServer], kind: BackendKind) -> Classification {
    let mut ips: Vec<IpAddr> = Vec::new();
    let mut ip_port: Option<i32> = None;
    let mut cluster: Option<(String, String, i32)> = None;
    let mut fqdn: Option<(String, i32)> = None;

    for s in servers {
        let url = match Url::parse(&s.url) {
            Ok(u) => u,
            Err(e) => {
                warn!(service = %pang_name, url = %s.url, error = %e, "invalid server url; skipping");
                continue;
            }
        };
        let host = match url.host_str() {
            Some(h) => h.trim_start_matches('[').trim_end_matches(']').to_string(),
            None => {
                warn!(service = %pang_name, url = %s.url, "server url has no host; skipping");
                continue;
            }
        };
        let port = match url.port_or_known_default() {
            Some(p) => p as i32,
            None => {
                warn!(service = %pang_name, url = %s.url, "no port and no default; skipping");
                continue;
            }
        };

        if let Ok(ip) = host.parse::<IpAddr>() {
            if ip_port.is_some_and(|p| p != port) {
                warn!(
                    service = %pang_name,
                    "pangolin service has IP servers with mixed ports; using first ({:?})", ip_port
                );
                continue;
            }
            ip_port.get_or_insert(port);
            ips.push(ip);
            continue;
        }

        if let Some((svc, ns)) = parse_cluster_dns(&host) {
            if cluster.is_some() {
                warn!(
                    service = %pang_name,
                    "multiple cluster-DNS backends in one pangolin service; using the first"
                );
                continue;
            }
            cluster = Some((svc, ns, port));
            continue;
        }

        // Bare FQDN — only emit when we have a backend kind that can carry it.
        match kind {
            BackendKind::EnvoyBackend => {
                if fqdn.is_some() {
                    warn!(
                        service = %pang_name,
                        "multiple FQDN backends in one pangolin service; using the first"
                    );
                    continue;
                }
                fqdn = Some((host, port));
            }
            BackendKind::Service => {
                warn!(
                    service = %pang_name,
                    host = %host,
                    "FQDN backend hosts require CONFIG_BACKEND_KIND=envoy-backend; skipping"
                );
            }
        }
    }

    // Prefer the most informative classification. Multiple kinds in one
    // pangolin service is unusual, so warn loudly when it happens.
    let kinds_present = [!ips.is_empty(), cluster.is_some(), fqdn.is_some()]
        .iter()
        .filter(|x| **x)
        .count();
    if kinds_present > 1 {
        warn!(
            service = %pang_name,
            "pangolin service mixes backend kinds; preferring IP > cluster-DNS > FQDN"
        );
    }

    if !ips.is_empty() {
        return Classification::Ips {
            entries: ips,
            port: ip_port.unwrap_or(80),
        };
    }
    if let Some((svc, ns, port)) = cluster {
        return Classification::ClusterDns {
            service: svc,
            namespace: ns,
            port,
        };
    }
    if let Some((hostname, port)) = fqdn {
        return Classification::Fqdn { hostname, port };
    }
    Classification::Empty
}

/// Parse `<svc>.<ns>.svc` or `<svc>.<ns>.svc.cluster.local` (with optional further
/// search-domain suffix) into `(svc, ns)`.
fn parse_cluster_dns(host: &str) -> Option<(String, String)> {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    let parts: Vec<&str> = normalized.split('.').collect();
    if parts.len() < 3 || parts[2] != "svc" {
        return None;
    }
    Some((parts[0].to_string(), parts[1].to_string()))
}

fn emit_ip_backend(
    cfg: &Config,
    pang_name: &str,
    ips: &[IpAddr],
    port: i32,
    desired: &mut Desired,
) -> ResolvedBackend {
    match cfg.backend_kind {
        BackendKind::Service => emit_synthesized_service(cfg, pang_name, ips, port, desired),
        BackendKind::EnvoyBackend => {
            let endpoints = ips
                .iter()
                .map(|ip| BackendEndpoint {
                    ip: Some(BackendIp {
                        address: ip.to_string(),
                        port,
                    }),
                    fqdn: None,
                })
                .collect();
            emit_envoy_backend(cfg, pang_name, endpoints, port, desired)
        }
    }
}

fn emit_fqdn_backend(
    cfg: &Config,
    pang_name: &str,
    hostname: &str,
    port: i32,
    desired: &mut Desired,
) -> ResolvedBackend {
    let endpoints = vec![BackendEndpoint {
        ip: None,
        fqdn: Some(BackendFqdn {
            hostname: hostname.to_string(),
            port,
        }),
    }];
    emit_envoy_backend(cfg, pang_name, endpoints, port, desired)
}

fn emit_envoy_backend(
    cfg: &Config,
    pang_name: &str,
    endpoints: Vec<BackendEndpoint>,
    port: i32,
    desired: &mut Desired,
) -> ResolvedBackend {
    let name = prefixed_label("be", pang_name);
    let labels = owner_labels(cfg, &name);

    let backend = Backend {
        metadata: managed_metadata(cfg, &name, labels),
        spec: BackendSpec { endpoints },
        // status field is not modeled (kube::CustomResource doesn't add one
        // when we don't declare it).
    };

    desired.envoy_backends.insert(name.clone(), backend);

    ResolvedBackend {
        group: "gateway.envoyproxy.io".into(),
        kind: "Backend".into(),
        name,
        namespace: None,
        port,
    }
}

fn emit_synthesized_service(
    cfg: &Config,
    pang_name: &str,
    ips: &[IpAddr],
    port: i32,
    desired: &mut Desired,
) -> ResolvedBackend {
    let svc_name = prefixed_label("svc", pang_name);
    let slice_name = prefixed_label("eps", pang_name);
    let labels = owner_labels(cfg, &svc_name);

    let service = Service {
        metadata: managed_metadata(cfg, &svc_name, labels.clone()),
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".into()),
            type_: Some("ClusterIP".into()),
            ports: Some(vec![ServicePort {
                name: Some(PORT_NAME.into()),
                port,
                target_port: Some(IntOrString::Int(port)),
                protocol: Some("TCP".into()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    let mut slice_labels = labels.clone();
    slice_labels.insert("kubernetes.io/service-name".into(), svc_name.clone());
    let slice_metadata = managed_metadata(cfg, &slice_name, slice_labels);

    let endpoint_slice = EndpointSlice {
        metadata: slice_metadata,
        address_type: "IPv4".into(),
        endpoints: ips
            .iter()
            .map(|ip| Endpoint {
                addresses: vec![ip.to_string()],
                conditions: None,
                ..Default::default()
            })
            .collect(),
        ports: Some(vec![EndpointPort {
            name: Some(PORT_NAME.into()),
            port: Some(port),
            protocol: Some("TCP".into()),
            ..Default::default()
        }]),
    };

    desired.services.insert(svc_name.clone(), service);
    desired.endpoint_slices.insert(slice_name, endpoint_slice);

    ResolvedBackend {
        group: String::new(),
        kind: "Service".into(),
        name: svc_name,
        namespace: None,
        port,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(url: &str) -> LoadBalancerServer {
        LoadBalancerServer { url: url.into() }
    }

    #[test]
    fn classifies_ipv4_service_mode() {
        match classify("svc", &[s("http://10.0.0.1:8080")], BackendKind::Service) {
            Classification::Ips { entries, port } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(port, 8080);
            }
            _ => panic!("expected Ips"),
        }
    }

    #[test]
    fn classifies_cluster_dns_short() {
        match classify(
            "svc",
            &[s("http://echo.foo.svc:8080")],
            BackendKind::Service,
        ) {
            Classification::ClusterDns {
                service,
                namespace,
                port,
            } => {
                assert_eq!(service, "echo");
                assert_eq!(namespace, "foo");
                assert_eq!(port, 8080);
            }
            _ => panic!("expected ClusterDns"),
        }
    }

    #[test]
    fn classifies_cluster_dns_full() {
        match classify(
            "svc",
            &[s("http://echo.foo.svc.cluster.local:80")],
            BackendKind::Service,
        ) {
            Classification::ClusterDns {
                service,
                namespace,
                port,
            } => {
                assert_eq!(service, "echo");
                assert_eq!(namespace, "foo");
                assert_eq!(port, 80);
            }
            _ => panic!("expected ClusterDns"),
        }
    }

    #[test]
    fn fqdn_drops_in_service_mode() {
        assert!(matches!(
            classify(
                "svc",
                &[s("http://api.example.com:80")],
                BackendKind::Service,
            ),
            Classification::Empty
        ));
    }

    #[test]
    fn fqdn_kept_in_envoy_backend_mode() {
        match classify(
            "svc",
            &[s("https://api.example.com")],
            BackendKind::EnvoyBackend,
        ) {
            Classification::Fqdn { hostname, port } => {
                assert_eq!(hostname, "api.example.com");
                assert_eq!(port, 443);
            }
            _ => panic!("expected Fqdn"),
        }
    }
}
