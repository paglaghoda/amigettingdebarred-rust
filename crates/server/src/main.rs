use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};

use debarred_server::{config::Config, db::MIGRATOR, ops::OpsConfig, AppState};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("debarred_server=info")),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .iter()
        .any(|arg| !matches!(arg.as_str(), "--migrate" | "--preview"))
        || args.len() > 1
    {
        return Err("usage: debarred-server [--migrate | --preview]".into());
    }
    if args.first().is_some_and(|arg| arg == "--preview") {
        let port: u16 = std::env::var("PREVIEW_PORT")
            .unwrap_or_else(|_| "3000".into())
            .parse()
            .map_err(|_| "PREVIEW_PORT must be a valid port")?;
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
        tracing::info!(address = %listener.local_addr()?, "credential-free preview listening");
        axum::serve(listener, debarred_server::preview_router())
            .with_graceful_shutdown(shutdown())
            .await?;
        return Ok(());
    }
    let config = Config::from_env()?;
    let ops = OpsConfig::from_env()?;
    let pool = PgPoolOptions::new()
        .max_connections(config.database_max_connections)
        .min_connections(0)
        .acquire_timeout(Duration::from_secs(5))
        .idle_timeout(Duration::from_secs(90))
        .max_lifetime(Duration::from_secs(1800))
        .connect_with(config.database_options()?)
        .await?;
    if args.first().is_some_and(|arg| arg == "--migrate") {
        MIGRATOR.run(&pool).await?;
        tracing::info!("database migrations applied");
        pool.close().await;
        return Ok(());
    }
    // Fail clearly on an unmigrated database rather than booting a broken API.
    debarred_server::verify_storage(&pool).await?;
    let bind = config.bind;
    let state = AppState::new(config, pool.clone())?;
    let maintenance_store = state.store.clone();
    let maintenance = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(error) = debarred_server::cleanup_expired(&maintenance_store).await {
                tracing::warn!(error = %error, "expiration maintenance failed");
            }
        }
    });
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(address = %listener.local_addr()?, "Rust API listening");
    let result = axum::serve(
        listener,
        debarred_server::router_with_ops(state, ops)
            .into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await;
    maintenance.abort();
    pool.close().await;
    result?;
    Ok(())
}

async fn shutdown() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = interrupt => {}, _ = terminate => {} }
}
