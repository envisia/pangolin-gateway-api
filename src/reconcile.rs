//! Top-level reconcile loop: poll pangolin, transform, apply, GC.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::Result;
use gateway_api::apis::experimental::httproutes::HTTPRoute;
use gateway_api::apis::experimental::listenersets::ListenerSet;
use gateway_api::apis::experimental::tcproutes::TCPRoute;
use gateway_api::apis::experimental::udproutes::UDPRoute;
use gateway_api::apis::standard::backendtlspolicies::BackendTLSPolicy;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::Api;
use kube::api::ListParams;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::apply::ssa_apply;
use crate::config::{BackendKind, Config};
use crate::envoy_gateway::{Backend as EnvoyBackend, SecurityPolicy};
use crate::gc;
use crate::health::Readiness;
use crate::pangolin::{Client as PangClient, FetchOutcome};
use crate::transform::{Desired, build_desired};

pub async fn run_loop(
    cfg: Config,
    kube_client: kube::Client,
    pang: PangClient,
    shutdown: CancellationToken,
    ready: Readiness,
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
                ready.set_ready();
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
                        tcp_routers = dyn_config.tcp.as_ref().map_or(0, |c| c.routers.len()),
                        udp_routers = dyn_config.udp.as_ref().map_or(0, |c| c.routers.len()),
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
                ready.set_ready();
                last_etag = etag.or(last_etag);
            }
        }

        if wait_or_shutdown(cfg.poll_interval, &shutdown).await {
            return Ok(());
        }
    }
}

