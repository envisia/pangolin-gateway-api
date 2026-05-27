//! Optional Gateway API UDPRoutes for Gerbil's WireGuard-facing ports.

use gateway_api::apis::experimental::udproutes::{
    UDPRoute, UdpRouteParentRefs, UdpRouteRules, UdpRouteRulesBackendRefs, UdpRouteSpec,
};

use crate::apply::{managed_metadata, owner_labels};
use crate::config::Config;
use crate::transform::Desired;
use crate::transform::naming::prefixed_label;

pub fn build_udp_routes(cfg: &Config, desired: &mut Desired) {
    let Some(gerbil) = cfg.gerbil_udp.as_ref() else {
        return;
    };

    for port in &gerbil.ports {
        let source_name = format!("gerbil-udp-{port}");
        let route_name = prefixed_label("udpr", &source_name);
        let labels = owner_labels(cfg, &route_name);
        let listener_name = prefixed_label("udp", &source_name);

        let route = UDPRoute {
            metadata: managed_metadata(cfg, &route_name, labels),
            spec: UdpRouteSpec {
                parent_refs: Some(vec![UdpRouteParentRefs {
                    group: Some("gateway.networking.k8s.io".into()),
                    kind: Some("ListenerSet".into()),
                    name: cfg.listener_set_name.clone(),
                    namespace: Some(cfg.namespace.clone()),
                    section_name: Some(listener_name),
                    ..Default::default()
                }]),
                rules: vec![UdpRouteRules {
                    backend_refs: vec![UdpRouteRulesBackendRefs {
                        group: Some(String::new()),
                        kind: Some("Service".into()),
                        name: gerbil.service_name.clone(),
                        namespace: gerbil.service_namespace.clone(),
                        port: Some(*port),
                        weight: Some(1),
                    }],
                    name: Some("default".into()),
                }],
                ..Default::default()
            },
            status: None,
        };

        desired.udp_routes.insert(route_name, route);
    }
}
