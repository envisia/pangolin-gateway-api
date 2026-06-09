//! Translate pangolin's `tcp`/`udp` blocks ("raw" resources) into Gateway API
//! TCPRoute/UDPRoute objects plus the TCP/UDP listeners they attach to.
//!
//! Pangolin encodes the public port in the entrypoint name (`tcp-234`,
//! `udp-345`) and only ever emits `HostSNI(`*`)` as a TCP rule — anything more
//! specific would need TLSRoute semantics, which we don't support yet, so such
//! routers are warned about and skipped (never silently misrouted).

use std::collections::BTreeMap;

use gateway_api::apis::experimental::listenersets::ListenerSetListeners;
use gateway_api::apis::experimental::tcproutes::{
    TCPRoute, TcpRouteParentRefs, TcpRouteRules, TcpRouteRulesBackendRefs, TcpRouteSpec,
};
use gateway_api::apis::experimental::udproutes::{
    UDPRoute, UdpRouteParentRefs, UdpRouteRules, UdpRouteRulesBackendRefs, UdpRouteSpec,
};
use tracing::warn;

use crate::apply::{managed_metadata, owner_labels};
use crate::config::Config;
use crate::pangolin::types::{L4Config, L4Router};
use crate::transform::Desired;
use crate::transform::backend::{self, ResolvedBackend};
use crate::transform::naming::{dns_label, prefixed_label};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum L4Protocol {
    Tcp,
    Udp,
}

impl L4Protocol {
    /// Gateway API listener protocol / Kubernetes port protocol.
    pub fn k8s_protocol(self) -> &'static str {
        match self {
            L4Protocol::Tcp => "TCP",
            L4Protocol::Udp => "UDP",
        }
    }

    /// Lowercase tag used in entrypoint names and synthesized object names.
    pub fn infix(self) -> &'static str {
        match self {
            L4Protocol::Tcp => "tcp",
            L4Protocol::Udp => "udp",
        }
    }
}

/// Build routes + backends for one protocol family and return the listeners the
/// ListenerSet builder should append. Listener names are `tcp-<port>`/`udp-<port>`,
/// which the routes reference via `parentRef.sectionName`.
pub fn build_l4(
    cfg: &Config,
    l4: &L4Config,
    proto: L4Protocol,
    desired: &mut Desired,
) -> Vec<ListenerSetListeners> {
    let backends = backend::build_l4_backends(cfg, &l4.services, proto, desired);

    let mut listeners: Vec<ListenerSetListeners> = Vec::new();
    // port -> router that claimed it; Gateway API allows only one listener per
    // (port, protocol) pair, and overlapping L4 routes would be ambiguous anyway.
    let mut claimed_ports: BTreeMap<i32, String> = BTreeMap::new();

    for (router_name, router) in &l4.routers {
        if !rule_is_supported(router_name, router, proto) {
            continue;
        }

        let Some(backend) = backends.get(&router.service) else {
            warn!(
                router = %router_name,
                service = %router.service,
                "L4 router points to unknown or unresolvable service; skipping"
            );
            continue;
        };

        let mut section_names: Vec<String> = Vec::new();
        for ep in &router.entry_points {
            let Some(port) = parse_entrypoint_port(ep, proto) else {
                warn!(
                    router = %router_name,
                    entrypoint = %ep,
                    expected = %format!("{}-<port>", proto.infix()),
                    "L4 entrypoint name does not encode a port; skipping entrypoint"
                );
                continue;
            };
            if proto == L4Protocol::Tcp && (port == cfg.http_port || port == cfg.https_port) {
                warn!(
                    router = %router_name,
                    port,
                    "TCP entrypoint port collides with the HTTP/HTTPS listener port; skipping entrypoint"
                );
                continue;
            }
            if let Some(owner) = claimed_ports.get(&port) {
                warn!(
                    router = %router_name,
                    port,
                    owner = %owner,
                    "L4 port already claimed by another router; skipping entrypoint"
                );
                continue;
            }
            claimed_ports.insert(port, router_name.clone());

            let listener_name = dns_label(&format!("{}-{port}", proto.infix()));
            listeners.push(ListenerSetListeners {
                name: listener_name.clone(),
                hostname: None,
                port,
                protocol: proto.k8s_protocol().into(),
                tls: None,
                allowed_routes: None,
            });
            section_names.push(listener_name);
        }

        if section_names.is_empty() {
            warn!(
                router = %router_name,
                "L4 router has no usable entrypoint; skipping"
            );
            continue;
        }

        let route_name = prefixed_label(proto.infix(), router_name);
        match proto {
            L4Protocol::Tcp => {
                let route = build_tcp_route(cfg, &route_name, &section_names, backend);
                desired.tcp_routes.insert(route_name, route);
            }
            L4Protocol::Udp => {
                let route = build_udp_route(cfg, &route_name, &section_names, backend);
                desired.udp_routes.insert(route_name, route);
            }
        }
    }

    listeners
}

