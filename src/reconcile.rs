//! Top-level reconcile loop: poll pangolin, transform, apply, GC.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{Context, Result};
use gateway_api::apis::experimental::httproutes::HTTPRoute;
use gateway_api::apis::experimental::listenersets::ListenerSet;
use gateway_api::apis::experimental::udproutes::UDPRoute;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::{Api, Resource};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::apply::ssa_apply;
use crate::config::{BackendKind, Config, ReconcileKind, ReconcileScope};
use crate::envoy_gateway::{Backend as EnvoyBackend, SecurityPolicy};
use crate::gc;
use crate::pangolin::{Client as PangClient, FetchOutcome};
use crate::transform::{Desired, build_desired};

pub async fn run_loop(
    cfg: Config,
    kube_client: kube::Client,
    pang: PangClient,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut last_etag: Option<String> = None;
    let mut last_digest: Option<String> = None;
    let mut backoff = Duration::from_secs(1);

    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }

        let outcome = pang.fetch(last_etag.as_deref()).await;
        let outcome = match outcome {
            Ok(o) => {
                backoff = Duration::from_secs(1);
                o
            }
            Err(e) => {
                error!(error = ?e, "pangolin fetch failed");
                if wait_or_shutdown(backoff, &shutdown).await {
                    return Ok(());
                }
                backoff = std::cmp::min(backoff * 2, cfg.max_backoff);
                continue;
            }
        };

        match outcome {
            FetchOutcome::NotModified => {
                info!("pangolin: 304 not modified");
            }
            FetchOutcome::Changed(changed) => {
                let crate::pangolin::client::ChangedConfig {
                    config: dyn_config,
                    etag,
                    digest,
                    raw_bytes,
                } = *changed;
                if last_digest.as_ref() == Some(&digest) {
                    info!(
                        bytes = raw_bytes,
                        "pangolin: body unchanged (matched sha256)"
                    );
                } else {
                    info!(
                        bytes = raw_bytes,
                        routers = dyn_config.http.routers.len(),
                        services = dyn_config.http.services.len(),
                        middlewares = dyn_config.http.middlewares.len(),
                        "pangolin: new configuration"
                    );
                    let desired = build_desired(&cfg, &dyn_config);
                    if let Err(e) = reconcile_once(&cfg, &kube_client, &desired).await {
                        error!(error = ?e, "reconciliation failed");
                        if wait_or_shutdown(backoff, &shutdown).await {
                            return Ok(());
                        }
                        backoff = std::cmp::min(backoff * 2, cfg.max_backoff);
                        continue;
                    }
                    last_digest = Some(digest);
                }
                last_etag = etag.or(last_etag);
            }
        }

        if wait_or_shutdown(cfg.poll_interval, &shutdown).await {
            return Ok(());
        }
    }
}

