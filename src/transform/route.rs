//! Build one HTTPRoute per pangolin router.

use std::collections::BTreeSet;

use gateway_api::apis::experimental::httproutes::{
    HTTPRoute, HttpRouteParentRefs, HttpRouteRules, HttpRouteRulesBackendRefs,
    HttpRouteRulesFiltersType, HttpRouteRulesMatches, HttpRouteRulesMatchesPath,
    HttpRouteRulesMatchesPathType, HttpRouteSpec,
};
use tracing::warn;

use crate::apply::{managed_metadata, managed_metadata_with, owner_labels};
use crate::config::Config;
use crate::envoy_gateway::{
    HttpExtAuthService, SecurityPolicy, SecurityPolicyBackendRef, SecurityPolicyExtAuth,
    SecurityPolicySpec, SecurityPolicyTargetRef,
};
use crate::pangolin::TraefikDynamicConfig;
use crate::transform::Desired;
use crate::transform::backend::BackendIndex;
use crate::transform::middleware;
use crate::transform::naming::prefixed_label;
use crate::transform::rule::{self, PathMatchKind};

/// Side data produced while building routes — used by the ListenerSet builder
/// to know which hostnames need listeners.
#[derive(Debug, Default)]
pub struct RouteIndex {
    pub hostnames: BTreeSet<String>,
    /// Any host -> true if at least one router on that host requested TLS.
    pub https_hosts: BTreeSet<String>,
}

pub fn build_routes(
    cfg: &Config,
    dyn_config: &TraefikDynamicConfig,
    backends: &BackendIndex,
    desired: &mut Desired,
) -> RouteIndex {
    let mut index = RouteIndex::default();

    for (router_name, router) in &dyn_config.http.routers {
        let parsed = rule::parse(&router.rule);
        if !parsed.is_usable() {
            warn!(
                router = %router_name,
                rule = %router.rule,
                has_disjunction = parsed.has_disjunction,
                has_negation = parsed.has_negation,
                host_regexp = parsed.host_regexp,
                unsupported = ?parsed.unsupported_predicates,
                "router rule cannot be translated to Gateway API; skipping"
            );
            continue;
        }

        let backend = match backends.get(&router.service) {
            Some(b) => b,
            None => {
                warn!(router = %router_name, service = %router.service, "router points to unknown service; skipping");
                continue;
            }
        };

        let uses_badger =
            middleware::references_badger(&router.middlewares, &dyn_config.http.middlewares);
        let badger_handled_by_ext_auth = cfg.badger_ext_auth.is_some() && uses_badger;
        let filters = middleware::build_filters(
            router_name,
            &router.middlewares,
            &dyn_config.http.middlewares,
            badger_handled_by_ext_auth,
        );

        let matches = parsed.path.as_ref().map(|p| {
            vec![HttpRouteRulesMatches {
                path: Some(HttpRouteRulesMatchesPath {
                    r#type: Some(match p.kind {
                        PathMatchKind::Exact => HttpRouteRulesMatchesPathType::Exact,
                        PathMatchKind::Prefix => HttpRouteRulesMatchesPathType::PathPrefix,
                        PathMatchKind::Regex => HttpRouteRulesMatchesPathType::RegularExpression,
                    }),
                    value: Some(p.value.clone()),
                }),
                ..Default::default()
            }]
        });

        // Gateway API CEL: "RequestRedirect filter must not be used together with
        // backendRefs". When the router carries a redirect middleware (e.g.
        // pangolin's `redirect-to-https`), the rule is terminating — emitting
        // backendRefs alongside would make admission reject the entire HTTPRoute.
        let is_terminating = filters
            .iter()
            .any(|f| f.r#type == HttpRouteRulesFiltersType::RequestRedirect);

        let backend_refs = (!is_terminating).then(|| {
            vec![HttpRouteRulesBackendRefs {
                name: backend.name.clone(),
                namespace: backend.namespace.clone(),
                port: Some(backend.port),
                kind: Some(backend.kind.clone()),
                // Empty group = core API. Non-empty for the Envoy Gateway Backend CRD.
                group: Some(backend.group.clone()),
                weight: Some(1),
                ..Default::default()
            }]
        });

        let rules = vec![HttpRouteRules {
            matches,
            filters: if filters.is_empty() {
                None
            } else {
                Some(filters)
            },
            backend_refs,
            ..Default::default()
        }];

        let route_name = prefixed_label("hr", router_name);
        let labels = owner_labels(cfg, &route_name);

        let parent_kind = "ListenerSet";
        let parent_group = "gateway.networking.k8s.io";

        let spec = HttpRouteSpec {
            parent_refs: Some(vec![HttpRouteParentRefs {
                group: Some(parent_group.into()),
                kind: Some(parent_kind.into()),
                name: cfg.listener_set_name.clone(),
                namespace: Some(cfg.namespace.clone()),
                ..Default::default()
            }]),
            hostnames: Some(parsed.hosts.clone()),
            rules: Some(rules),
            ..Default::default()
        };

        let route = HTTPRoute {
            metadata: managed_metadata_with(cfg, &route_name, labels, &cfg.httproute_annotations),
            spec,
            status: None,
        };

        for host in &parsed.hosts {
            index.hostnames.insert(host.clone());
            if cfg.enable_https_listeners
                && (router.tls.is_some() || router.entry_points.iter().any(|e| e == "https"))
            {
                index.https_hosts.insert(host.clone());
            }
        }

        if badger_handled_by_ext_auth && !is_terminating {
            let policy = build_badger_security_policy(cfg, router_name, &route_name);
            let policy_name = policy.metadata.name.clone().unwrap_or_default();
            desired.security_policies.insert(policy_name, policy);
        }

        desired.http_routes.insert(route_name, route);
    }

    index
}

fn build_badger_security_policy(
    cfg: &Config,
    router_name: &str,
    route_name: &str,
) -> SecurityPolicy {
    let ext_auth = cfg
        .badger_ext_auth
        .as_ref()
        .expect("badger ext auth checked by caller");
    let name = prefixed_label("sp", router_name);
    let labels = owner_labels(cfg, &name);

    SecurityPolicy {
        metadata: managed_metadata(cfg, &name, labels),
        spec: SecurityPolicySpec {
            target_refs: Some(vec![SecurityPolicyTargetRef {
                group: "gateway.networking.k8s.io".into(),
                kind: "HTTPRoute".into(),
                name: route_name.to_string(),
                section_name: None,
            }]),
            ext_auth: Some(SecurityPolicyExtAuth {
                http: Some(HttpExtAuthService {
                    backend_refs: vec![SecurityPolicyBackendRef {
                        group: Some(String::new()),
                        kind: Some("Service".into()),
                        name: ext_auth.backend_name.clone(),
                        namespace: ext_auth.backend_namespace.clone(),
                        port: ext_auth.backend_port,
                    }],
                    path: ext_auth.path.clone(),
                    headers_to_backend: Some(ext_auth.headers_to_backend.clone()),
                }),
                headers_to_ext_auth: Some(ext_auth.headers_to_ext_auth.clone()),
                fail_open: Some(ext_auth.fail_open),
            }),
        },
    }
}
