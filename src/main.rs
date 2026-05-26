use std::process::ExitCode;

use anyhow::Context;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info};

mod apply;
mod config;
mod envoy_gateway;
mod gc;
mod pangolin;
mod reconcile;
mod transform;

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    let cfg = match config::Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("invalid configuration: {e:#}");
            return ExitCode::from(2);
        }
    };
    info!(
        endpoint = %cfg.pangolin_endpoint,
        namespace = %cfg.namespace,
        gateway = %cfg.parent_gateway,
        "starting pangolin-gateway-controller"
    );

    let shutdown = tokio_util::sync::CancellationToken::new();
    spawn_signal_listener(shutdown.clone());

    if let Err(e) = run(cfg, shutdown).await {
        error!(error = ?e, "controller exited with error");
        return ExitCode::FAILURE;
    }

    info!("controller exited cleanly");
    ExitCode::SUCCESS
}

async fn run(
    cfg: config::Config,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let kube = kube::Client::try_default()
        .await
        .context("connecting to Kubernetes API")?;

    let pangolin_client = pangolin::Client::new(&cfg).context("building pangolin HTTP client")?;

    reconcile::run_loop(cfg, kube, pangolin_client, shutdown).await
}

fn spawn_signal_listener(token: tokio_util::sync::CancellationToken) {
    tokio::spawn(async move {
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut intr = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => info!("SIGTERM received"),
            _ = intr.recv() => info!("SIGINT received"),
        }
        token.cancel();
    });
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,kube=info"));
    let layer = fmt::layer().with_target(true);
    tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .init();
}

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
