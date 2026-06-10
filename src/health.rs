//! Liveness/readiness endpoints for the controller binary.
//!
//! `/healthz` answers 200 as long as the process serves HTTP (liveness).
//! `/readyz` answers 200 once the controller has completed one successful
//! pangolin poll cycle, 503 before that (readiness). The flag is sticky:
//! transient pangolin outages after startup do not flip the pod unready,
//! they're surfaced via logs and backoff instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Clone, Default)]
pub struct Readiness(Arc<AtomicBool>);

impl Readiness {
    pub fn set_ready(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_ready(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

pub fn router(ready: Readiness) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/readyz",
            get(|State(ready): State<Readiness>| async move {
                if ready.is_ready() {
                    (StatusCode::OK, "ready")
                } else {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "waiting for first successful reconcile",
                    )
                }
            }),
        )
        .with_state(ready)
}

/// Serve the probe endpoints until `shutdown` fires.
pub async fn serve(listen: &str, ready: Readiness, shutdown: CancellationToken) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding health endpoint {listen}"))?;
    info!(listen, "health endpoints (/healthz, /readyz) up");
    axum::serve(listener, router(ready))
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .context("serving health endpoints")
}
