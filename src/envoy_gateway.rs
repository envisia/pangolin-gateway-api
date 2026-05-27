//! Hand-rolled Rust bindings for the subset of Envoy Gateway's API we use.
//!
//! Envoy Gateway publishes its CRDs at `gateway.envoyproxy.io/v1alpha1`. The
//! `gateway-api` crate doesn't include these (it tracks upstream Gateway API
//! only), so we define just enough of the `Backend` and `SecurityPolicy` CRDs
//! for the controller features we emit.
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

/// `gateway.envoyproxy.io/v1alpha1/SecurityPolicy` — used to attach Envoy
/// external authorization to HTTPRoutes that carry pangolin's badger middleware.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
#[kube(
    group = "gateway.envoyproxy.io",
    version = "v1alpha1",
    kind = "SecurityPolicy",
    plural = "securitypolicies",
    namespaced
)]
pub struct SecurityPolicySpec {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "targetRefs"
    )]
    pub target_refs: Option<Vec<SecurityPolicyTargetRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "extAuth")]
    pub ext_auth: Option<SecurityPolicyExtAuth>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
pub struct SecurityPolicyTargetRef {
    pub group: String,
    pub kind: String,
    pub name: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "sectionName"
    )]
    pub section_name: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
pub struct SecurityPolicyExtAuth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpExtAuthService>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "headersToExtAuth"
    )]
    pub headers_to_ext_auth: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "failOpen")]
    pub fail_open: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
pub struct HttpExtAuthService {
    #[serde(rename = "backendRefs")]
    pub backend_refs: Vec<SecurityPolicyBackendRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "headersToBackend"
    )]
    pub headers_to_backend: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
pub struct SecurityPolicyBackendRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub port: i32,
}