async fn reconcile_once(cfg: &Config, kube_client: &kube::Client, desired: &Desired) -> Result<()> {
    let ns = cfg.namespace.as_str();
    let route_api: Api<HTTPRoute> = Api::namespaced(kube_client.clone(), ns);
    let ls_api: Api<ListenerSet> = Api::namespaced(kube_client.clone(), ns);
    let svc_api: Api<Service> = Api::namespaced(kube_client.clone(), ns);
    let eps_api: Api<EndpointSlice> = Api::namespaced(kube_client.clone(), ns);
    let be_api: Api<EnvoyBackend> = Api::namespaced(kube_client.clone(), ns);
    let tcp_api: Api<TCPRoute> = Api::namespaced(kube_client.clone(), ns);
    let udp_api: Api<UDPRoute> = Api::namespaced(kube_client.clone(), ns);
    let sp_api: Api<SecurityPolicy> = Api::namespaced(kube_client.clone(), ns);
    let btp_api: Api<BackendTLSPolicy> = Api::namespaced(kube_client.clone(), ns);

    // Apply backends first so HTTPRoute backendRefs resolve immediately.
    for svc in desired.services.values() {
        ssa_apply(&svc_api, cfg, svc).await?;
    }
    for eps in desired.endpoint_slices.values() {
        ssa_apply(&eps_api, cfg, eps).await?;
    }
    for be in desired.envoy_backends.values() {
        ssa_apply(&be_api, cfg, be).await?;
    }
    for btp in desired.backend_tls_policies.values() {
        ssa_apply(&btp_api, cfg, btp).await?;
    }
    // Then listener set so the parent for routes exists.
    for ls in desired.listener_sets.values() {
        ssa_apply(&ls_api, cfg, ls).await?;
    }
    for route in desired.http_routes.values() {
        ssa_apply(&route_api, cfg, route).await?;
    }
    for route in desired.tcp_routes.values() {
        ssa_apply(&tcp_api, cfg, route).await?;
    }
    for route in desired.udp_routes.values() {
        ssa_apply(&udp_api, cfg, route).await?;
    }
    // Policies last: they reference routes that must already exist.
    for sp in desired.security_policies.values() {
        ssa_apply(&sp_api, cfg, sp).await?;
    }

    // GC anything we own that's no longer wanted.
    let route_names: BTreeSet<String> = desired.http_routes.keys().cloned().collect();
    let ls_names: BTreeSet<String> = desired.listener_sets.keys().cloned().collect();
    let svc_names: BTreeSet<String> = desired.services.keys().cloned().collect();
    let eps_names: BTreeSet<String> = desired.endpoint_slices.keys().cloned().collect();
    let be_names: BTreeSet<String> = desired.envoy_backends.keys().cloned().collect();
    let tcp_names: BTreeSet<String> = desired.tcp_routes.keys().cloned().collect();
    let udp_names: BTreeSet<String> = desired.udp_routes.keys().cloned().collect();
    let sp_names: BTreeSet<String> = desired.security_policies.keys().cloned().collect();
    let btp_names: BTreeSet<String> = desired.backend_tls_policies.keys().cloned().collect();

    if let Err(e) = gc::sweep(&route_api, cfg, &route_names).await {
        warn!(error = ?e, "GC HTTPRoute failed");
    }
    if let Err(e) = gc::sweep(&ls_api, cfg, &ls_names).await {
        warn!(error = ?e, "GC ListenerSet failed");
    }
    if let Err(e) = gc::sweep(&eps_api, cfg, &eps_names).await {
        warn!(error = ?e, "GC EndpointSlice failed");
    }
    if let Err(e) = gc::sweep(&svc_api, cfg, &svc_names).await {
        warn!(error = ?e, "GC Service failed");
    }
    // Only sweep Envoy Backends when the controller is in that mode; otherwise
    // the Backend CRD may not be installed and the list call would 404.
    if cfg.backend_kind == BackendKind::EnvoyBackend
        && let Err(e) = gc::sweep(&be_api, cfg, &be_names).await
    {
        warn!(error = ?e, "GC Envoy Backend failed");
    }
    // TCPRoute/UDPRoute are experimental-channel CRDs; only list them when the
    // corresponding feature is on, for the same may-not-be-installed reason.
    if cfg.enable_tcp_routes
        && let Err(e) = gc::sweep(&tcp_api, cfg, &tcp_names).await
    {
        warn!(error = ?e, "GC TCPRoute failed");
    }
    if cfg.enable_udp_routes
        && let Err(e) = gc::sweep(&udp_api, cfg, &udp_names).await
    {
        warn!(error = ?e, "GC UDPRoute failed");
    }
    // SecurityPolicy is an Envoy Gateway CRD; only list it when ext-authz is
    // configured. Un-configuring ext-authz leaves stale policies behind —
    // delete them manually (or via the managed-by label) in that case.
    if cfg.ext_authz.is_some()
        && let Err(e) = gc::sweep(&sp_api, cfg, &sp_names).await
    {
        warn!(error = ?e, "GC SecurityPolicy failed");
    }
    // BackendTLSPolicy is a standard-channel Gateway API CRD; the documented
    // install (experimental channel >= v1.5) always carries it.
    if let Err(e) = gc::sweep(&btp_api, cfg, &btp_names).await {
        warn!(error = ?e, "GC BackendTLSPolicy failed");
    }

    // Surface rejected objects: a route the gateway refuses (missing
    // ReferenceGrant, unsupported listener, EG version mismatch) is otherwise
    // only visible via `kubectl describe`. Statuses are eventually consistent,
    // so a fresh apply may legitimately not be reflected yet — persistent
    // warnings across cycles are the real signal.
    report_status(&route_api, cfg, "HTTPRoute", |r: &HTTPRoute| {
        r.status
            .as_ref()
            .map(|s| {
                s.parents
                    .iter()
                    .flat_map(|p| p.conditions.clone())
                    .collect()
            })
            .unwrap_or_default()
    })
    .await;
    report_status(&ls_api, cfg, "ListenerSet", |ls: &ListenerSet| {
        ls.status
            .as_ref()
            .and_then(|s| s.conditions.clone())
            .unwrap_or_default()
    })
    .await;
    if cfg.enable_tcp_routes {
        report_status(&tcp_api, cfg, "TCPRoute", |r: &TCPRoute| {
            r.status
                .as_ref()
                .map(|s| {
                    s.parents
                        .iter()
                        .flat_map(|p| p.conditions.clone())
                        .collect()
                })
                .unwrap_or_default()
        })
        .await;
    }
    if cfg.enable_udp_routes {
        report_status(&udp_api, cfg, "UDPRoute", |r: &UDPRoute| {
            r.status
                .as_ref()
                .map(|s| {
                    s.parents
                        .iter()
                        .flat_map(|p| p.conditions.clone())
                        .collect()
                })
                .unwrap_or_default()
        })
        .await;
    }
    Ok(())
}

/// Log a warning for every managed object of one kind that carries a
/// `status=False` condition (Accepted, ResolvedRefs, Programmed, …).
async fn report_status<T, F>(api: &Api<T>, cfg: &Config, kind: &str, conditions: F)
where
    T: kube::Resource<DynamicType = ()> + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
    F: Fn(&T) -> Vec<Condition>,
{
    let lp = ListParams::default().labels(&cfg.managed_selector());
    let list = match api.list(&lp).await {
        Ok(l) => l,
        Err(e) => {
            warn!(error = ?e, kind, "listing for status report failed");
            return;
        }
    };
    for obj in &list.items {
        let name = obj.meta().name.as_deref().unwrap_or("<unnamed>");
        for c in conditions(obj) {
            if c.status == "False" {
                warn!(
                    kind,
                    name,
                    condition = %c.type_,
                    reason = %c.reason,
                    message = %c.message,
                    "object not accepted by the gateway"
                );
            }
        }
    }
}

/// Sleep for `dur`, returning true if shutdown was requested before the timer expired.
async fn wait_or_shutdown(dur: Duration, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        _ = shutdown.cancelled() => true,
    }
}
