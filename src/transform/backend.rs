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
use crate::pangolin::types::{L4Service, LoadBalancerServer, Service as PangService};
use crate::transform::Desired;
use crate::transform::l4::L4Protocol;
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

        let entries = parse_url_servers(pang_name, &lb.servers);
        let classification = classify_entries(pang_name, &entries, cfg.backend_kind);
        if let Some(resolved) = emit_backend(cfg, pang_name, classification, "", "TCP", desired) {
            index.insert(pang_name.clone(), resolved);
        }
    }

    index
}

/// L4 sibling of [`build_backends`]: pangolin's `tcp`/`udp` services carry
/// `address` (`host:port`) entries instead of URLs. Synthesized object names get
/// a protocol infix (`be-tcp-…`) so a pangolin service name reused across the
/// http/tcp/udp blocks can't collide, and `service`-mode stubs carry the right
/// port protocol.
pub fn build_l4_backends(
    cfg: &Config,
    services: &BTreeMap<String, L4Service>,
    proto: L4Protocol,
    desired: &mut Desired,
) -> BackendIndex {
    let mut index = BackendIndex::new();

    for (pang_name, svc) in services {
        let Some(lb) = svc.load_balancer.as_ref() else {
            if svc.weighted.is_some() {
                warn!(service = %pang_name, "weighted L4 services are not supported");
            }
            continue;
        };
        if lb.servers.is_empty() {
            warn!(service = %pang_name, "pangolin L4 service has no servers; skipping");
            continue;
        }

        let entries: Vec<HostPort> = lb
            .servers
            .iter()
            .filter_map(|s| parse_l4_address(pang_name, &s.address))
            .collect();
        let classification = classify_entries(pang_name, &entries, cfg.backend_kind);
        if let Some(resolved) = emit_backend(
            cfg,
            pang_name,
            classification,
            proto.infix(),
            proto.k8s_protocol(),
            desired,
        ) {
            index.insert(pang_name.clone(), resolved);
        }
    }

    index
}

/// Dispatch a classification to the right emitter. `infix` distinguishes
/// synthesized object names per traffic family (`""` for http, `"tcp"`/`"udp"`),
/// `protocol` is the Kubernetes port protocol for Service/EndpointSlice stubs.
fn emit_backend(
    cfg: &Config,
    pang_name: &str,
    classification: Classification,
    infix: &str,
    protocol: &str,
    desired: &mut Desired,
) -> Option<ResolvedBackend> {
    match classification {
        Classification::Empty => None,
        Classification::Ips { entries, port } => Some(emit_ip_backend(
            cfg, pang_name, &entries, port, infix, protocol, desired,
        )),
        // classify_entries() only returns Fqdn in EnvoyBackend mode.
        Classification::Fqdn { hostname, port } => Some(emit_fqdn_backend(
            cfg, pang_name, &hostname, port, infix, desired,
        )),
        Classification::ClusterDns {
            service,
            namespace,
            port,
        } => Some(ResolvedBackend {
            group: String::new(),
            kind: "Service".into(),
            name: service,
            namespace: Some(namespace),
            port,
        }),
    }
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

/// A server target reduced to its host + port, whatever syntax it arrived in.
struct HostPort {
    host: String,
    port: i32,
}

fn parse_url_servers(pang_name: &str, servers: &[LoadBalancerServer]) -> Vec<HostPort> {
    let mut entries = Vec::new();
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
        entries.push(HostPort { host, port });
    }
    entries
}

/// Parse a Traefik L4 server `address` (`host:port`, IPv6 in brackets). Pangolin
/// has been seen prefixing a scheme even for UDP targets, so a leading
/// `<scheme>://` is tolerated and stripped.
fn parse_l4_address(pang_name: &str, address: &str) -> Option<HostPort> {
    let trimmed = address.trim();
    let without_scheme = match trimmed.split_once("://") {
        Some((_, rest)) => rest,
        None => trimmed,
    };
    let (host, port_str) = if let Some(rest) = without_scheme.strip_prefix('[') {
        // Bracketed IPv6: [::1]:8080
        let (host, after) = rest.split_once(']')?;
        (host, after.strip_prefix(':'))
    } else {
        match without_scheme.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (without_scheme, None),
        }
    };
    let Some(port_str) = port_str else {
        warn!(service = %pang_name, address = %address, "L4 server address has no port; skipping");
        return None;
    };
    let port: u16 = match port_str.parse() {
        Ok(p) => p,
        Err(_) => {
            warn!(service = %pang_name, address = %address, "L4 server address has an invalid port; skipping");
            return None;
        }
    };
    if host.is_empty() {
        warn!(service = %pang_name, address = %address, "L4 server address has no host; skipping");
        return None;
    }
    Some(HostPort {
        host: host.to_string(),
        port: i32::from(port),
    })
}

