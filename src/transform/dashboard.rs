//! Optional static Gateway API routes for Pangolin's own dashboard/API.

use gateway_api::apis::experimental::httproutes::{
    HTTPRoute, HttpRouteParentRefs, HttpRouteRules, HttpRouteRulesBackendRefs,
    HttpRouteRulesFilters, HttpRouteRulesFiltersRequestRedirect,
    HttpRouteRulesFiltersRequestRedirectScheme, HttpRouteRulesFiltersType, HttpRouteRulesMatches,
    HttpRouteRulesMatchesHeaders, HttpRouteRulesMatchesHeadersType, HttpRouteRulesMatchesPath,
    HttpRouteRulesMatchesPathType, HttpRouteSpec,
};

use crate::apply::{managed_metadata_with, owner_labels};
use crate::config::{Config, PangolinDashboardConfig};
use crate::transform::Desired;
use crate::transform::naming::{dns_label, prefixed_label};
use crate::transform::route::RouteIndex;

pub fn build_dashboard_routes(cfg: &Config, routes: &mut RouteIndex, desired: &mut Desired) {
    let Some(dashboard) = cfg.pangolin_dashboard.as_ref() else {
        return;
    };

    let tls_enabled = cfg.enable_https_listeners && cfg.tls_secret_template.is_some();
    routes.hostnames.insert(dashboard.hostname.clone());
    if tls_enabled {
        routes.https_hosts.insert(dashboard.hostname.clone());
    }

    if dashboard.redirect_http_to_https && tls_enabled {
        insert_route(
            cfg,
            desired,
            "pangolin-dashboard-redirect",
            dashboard,
            Some(vec![HttpRouteRulesFilters {
                r#type: HttpRouteRulesFiltersType::RequestRedirect,
                request_redirect: Some(HttpRouteRulesFiltersRequestRedirect {
                    scheme: Some(HttpRouteRulesFiltersRequestRedirectScheme::Https),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            None,
            None,
            Some(dns_label(&format!("http-{}", dashboard.hostname))),
        );
    }

    let backend_section = if tls_enabled {
        Some(dns_label(&format!("https-{}", dashboard.hostname)))
    } else {
        Some(dns_label(&format!("http-{}", dashboard.hostname)))
    };

    insert_route(
        cfg,
        desired,
        "pangolin-dashboard-ws",
        dashboard,
        None,
        Some(path_and_header_match("/api/v1/ws", "upgrade", "websocket")),
        Some(dashboard.api_port),
        backend_section.clone(),
    );

    insert_route(
        cfg,
        desired,
        "pangolin-dashboard-api",
        dashboard,
        None,
        Some(path_match("/api/v1")),
        Some(dashboard.api_port),
        backend_section.clone(),
    );

    insert_route(
        cfg,
        desired,
        "pangolin-dashboard-web",
        dashboard,
        None,
        Some(path_match("/")),
        Some(dashboard.next_port),
        backend_section,
    );
}

fn insert_route(
    cfg: &Config,
    desired: &mut Desired,
    source_name: &str,
    dashboard: &PangolinDashboardConfig,
    filters: Option<Vec<HttpRouteRulesFilters>>,
    matches: Option<Vec<HttpRouteRulesMatches>>,
    backend_port: Option<i32>,
    parent_section: Option<String>,
) {
    let route_name = prefixed_label("hr", source_name);
    let labels = owner_labels(cfg, &route_name);

    let backend_refs = backend_port.map(|port| {
        vec![HttpRouteRulesBackendRefs {
            group: Some(String::new()),
            kind: Some("Service".into()),
            name: dashboard.service_name.clone(),
            namespace: dashboard.service_namespace.clone(),
            port: Some(port),
            weight: Some(1),
            ..Default::default()
        }]
    });

    let route = HTTPRoute {
        metadata: managed_metadata_with(cfg, &route_name, labels, &cfg.httproute_annotations),
        spec: HttpRouteSpec {
            parent_refs: Some(vec![HttpRouteParentRefs {
                group: Some("gateway.networking.k8s.io".into()),
                kind: Some("ListenerSet".into()),
                name: cfg.listener_set_name.clone(),
                namespace: Some(cfg.namespace.clone()),
                section_name: parent_section,
                ..Default::default()
            }]),
            hostnames: Some(vec![dashboard.hostname.clone()]),
            rules: Some(vec![HttpRouteRules {
                matches,
                filters,
                backend_refs,
                ..Default::default()
            }]),
            ..Default::default()
        },
        status: None,
    };

    desired.http_routes.insert(route_name, route);
}

fn path_match(path: &str) -> Vec<HttpRouteRulesMatches> {
    vec![HttpRouteRulesMatches {
        path: Some(HttpRouteRulesMatchesPath {
            r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
            value: Some(path.to_string()),
        }),
        ..Default::default()
    }]
}

fn path_and_header_match(path: &str, header: &str, value: &str) -> Vec<HttpRouteRulesMatches> {
    vec![HttpRouteRulesMatches {
        path: Some(HttpRouteRulesMatchesPath {
            r#type: Some(HttpRouteRulesMatchesPathType::PathPrefix),
            value: Some(path.to_string()),
        }),
        headers: Some(vec![HttpRouteRulesMatchesHeaders {
            name: header.to_string(),
            r#type: Some(HttpRouteRulesMatchesHeadersType::Exact),
            value: value.to_string(),
        }]),
        ..Default::default()
    }]
}
