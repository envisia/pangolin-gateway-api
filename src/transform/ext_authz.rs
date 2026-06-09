//! Emit one Envoy Gateway `SecurityPolicy` per badger-protected HTTPRoute.
//!
//! Pangolin enforces resource auth (SSO, password, PIN, access rules) through
//! its `badger` Traefik plugin. Envoy can't run Traefik plugins, but badger is
//! semantically an auth check + redirect — exactly Envoy's HTTP external
//! authorization model. The operator points `CONFIG_EXT_AUTHZ_SERVICE` at a
//! service speaking that protocol (typically a small shim verifying sessions
//! against pangolin's badger API) and the controller wires every protected
//! route to it. `failOpen` is always false: if the auth service is down,
//! protected resources stay closed.

use crate::apply::{managed_metadata, owner_labels};
use crate::config::{Config, ExtAuthzConfig};
use crate::envoy_gateway::{
    ExtAuth, ExtAuthBackendRef, HttpExtAuthService, PolicyTargetRef, SecurityPolicy,
    SecurityPolicySpec,
};
use crate::transform::naming::prefixed_label;

/// Policy protecting the HTTPRoute named `route_name`. The policy name is
/// derived from the route name so route GC and policy GC stay in lockstep.
pub fn build_security_policy(
    cfg: &Config,
    ea: &ExtAuthzConfig,
    route_name: &str,
) -> SecurityPolicy {
    let name = prefixed_label("sp", route_name);
    let labels = owner_labels(cfg, &name);

    SecurityPolicy {
        metadata: managed_metadata(cfg, &name, labels),
        spec: SecurityPolicySpec {
            target_refs: vec![PolicyTargetRef {
                group: "gateway.networking.k8s.io".into(),
                kind: "HTTPRoute".into(),
                name: route_name.to_string(),
            }],
            ext_auth: Some(ExtAuth {
                http: Some(HttpExtAuthService {
                    backend_refs: vec![ExtAuthBackendRef {
                        name: ea.service.clone(),
                        namespace: ea.namespace.clone(),
                        port: Some(ea.port),
                    }],
                    path: ea.path.clone(),
                    headers_to_backend: if ea.headers_to_backend.is_empty() {
                        None
                    } else {
                        Some(ea.headers_to_backend.clone())
                    },
                }),
                headers_to_ext_auth: if ea.headers_to_ext_auth.is_empty() {
                    None
                } else {
                    Some(ea.headers_to_ext_auth.clone())
                },
                fail_open: Some(false),
            }),
        },
    }
}
