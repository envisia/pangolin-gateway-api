//! Hand-rolled Rust bindings for the subset of Envoy Gateway's API we use.
//!
//! Envoy Gateway publishes its CRDs at `gateway.envoyproxy.io/v1alpha1`. The
//! `gateway-api` crate doesn't include these (it tracks upstream Gateway API
//! only), so we define just enough of the `Backend` CRD to emit one when the
//! controller is configured for the `envoy-backend` strategy.
//!
//! Reference: <https://gateway.envoyproxy.io/docs/api/extension_types/>.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `gateway.envoyproxy.io/v1alpha1/Backend` — direct IP/FQDN endpoints addressable
/// from an HTTPRoute via `backendRefs.group=gateway.envoyproxy.io, kind=Backend`.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
#[kube(
    group = "gateway.envoyproxy.io",
    version = "v1alpha1",
    kind = "Backend",
    plural = "backends",
    namespaced
)]
pub struct BackendSpec {
    /// Endpoints carries one entry per target. Each entry sets exactly one of
    /// `ip` / `fqdn` / `unix`. We never emit `unix`.
    pub endpoints: Vec<BackendEndpoint>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
pub struct BackendEndpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<BackendIp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fqdn: Option<BackendFqdn>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
pub struct BackendIp {
    pub address: String,
    pub port: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
pub struct BackendFqdn {
    pub hostname: String,
    pub port: i32,
}