async fn reconcile_once(cfg: &Config, kube_client: &kube::Client, desired: &Desired) -> Result<()> {
    let reconcile_scope = expand_reconcile_scope(cfg, desired);
    let ns = cfg.namespace.as_str();
    let route_api: Api<HTTPRoute> = Api::namespaced(kube_client.clone(), ns);
    let ls_api: Api<ListenerSet> = Api::namespaced(kube_client.clone(), ns);
    let svc_api: Api<Service> = Api::namespaced(kube_client.clone(), ns);
    let eps_api: Api<EndpointSlice> = Api::namespaced(kube_client.clone(), ns);
    let be_api: Api<EnvoyBackend> = Api::namespaced(kube_client.clone(), ns);
    let sp_api: Api<SecurityPolicy> = Api::namespaced(kube_client.clone(), ns);
    let udp_api: Api<UDPRoute> = Api::namespaced(kube_client.clone(), ns);

    // Apply backends first so HTTPRoute backendRefs resolve immediately.
    for svc in desired.services.values() {
        ssa_apply_scoped(&svc_api, cfg, &reconcile_scope, ReconcileKind::Service, svc).await?;
    }
    for eps in desired.endpoint_slices.values() {
        ssa_apply_scoped(
            &eps_api,
            cfg,
            &reconcile_scope,
            ReconcileKind::EndpointSlice,
            eps,
        )
        .await?;
    }
    for be in desired.envoy_backends.values() {
        ssa_apply_scoped(&be_api, cfg, &reconcile_scope, ReconcileKind::Backend, be).await?;
    }
    // Then listener set so the parent for routes exists.
    for ls in desired.listener_sets.values() {
        ssa_apply_scoped(
            &ls_api,
            cfg,
            &reconcile_scope,
            ReconcileKind::ListenerSet,
            ls,
        )
        .await?;
    }
    for udp in desired.udp_routes.values() {
        ssa_apply_scoped(
            &udp_api,
            cfg,
            &reconcile_scope,
            ReconcileKind::UDPRoute,
            udp,
        )
        .await?;
    }
    for route in desired.http_routes.values() {
        ssa_apply_scoped(
            &route_api,
            cfg,
            &reconcile_scope,
            ReconcileKind::HttpRoute,
            route,
        )
        .await?;
    }
    for policy in desired.security_policies.values() {
        ssa_apply_scoped(
            &sp_api,
            cfg,
            &reconcile_scope,
            ReconcileKind::SecurityPolicy,
            policy,
        )
        .await?;
    }

    // GC anything we own that's no longer wanted.
    let route_names: BTreeSet<String> = desired.http_routes.keys().cloned().collect();
    let ls_names: BTreeSet<String> = desired.listener_sets.keys().cloned().collect();
    let svc_names: BTreeSet<String> = desired.services.keys().cloned().collect();
    let eps_names: BTreeSet<String> = desired.endpoint_slices.keys().cloned().collect();
    let be_names: BTreeSet<String> = desired.envoy_backends.keys().cloned().collect();
    let sp_names: BTreeSet<String> = desired.security_policies.keys().cloned().collect();
    let udp_names: BTreeSet<String> = desired.udp_routes.keys().cloned().collect();

    if let Err(e) = gc::sweep(&route_api, cfg, &reconcile_scope, &route_names).await {
        warn!(error = ?e, "GC HTTPRoute failed");
    }
    if let Err(e) = gc::sweep(&ls_api, cfg, &reconcile_scope, &ls_names).await {
        warn!(error = ?e, "GC ListenerSet failed");
    }
    if let Err(e) = gc::sweep(&eps_api, cfg, &reconcile_scope, &eps_names).await {
        warn!(error = ?e, "GC EndpointSlice failed");
    }
    if let Err(e) = gc::sweep(&svc_api, cfg, &reconcile_scope, &svc_names).await {
        warn!(error = ?e, "GC Service failed");
    }
    // Only sweep Envoy Backends when the controller is in that mode; otherwise
    // the Backend CRD may not be installed and the list call would 404.
    if cfg.backend_kind == BackendKind::EnvoyBackend
        && let Err(e) = gc::sweep(&be_api, cfg, &reconcile_scope, &be_names).await
    {
        warn!(error = ?e, "GC Envoy Backend failed");
    }
    if cfg.badger_ext_auth.is_some()
        && let Err(e) = gc::sweep(&sp_api, cfg, &reconcile_scope, &sp_names).await
    {
        warn!(error = ?e, "GC SecurityPolicy failed");
    }
    if cfg.gerbil_udp.is_some()
        && let Err(e) = gc::sweep(&udp_api, cfg, &reconcile_scope, &udp_names).await
    {
        warn!(error = ?e, "GC UDPRoute failed");
    }
    Ok(())
}

fn expand_reconcile_scope(cfg: &Config, desired: &Desired) -> ReconcileScope {
    if cfg.reconcile_scope.is_all() {
        return cfg.reconcile_scope.clone();
    }

    let hostname_selectors = cfg.reconcile_scope.hostname_candidates();
    if hostname_selectors.is_empty() {
        return cfg.reconcile_scope.clone();
    }

    let mut expanded = BTreeSet::new();
    let mut matched_any_hostname = false;
    for (route_name, route) in &desired.http_routes {
        if !route_matches_hostname(route, &hostname_selectors) {
            continue;
        }
        matched_any_hostname = true;
        expanded.insert((ReconcileKind::HttpRoute, route_name.clone()));
        expand_route_dependencies(route_name, route, desired, &mut expanded);
    }

    if matched_any_hostname {
        expanded.extend(
            desired
                .listener_sets
                .keys()
                .cloned()
                .map(|name| (ReconcileKind::ListenerSet, name)),
        );
    } else {
        warn!(
            selectors = ?hostname_selectors,
            "CONFIG_RECONCILE_ONLY hostname selectors did not match any desired HTTPRoute"
        );
    }

    cfg.reconcile_scope.with_expanded_objects(expanded)
}