fn classify_entries(pang_name: &str, entries: &[HostPort], kind: BackendKind) -> Classification {
    let mut ips: Vec<IpAddr> = Vec::new();
    let mut ip_port: Option<i32> = None;
    let mut cluster: Option<(String, String, i32)> = None;
    let mut fqdn: Option<(String, i32)> = None;

    for HostPort { host, port } in entries {
        let (host, port) = (host.clone(), *port);
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

/// `<base>-<infix>` when an infix is set, plain `<base>` otherwise. Keeps the
/// established http-mode names (`be-…`, `svc-…`) stable.
fn name_prefix(base: &str, infix: &str) -> String {
    if infix.is_empty() {
        base.to_string()
    } else {
        format!("{base}-{infix}")
    }
}

fn emit_ip_backend(
    cfg: &Config,
    pang_name: &str,
    ips: &[IpAddr],
    port: i32,
    infix: &str,
    protocol: &str,
    desired: &mut Desired,
) -> ResolvedBackend {
    match cfg.backend_kind {
        BackendKind::Service => {
            emit_synthesized_service(cfg, pang_name, ips, port, infix, protocol, desired)
        }
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
            emit_envoy_backend(cfg, pang_name, endpoints, port, infix, desired)
        }
    }
}

fn emit_fqdn_backend(
    cfg: &Config,
    pang_name: &str,
    hostname: &str,
    port: i32,
    infix: &str,
    desired: &mut Desired,
) -> ResolvedBackend {
    let endpoints = vec![BackendEndpoint {
        ip: None,
        fqdn: Some(BackendFqdn {
            hostname: hostname.to_string(),
            port,
        }),
    }];
    emit_envoy_backend(cfg, pang_name, endpoints, port, infix, desired)
}

fn emit_envoy_backend(
    cfg: &Config,
    pang_name: &str,
    endpoints: Vec<BackendEndpoint>,
    port: i32,
    infix: &str,
    desired: &mut Desired,
) -> ResolvedBackend {
    let name = prefixed_label(&name_prefix("be", infix), pang_name);
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
    infix: &str,
    protocol: &str,
    desired: &mut Desired,
) -> ResolvedBackend {
    let svc_name = prefixed_label(&name_prefix("svc", infix), pang_name);
    let slice_name = prefixed_label(&name_prefix("eps", infix), pang_name);
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
                protocol: Some(protocol.into()),
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
            protocol: Some(protocol.into()),
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

    fn classify(
        pang_name: &str,
        servers: &[LoadBalancerServer],
        kind: BackendKind,
    ) -> Classification {
        classify_entries(pang_name, &parse_url_servers(pang_name, servers), kind)
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

    #[test]
    fn l4_address_plain_host_port() {
        let hp = parse_l4_address("svc", "10.0.0.5:8000").expect("parsed");
        assert_eq!(hp.host, "10.0.0.5");
        assert_eq!(hp.port, 8000);
    }

    #[test]
    fn l4_address_strips_stray_scheme() {
        // Pangolin emits this shape for raw UDP resources.
        let hp = parse_l4_address("svc", "http://echo.foo.svc.cluster.local:9090").expect("parsed");
        assert_eq!(hp.host, "echo.foo.svc.cluster.local");
        assert_eq!(hp.port, 9090);
    }

    #[test]
    fn l4_address_bracketed_ipv6() {
        let hp = parse_l4_address("svc", "[2001:db8::1]:53").expect("parsed");
        assert_eq!(hp.host, "2001:db8::1");
        assert_eq!(hp.port, 53);
    }

    #[test]
    fn l4_address_requires_port() {
        assert!(parse_l4_address("svc", "10.0.0.5").is_none());
        assert!(parse_l4_address("svc", "host:notaport").is_none());
        assert!(parse_l4_address("svc", ":8080").is_none());
    }

    #[test]
    fn l4_cluster_dns_classifies_directly() {
        let entries =
            vec![parse_l4_address("svc", "show.dummyservices.svc.cluster.local:8000").unwrap()];
        match classify_entries("svc", &entries, BackendKind::EnvoyBackend) {
            Classification::ClusterDns {
                service,
                namespace,
                port,
            } => {
                assert_eq!(service, "show");
                assert_eq!(namespace, "dummyservices");
                assert_eq!(port, 8000);
            }
            _ => panic!("expected ClusterDns"),
        }
    }
}