/// TCP routers must carry `HostSNI(`*`)` (or nothing); a concrete SNI or TLS
/// options would need TLSRoute passthrough semantics. UDP routers never have rules.
fn rule_is_supported(router_name: &str, router: &L4Router, proto: L4Protocol) -> bool {
    if router.tls.is_some() {
        warn!(
            router = %router_name,
            "L4 router sets TLS options (passthrough); TLSRoute is not supported yet; skipping"
        );
        return false;
    }
    match router.rule.as_deref().map(str::trim) {
        None | Some("") => true,
        Some(rule) if proto == L4Protocol::Tcp && rule == "HostSNI(`*`)" => true,
        Some(rule) => {
            warn!(
                router = %router_name,
                rule = %rule,
                "L4 router rule cannot be translated to Gateway API; skipping"
            );
            false
        }
    }
}

/// `tcp-234` -> 234 (for the matching protocol only).
fn parse_entrypoint_port(entrypoint: &str, proto: L4Protocol) -> Option<i32> {
    let rest = entrypoint.strip_prefix(proto.infix())?.strip_prefix('-')?;
    let port: u16 = rest.parse().ok()?;
    (port != 0).then_some(i32::from(port))
}

fn parent_refs_tcp(cfg: &Config, section_names: &[String]) -> Vec<TcpRouteParentRefs> {
    section_names
        .iter()
        .map(|section| TcpRouteParentRefs {
            group: Some("gateway.networking.k8s.io".into()),
            kind: Some("ListenerSet".into()),
            name: cfg.listener_set_name.clone(),
            namespace: Some(cfg.namespace.clone()),
            section_name: Some(section.clone()),
            ..Default::default()
        })
        .collect()
}

fn build_tcp_route(
    cfg: &Config,
    route_name: &str,
    section_names: &[String],
    backend: &ResolvedBackend,
) -> TCPRoute {
    let labels = owner_labels(cfg, route_name);
    TCPRoute {
        metadata: managed_metadata(cfg, route_name, labels),
        spec: TcpRouteSpec {
            parent_refs: Some(parent_refs_tcp(cfg, section_names)),
            rules: vec![TcpRouteRules {
                backend_refs: vec![TcpRouteRulesBackendRefs {
                    group: Some(backend.group.clone()),
                    kind: Some(backend.kind.clone()),
                    name: backend.name.clone(),
                    namespace: backend.namespace.clone(),
                    port: Some(backend.port),
                    weight: Some(1),
                }],
                name: None,
            }],
            ..Default::default()
        },
        status: None,
    }
}

fn build_udp_route(
    cfg: &Config,
    route_name: &str,
    section_names: &[String],
    backend: &ResolvedBackend,
) -> UDPRoute {
    let labels = owner_labels(cfg, route_name);
    let parent_refs = section_names
        .iter()
        .map(|section| UdpRouteParentRefs {
            group: Some("gateway.networking.k8s.io".into()),
            kind: Some("ListenerSet".into()),
            name: cfg.listener_set_name.clone(),
            namespace: Some(cfg.namespace.clone()),
            section_name: Some(section.clone()),
            ..Default::default()
        })
        .collect();
    UDPRoute {
        metadata: managed_metadata(cfg, route_name, labels),
        spec: UdpRouteSpec {
            parent_refs: Some(parent_refs),
            rules: vec![UdpRouteRules {
                backend_refs: vec![UdpRouteRulesBackendRefs {
                    group: Some(backend.group.clone()),
                    kind: Some(backend.kind.clone()),
                    name: backend.name.clone(),
                    namespace: backend.namespace.clone(),
                    port: Some(backend.port),
                    weight: Some(1),
                }],
                name: None,
            }],
            ..Default::default()
        },
        status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entrypoint_port_parses_matching_protocol() {
        assert_eq!(parse_entrypoint_port("tcp-234", L4Protocol::Tcp), Some(234));
        assert_eq!(parse_entrypoint_port("udp-345", L4Protocol::Udp), Some(345));
    }

    #[test]
    fn entrypoint_port_rejects_mismatches() {
        assert_eq!(parse_entrypoint_port("udp-345", L4Protocol::Tcp), None);
        assert_eq!(parse_entrypoint_port("web", L4Protocol::Tcp), None);
        assert_eq!(parse_entrypoint_port("tcp-", L4Protocol::Tcp), None);
        assert_eq!(parse_entrypoint_port("tcp-0", L4Protocol::Tcp), None);
        assert_eq!(parse_entrypoint_port("tcp-70000", L4Protocol::Tcp), None);
    }

    #[test]
    fn hostsni_wildcard_is_supported_for_tcp() {
        let router = L4Router {
            rule: Some("HostSNI(`*`)".into()),
            ..Default::default()
        };
        assert!(rule_is_supported("r", &router, L4Protocol::Tcp));
    }

    #[test]
    fn concrete_sni_is_rejected() {
        let router = L4Router {
            rule: Some("HostSNI(`db.example.com`)".into()),
            ..Default::default()
        };
        assert!(!rule_is_supported("r", &router, L4Protocol::Tcp));
    }

    #[test]
    fn tls_options_are_rejected() {
        let router = L4Router {
            rule: Some("HostSNI(`*`)".into()),
            tls: Some(serde_json::json!({ "passthrough": true })),
            ..Default::default()
        };
        assert!(!rule_is_supported("r", &router, L4Protocol::Tcp));
    }
}