fn route_matches_hostname(route: &HTTPRoute, hostname_selectors: &[String]) -> bool {
    let Some(hostnames) = &route.spec.hostnames else {
        return false;
    };
    hostnames.iter().any(|route_hostname| {
        hostname_selectors
            .iter()
            .any(|selector| hostname_matches(selector, route_hostname))
    })
}

fn hostname_matches(selector: &str, route_hostname: &str) -> bool {
    let selector = normalize_hostname(selector);
    let route_hostname = normalize_hostname(route_hostname);
    selector == route_hostname
        || route_hostname.strip_prefix("*.").is_some_and(|suffix| {
            selector.len() > suffix.len()
                && selector.ends_with(suffix)
                && selector.as_bytes()[selector.len() - suffix.len() - 1] == b'.'
        })
}

fn normalize_hostname(hostname: &str) -> String {
    hostname.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn expand_route_dependencies(
    route_name: &str,
    route: &HTTPRoute,
    desired: &Desired,
    expanded: &mut BTreeSet<(ReconcileKind, String)>,
) {
    for policy_name in security_policies_for_route(route_name, desired) {
        expanded.insert((ReconcileKind::SecurityPolicy, policy_name));
    }

    let Some(rules) = &route.spec.rules else {
        return;
    };
    for rule in rules {
        let Some(backend_refs) = &rule.backend_refs else {
            continue;
        };
        for backend_ref in backend_refs {
            let name = backend_ref.name.clone();
            let kind = backend_ref.kind.as_deref().unwrap_or("Service");
            let group = backend_ref.group.as_deref().unwrap_or("");
            if group == "gateway.envoyproxy.io"
                && kind == "Backend"
                && desired.envoy_backends.contains_key(&name)
            {
                expanded.insert((ReconcileKind::Backend, name));
            } else if group.is_empty() && kind == "Service" && desired.services.contains_key(&name)
            {
                expanded.insert((ReconcileKind::Service, name.clone()));
                for endpoint_slice_name in endpoint_slices_for_service(&name, desired) {
                    expanded.insert((ReconcileKind::EndpointSlice, endpoint_slice_name));
                }
            }
        }
    }
}

fn security_policies_for_route(route_name: &str, desired: &Desired) -> Vec<String> {
    desired
        .security_policies
        .iter()
        .filter_map(|(policy_name, policy)| {
            let matches = policy.spec.target_refs.as_ref().is_some_and(|target_refs| {
                target_refs.iter().any(|target_ref| {
                    target_ref.group == "gateway.networking.k8s.io"
                        && target_ref.kind == "HTTPRoute"
                        && target_ref.name == route_name
                })
            });
            matches.then(|| policy_name.clone())
        })
        .collect()
}

fn endpoint_slices_for_service(service_name: &str, desired: &Desired) -> Vec<String> {
    desired
        .endpoint_slices
        .iter()
        .filter_map(|(slice_name, slice)| {
            let matches = slice
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("kubernetes.io/service-name"))
                .is_some_and(|name| name == service_name);
            matches.then(|| slice_name.clone())
        })
        .collect()
}

async fn ssa_apply_scoped<T>(
    api: &Api<T>,
    cfg: &Config,
    scope: &ReconcileScope,
    kind: ReconcileKind,
    obj: &T,
) -> Result<()>
where
    T: Resource<DynamicType = ()>
        + Clone
        + serde::Serialize
        + serde::de::DeserializeOwned
        + std::fmt::Debug,
{
    let name = obj
        .meta()
        .name
        .as_deref()
        .context("object has no metadata.name")?;
    if !scope.includes(kind, name) {
        info!(kind = %kind.as_str(), name = %name, "reconcile scope: skipping apply");
        return Ok(());
    }
    ssa_apply(api, cfg, obj).await
}

