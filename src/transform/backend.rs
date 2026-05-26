//! Map pangolin's `http.services.{name}.loadBalancer.servers[]` lists to backend references.
//!
//! Pangolin emits servers as `{ url: "http://host:port" }`. The `host` is either:
//!
//! * an IP literal — pangolin is tunneling to an external endpoint, so we synthesize a
//!   headless `Service` + `EndpointSlice` to carry the IP(s);
//! * a Kubernetes cluster DNS name (`<svc>.<ns>.svc[.cluster.local]`) — we reference
//!   the existing Service directly with no synthesis; or
//! * any other hostname — currently unsupported (would need an ExternalName Service per
//!   target, which can't be combined with load balancing).

use std::collections::BTreeMap;
use std::net::IpAddr;

use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
use k8s_openapi::api::discovery::v1::{Endpoint, EndpointPort, EndpointSlice};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use tracing::warn;
use url::Url;

use crate::apply::{managed_metadata, owner_labels};
use crate::config::Config;
use crate::pangolin::types::{LoadBalancerServer, Service as PangService};
use crate::transform::Desired;
use crate::transform::naming::prefixed_label;

pub type BackendIndex = BTreeMap<String, ResolvedBackend>;

#[derive(Debug, Clone)]
pub struct ResolvedBackend {
    pub service_name: String,
    /// Cross-namespace reference — set when the backend lives outside the controller's
    /// namespace (then the HTTPRoute needs a ReferenceGrant).
    pub service_namespace: Option<String>,
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

        match classify(pang_name, &lb.servers) {
            Classification::Empty => {
                continue;
            }
            Classification::Ips { entries, port } => {
                emit_synthesized_service(cfg, pang_name, &entries, port, desired);
                index.insert(
                    pang_name.clone(),
                    ResolvedBackend {
                        service_name: prefixed_label("svc", pang_name),
                        service_namespace: None,
                        port,
                    },
                );
            }
            Classification::ClusterDns {
                service,
                namespace,
                port,
            } => {
                index.insert(
                    pang_name.clone(),
                    ResolvedBackend {
                        service_name: service,
                        service_namespace: Some(namespace),
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
    ClusterDns {
        service: String,
        namespace: String,
        port: i32,
    },
}

fn classify(pang_name: &str, servers: &[LoadBalancerServer]) -> Classification {
    let mut ips: Vec<IpAddr> = Vec::new();
    let mut cluster: Option<(String, String, i32)> = None;
    let mut port_hint: Option<i32> = None;

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
            ips.push(ip);
            port_hint.get_or_insert(port);
            continue;
        }

        if let Some((svc, ns)) = parse_cluster_dns(&host) {
            if cluster.is_some() {
                warn!(
                    service = %pang_name,
                    "multiple cluster-DNS backends in one pangolin service; only the first is used"
                );
                continue;
            }
            cluster = Some((svc, ns, port));
            continue;
        }

        warn!(
            service = %pang_name,
            host = %host,
            "unsupported backend host kind (expected IP literal or *.svc.cluster.local); skipping"
        );
    }

    if !ips.is_empty() && cluster.is_some() {
        warn!(
            service = %pang_name,
            "pangolin service mixes IP and cluster-DNS backends; preferring IP backends"
        );
        cluster = None;
    }

    if let Some(port) = port_hint
        && !ips.is_empty()
    {
        return Classification::Ips { entries: ips, port };
    }
    if let Some((svc, ns, port)) = cluster {
        return Classification::ClusterDns {
            service: svc,
            namespace: ns,
            port,
        };
    }
    Classification::Empty
}

/// Parse `<svc>.<ns>.svc` or `<svc>.<ns>.svc.cluster.local` (with optional further
/// search-domain suffix) into `(svc, ns)`.
fn parse_cluster_dns(host: &str) -> Option<(String, String)> {
    // Lowercase compare; FQDN-style trailing dot tolerated.
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    let parts: Vec<&str> = normalized.split('.').collect();
    if parts.len() < 3 {
        return None;
    }
    // parts: [svc, ns, "svc", ...rest]
    if parts[2] != "svc" {
        return None;
    }
    Some((parts[0].to_string(), parts[1].to_string()))
}

fn emit_synthesized_service(
    cfg: &Config,
    pang_name: &str,
    ips: &[IpAddr],
    port: i32,
    desired: &mut Desired,
) {
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

    desired.services.insert(svc_name, service);
    desired.endpoint_slices.insert(slice_name, endpoint_slice);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(url: &str) -> LoadBalancerServer {
        LoadBalancerServer { url: url.into() }
    }

    #[test]
    fn classifies_ipv4() {
        match classify("svc", &[s("http://10.0.0.1:8080")]) {
            Classification::Ips { entries, port } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(port, 8080);
            }
            _ => panic!("expected Ips"),
        }
    }

    #[test]
    fn classifies_cluster_dns_short() {
        match classify("svc", &[s("http://echo.foo.svc:8080")]) {
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
        ) {
            Classification::ClusterDns {
                service, namespace, port,
            } => {
                assert_eq!(service, "echo");
                assert_eq!(namespace, "foo");
                assert_eq!(port, 80);
            }
            _ => panic!("expected ClusterDns"),
        }
    }

    #[test]
    fn drops_unknown_hostname() {
        assert!(matches!(
            classify("svc", &[s("http://api.example.com:80")]),
            Classification::Empty
        ));
    }
}
