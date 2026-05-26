//! Top-level reconcile loop: poll pangolin, transform, apply, GC.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::Result;
use gateway_api::apis::experimental::httproutes::HTTPRoute;
use gateway_api::apis::experimental::listenersets::ListenerSet;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::Api;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::apply::ssa_apply;
use crate::config::Config;
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
                    info!(bytes = raw_bytes, "pangolin: body unchanged (matched sha256)");
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

async fn reconcile_once(
    cfg: &Config,
    kube_client: &kube::Client,
    desired: &Desired,
) -> Result<()> {
    let ns = cfg.namespace.as_str();
    let route_api: Api<HTTPRoute> = Api::namespaced(kube_client.clone(), ns);
    let ls_api: Api<ListenerSet> = Api::namespaced(kube_client.clone(), ns);
    let svc_api: Api<Service> = Api::namespaced(kube_client.clone(), ns);
    let eps_api: Api<EndpointSlice> = Api::namespaced(kube_client.clone(), ns);

    // Apply backends first so HTTPRoute backendRefs resolve immediately.
    for svc in desired.services.values() {
        ssa_apply(&svc_api, cfg, svc).await?;
    }
    for eps in desired.endpoint_slices.values() {
        ssa_apply(&eps_api, cfg, eps).await?;
    }
    // Then listener set so the parent for routes exists.
    for ls in desired.listener_sets.values() {
        ssa_apply(&ls_api, cfg, ls).await?;
    }
    for route in desired.http_routes.values() {
        ssa_apply(&route_api, cfg, route).await?;
    }

    // GC anything we own that's no longer wanted.
    let route_names: BTreeSet<String> = desired.http_routes.keys().cloned().collect();
    let ls_names: BTreeSet<String> = desired.listener_sets.keys().cloned().collect();
    let svc_names: BTreeSet<String> = desired.services.keys().cloned().collect();
    let eps_names: BTreeSet<String> = desired.endpoint_slices.keys().cloned().collect();

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
    Ok(())
}

/// Sleep for `dur`, returning true if shutdown was requested before the timer expired.
async fn wait_or_shutdown(dur: Duration, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        _ = shutdown.cancelled() => true,
    }
}
