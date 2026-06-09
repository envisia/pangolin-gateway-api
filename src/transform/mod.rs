//! Transform pangolin's Traefik dynamic config into a desired set of Gateway API objects.

pub mod backend;
pub mod l4;
pub mod listener;
pub mod middleware;
pub mod naming;
pub mod route;
pub mod rule;

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use tracing::warn;

use crate::config::Config;
use crate::envoy_gateway::Backend;
use crate::pangolin::TraefikDynamicConfig;
use crate::pangolin::types::L4Config;
use crate::transform::l4::L4Protocol;
use gateway_api::apis::experimental::httproutes::HTTPRoute;
use gateway_api::apis::experimental::listenersets::{ListenerSet, ListenerSetListeners};
use gateway_api::apis::experimental::tcproutes::TCPRoute;
use gateway_api::apis::experimental::udproutes::UDPRoute;

/// Everything the controller wants to exist in the cluster, keyed by resource name.
/// Names are guaranteed DNS-label safe.
#[derive(Debug, Default)]
pub struct Desired {
    pub http_routes: BTreeMap<String, HTTPRoute>,
    pub listener_sets: BTreeMap<String, ListenerSet>,
    pub services: BTreeMap<String, Service>,
    pub endpoint_slices: BTreeMap<String, EndpointSlice>,
    /// Populated only when `CONFIG_BACKEND_KIND=envoy-backend`. Empty otherwise.
    pub envoy_backends: BTreeMap<String, Backend>,
    /// Populated only when `CONFIG_ENABLE_TCP_ROUTES=true`. Empty otherwise.
    pub tcp_routes: BTreeMap<String, TCPRoute>,
    /// Populated only when `CONFIG_ENABLE_UDP_ROUTES=true`. Empty otherwise.
    pub udp_routes: BTreeMap<String, UDPRoute>,
}

pub fn build_desired(cfg: &Config, dyn_config: &TraefikDynamicConfig) -> Desired {
    let mut desired = Desired::default();

    // 1. Backends: pangolin services -> Service + EndpointSlice (or Envoy Backend CRDs)
    let backend_index = backend::build_backends(cfg, &dyn_config.http.services, &mut desired);

    // 2. Routes: pangolin routers -> HTTPRoute
    let route_index = route::build_routes(cfg, dyn_config, &backend_index, &mut desired);

    // 3. Raw TCP/UDP resources -> TCPRoute/UDPRoute plus extra L4 listeners
    let mut l4_listeners: Vec<ListenerSetListeners> = Vec::new();
    l4_listeners.extend(build_l4_family(
        cfg,
        dyn_config.tcp.as_ref(),
        L4Protocol::Tcp,
        cfg.enable_tcp_routes,
        "CONFIG_ENABLE_TCP_ROUTES",
        &mut desired,
    ));
    l4_listeners.extend(build_l4_family(
        cfg,
        dyn_config.udp.as_ref(),
        L4Protocol::Udp,
        cfg.enable_udp_routes,
        "CONFIG_ENABLE_UDP_ROUTES",
        &mut desired,
    ));

    // 4. Listeners: aggregate unique hostnames (+ L4 ports) into one ListenerSet
    listener::build_listener_set(cfg, &route_index, l4_listeners, &mut desired);

    desired
}

fn build_l4_family(
    cfg: &Config,
    block: Option<&L4Config>,
    proto: L4Protocol,
    enabled: bool,
    flag: &str,
    desired: &mut Desired,
) -> Vec<ListenerSetListeners> {
    let Some(block) = block else {
        return Vec::new();
    };
    if !enabled {
        if !block.routers.is_empty() {
            warn!(
                protocol = proto.k8s_protocol(),
                routers = block.routers.len(),
                "pangolin response contains L4 routers but {flag} is not set; skipping them"
            );
        }
        return Vec::new();
    }
    l4::build_l4(cfg, block, proto, desired)
}
