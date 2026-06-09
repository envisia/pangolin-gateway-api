//! Build one HTTPRoute per pangolin router.

use std::collections::BTreeSet;

use gateway_api::apis::experimental::httproutes::{
    HTTPRoute, HttpRouteParentRefs, HttpRouteRules, HttpRouteRulesBackendRefs,
    HttpRouteRulesFiltersType, HttpRouteRulesMatches, HttpRouteRulesMatchesPath,
    HttpRouteRulesMatchesPathType, HttpRouteSpec,
};
use tracing::warn;

use crate::apply::{managed_metadata_with, owner_labels};
use crate::config::Config;
use crate::pangolin::TraefikDynamicConfig;
use crate::transform::Desired;
use crate::transform::backend::BackendIndex;
use crate::transform::ext_authz;
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

        // Pangolin marks auth-enforced resources by attaching its `badger`
        // plugin middleware. Envoy can't run that plugin, so the route must be
        // covered by an ext-authz SecurityPolicy — or explicitly allowed to go
        // out unauthenticated — or it is skipped. Emitting it silently would
        // expose SSO/password/PIN-protected resources to the world.
        let protected =
            middleware::requires_badger_auth(&router.middlewares, &dyn_config.http.middlewares);
        if protected && cfg.ext_authz.is_none() {
            if cfg.allow_unauthenticated_routes {
                warn!(
                    router = %router_name,
                    "auth-protected router emitted WITHOUT authentication \
                     (CONFIG_ALLOW_UNAUTHENTICATED_ROUTES=true)"
                );
            } else {
                warn!(
                    router = %router_name,
                    "router is protected by pangolin auth (badger) but no ext-authz service is \
                     configured; skipping. Set CONFIG_EXT_AUTHZ_SERVICE to wire it to an \
                     external authorization service, or CONFIG_ALLOW_UNAUTHENTICATED_ROUTES=true \
                     to expose it without auth"
                );
                continue;
            }
        }

        let backend = match backends.get(&router.service) {
            Some(b) => b,
            None => {
                warn!(router = %router_name, service = %router.service, "router points to unknown service; skipping");
                continue;
            }
        };

        let filters = middleware::build_filters(
            router_name,
            &router.middlewares,
            &dyn_config.http.middlewares,
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

        if protected && let Some(ea) = &cfg.ext_authz {
            let sp = ext_authz::build_security_policy(cfg, ea, &route_name);
            let sp_name = sp.metadata.name.clone().expect("policy has a name");
            desired.security_policies.insert(sp_name, sp);
        }

        desired.http_routes.insert(route_name, route);
    }

    index
}