/// Sleep for `dur`, returning true if shutdown was requested before the timer expired.
async fn wait_or_shutdown(dur: Duration, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        _ = shutdown.cancelled() => true,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use url::Url;

    use super::*;
    use crate::config::{BadgerExtAuthConfig, PangolinDashboardConfig};
    use crate::pangolin::TraefikDynamicConfig;

    fn test_config(reconcile_scope: ReconcileScope) -> Config {
        Config {
            pangolin_endpoint: Url::parse("http://pangolin.local/api/v1/traefik-config").unwrap(),
            auth_header: None,
            fetch_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_secs(30),
            max_backoff: Duration::from_secs(60),
            max_response_body_bytes: 1 << 20,
            tls_skip_verify: false,
            ca_file: None,
            namespace: "gateway".into(),
            parent_gateway: "eg".into(),
            parent_gateway_namespace: Some("gateway".into()),
            listener_set_name: "pangolin".into(),
            http_port: 80,
            https_port: 443,
            enable_https_listeners: true,
            backend_kind: BackendKind::Service,
            tls_secret_template: Some("{hostname-dashed}-tls".into()),
            tls_secret_namespace: None,
            field_manager: "pangolin-gateway-controller".into(),
            managed_label_key: "app.kubernetes.io/managed-by".into(),
            managed_label_value: "pangolin-gateway-controller".into(),
            instance_label_key: "pangolin.envisia.de/instance".into(),
            instance_label_value: "default".into(),
            managed_annotation_key: "pangolin.envisia.de/source".into(),
            managed_annotation_value: "pangolin-gateway-controller".into(),
            httproute_annotations: BTreeMap::new(),
            listenerset_annotations: BTreeMap::new(),
            badger_ext_auth: Some(BadgerExtAuthConfig {
                backend_name: "badger-shim".into(),
                backend_namespace: Some("gateway".into()),
                backend_port: 9002,
                path: None,
                headers_to_ext_auth: vec!["cookie".into()],
                headers_to_backend: vec!["remote-user".into()],
                fail_open: false,
            }),
            pangolin_dashboard: None::<PangolinDashboardConfig>,
            gerbil_udp: None,
            reconcile_scope,
            read_only: false,
            log_traefik_config: false,
        }
    }

    #[test]
    fn hostname_scope_expands_to_route_policy_backend_and_listener_set() {
        let cfg = test_config(ReconcileScope::parse("protected.example.com").unwrap());
        let dyn_config: TraefikDynamicConfig = serde_json::from_value(json!({
            "http": {
                "routers": {
                    "protected": {
                        "rule": "Host(`protected.example.com`)",
                        "service": "protected-service",
                        "middlewares": ["badger"]
                    },
                    "other": {
                        "rule": "Host(`other.example.com`)",
                        "service": "other-service"
                    }
                },
                "services": {
                    "protected-service": {
                        "loadBalancer": { "servers": [{"url": "http://10.0.0.7:8080"}] }
                    },
                    "other-service": {
                        "loadBalancer": { "servers": [{"url": "http://10.0.0.8:8080"}] }
                    }
                },
                "middlewares": {
                    "badger": { "plugin": { "badger": { "disableForwardAuth": true } } }
                }
            }
        }))
        .unwrap();

        let desired = build_desired(&cfg, &dyn_config);
        let scope = expand_reconcile_scope(&cfg, &desired);

        assert!(scope.includes(ReconcileKind::HttpRoute, "hr-protected"));
        assert!(!scope.includes(ReconcileKind::HttpRoute, "hr-other"));
        assert!(scope.includes(ReconcileKind::SecurityPolicy, "sp-protected"));
        assert!(scope.includes(ReconcileKind::Service, "svc-protected-service"));
        assert!(!scope.includes(ReconcileKind::Service, "svc-other-service"));
        assert!(scope.includes(ReconcileKind::EndpointSlice, "eps-protected-service"));
        assert!(scope.includes(ReconcileKind::ListenerSet, "pangolin"));
    }
}
