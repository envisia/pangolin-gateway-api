//! Ext-authz shim binary: verifies pangolin (badger) sessions for Envoy
//! Gateway's external authorization filter. All logic lives in
//! `pangolin_gateway_controller::shim`.

use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::info;

use pangolin_gateway_controller::shim::{ShimConfig, ShimState, build_http_client, router};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = ShimConfig::from_env()?;
    info!(
        listen = %cfg.listen,
        pangolin = %cfg.pangolin_api_base_url,
        path_prefix = %cfg.path_prefix,
        "badger ext-authz shim starting"
    );

    let state = Arc::new(ShimState {
        http: build_http_client(&cfg)?,
        cfg: cfg.clone(),
    });

    let listener = tokio::net::TcpListener::bind(&cfg.listen)
        .await
        .with_context(|| format!("binding {}", cfg.listen))?;

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving")?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
    info!("shutdown signal received");
}
