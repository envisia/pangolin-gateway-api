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
    /// TLS origination towards the backend. Setting this alone makes Envoy
    /// speak TLS to the endpoints (see EG's backend-skip-tls-verification
    /// task); a BackendTLSPolicy targeting this Backend merges on top.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<BackendTlsSettings>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BackendTlsSettings {
    /// `"System"` — validate against the proxy container's system CA pool.
    /// NB: explicit rename — serde's camelCase would produce
    /// `wellKnownCaCertificates`, which the CRD (and SSA) rejects.
    #[serde(
        default,
        rename = "wellKnownCACertificates",
        skip_serializing_if = "Option::is_none"
    )]
    pub well_known_ca_certificates: Option<String>,
    /// Disable certificate validation entirely. Mirrors pangolin's
    /// `serversTransports[].insecureSkipVerify`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insecure_skip_verify: Option<bool>,
    /// SNI + SAN match for verification. Must be a DNS hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sni: Option<String>,
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

/// `gateway.envoyproxy.io/v1alpha1/SecurityPolicy` — only the `extAuth.http`
/// surface we emit for pangolin's badger-protected routers. The external auth
/// service is expected to translate Envoy's check request into pangolin's
/// badger session verification (and reply with a redirect to the auth portal
/// when the session is missing/invalid).
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
#[kube(
    group = "gateway.envoyproxy.io",
    version = "v1alpha1",
    kind = "SecurityPolicy",
    plural = "securitypolicies",
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct SecurityPolicySpec {
    /// One entry per HTTPRoute the policy protects. We emit exactly one.
    pub target_refs: Vec<PolicyTargetRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ext_auth: Option<ExtAuth>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
pub struct PolicyTargetRef {
    pub group: String,
    pub kind: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtAuth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpExtAuthService>,
    /// Client request headers forwarded to the auth service in addition to
    /// Envoy's defaults (we always include the session cookie).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers_to_ext_auth: Option<Vec<String>>,
    /// `false` (our hardcoded value): an unreachable auth service denies
    /// requests rather than letting them through unauthenticated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_open: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HttpExtAuthService {
    pub backend_refs: Vec<ExtAuthBackendRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Auth-service response headers copied onto the upstream request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers_to_backend: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
pub struct ExtAuthBackendRef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CRDs use acronym casing serde's `camelCase` can't derive — pin the
    /// wire format so SSA against the real schemas keeps working.
    #[test]
    fn tls_settings_serialize_with_crd_field_names() {
        let verified = BackendTlsSettings {
            well_known_ca_certificates: Some("System".into()),
            sni: Some("example.com".into()),
            ..Default::default()
        };
        let json = serde_json::to_value(&verified).unwrap();
        assert!(json.get("wellKnownCACertificates").is_some(), "{json}");
        assert!(json.get("sni").is_some());
        assert!(json.get("insecureSkipVerify").is_none());

        let skip = BackendTlsSettings {
            insecure_skip_verify: Some(true),
            ..Default::default()
        };
        let json = serde_json::to_value(&skip).unwrap();
        assert!(json.get("insecureSkipVerify").is_some(), "{json}");
        assert!(json.get("wellKnownCACertificates").is_none());
    }

    #[test]
    fn security_policy_spec_serializes_with_crd_field_names() {
        let spec = SecurityPolicySpec {
            target_refs: vec![PolicyTargetRef {
                group: "gateway.networking.k8s.io".into(),
                kind: "HTTPRoute".into(),
                name: "r".into(),
            }],
            ext_auth: Some(ExtAuth {
                http: Some(HttpExtAuthService {
                    backend_refs: vec![ExtAuthBackendRef {
                        name: "shim".into(),
                        namespace: None,
                        port: Some(9001),
                    }],
                    path: None,
                    headers_to_backend: None,
                }),
                headers_to_ext_auth: Some(vec!["cookie".into()]),
                fail_open: Some(false),
            }),
        };
        let json = serde_json::to_value(&spec).unwrap();
        assert!(json.get("targetRefs").is_some(), "{json}");
        let ext = json.get("extAuth").expect("extAuth");
        assert!(ext.get("headersToExtAuth").is_some(), "{ext}");
        assert!(ext.get("failOpen").is_some());
        assert!(ext.get("http").and_then(|h| h.get("backendRefs")).is_some());
    }
}
