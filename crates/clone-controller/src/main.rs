//! clone-controller: multi-tenant orchestration layer for the Clone VMM.
//!
//! Sits between openresty and the clone daemon. Provides tenant lifecycle
//! (register / acquire / release / delete), idle-driven aging
//! (soft reclaim via balloon, hard reclaim via save+kill), and Prometheus
//! metrics. State persists to JSON so a controller restart picks up where
//! the previous process left off.

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use clone_controller::{
    api, clone_client::CloneClient, config::{Cli, ControllerConfig}, reconciler, state::ControllerState,
};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let config: ControllerConfig = cli.into();

    tracing::info!(listen = %config.listen, daemon = %config.clone_daemon_url, "clone-controller starting");

    // 1. Connectivity probe against the daemon (skip if flag set).
    let clone_client = CloneClient::new(
        config.clone_daemon_url.clone(),
        config.clone_auth_token.clone(),
    );
    if !config.skip_daemon_health_check {
        if let Err(e) = clone_client.health().await {
            tracing::error!(error = ?e, "clone daemon unreachable at startup; pass --skip-daemon-health-check to bypass");
            std::process::exit(1);
        }
        tracing::info!("clone daemon health check passed");
    }

    // 2. Load persisted state.
    let tenants = ControllerState::load(&config.state_file).await?;
    tracing::info!(tenants = tenants.len(), "loaded persisted state");
    let state = Arc::new(ControllerState::new(config.clone(), clone_client));
    {
        let mut map = state.tenants.write().await;
        *map = tenants;
    }

    // 3. Ensure saves root exists.
    tokio::fs::create_dir_all(&config.saves_root)
        .await
        .ok();

    // 4. Launch background reconciler.
    reconciler::spawn(state.clone());

    // 5. Start HTTP server.
    let app = api::router(state.clone());
    let listener = TcpListener::bind(&config.listen)
        .await
        .map_err(|e| anyhow::anyhow!("bind {}: {e}", config.listen))?;
    tracing::info!(addr = %config.listen, "HTTP server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| anyhow::anyhow!("axum::serve: {e}"))?;

    tracing::info!("shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl-c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}
