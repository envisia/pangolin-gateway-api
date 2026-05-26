//! Transform pangolin's Traefik dynamic config into a desired set of Gateway API objects.

pub mod backend;
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
use gateway_api::apis::experimental::httproutes::HTTPRoute;
use gateway_api::apis::experimental::listenersets::ListenerSet;

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
}

pub fn build_desired(cfg: &Config, dyn_config: &TraefikDynamicConfig) -> Desired {
    let mut desired = Desired::default();

    // 1. Backends: pangolin services -> Service + EndpointSlice (or Envoy Backend CRDs)
    let backend_index = backend::build_backends(cfg, &dyn_config.http.services, &mut desired);

    // 2. Routes: pangolin routers -> HTTPRoute
    let route_index = route::build_routes(cfg, dyn_config, &backend_index, &mut desired);

    // 3. Listeners: aggregate unique hostnames into one ListenerSet
    listener::build_listener_set(cfg, &route_index, &mut desired);

    if let (Some(_), Some(_)) = (&dyn_config.tcp, &dyn_config.udp) {
        warn!("pangolin response contains tcp/udp blocks; this controller only handles HTTP");
    }

    desired
}
