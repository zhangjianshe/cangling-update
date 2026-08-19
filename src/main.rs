mod api;
mod auth;
mod backup;
mod db;
mod docker;
mod error;
mod models;
mod paths;
mod state;

use anyhow::Context;
use clap::Parser;
use docker::Docker;
use paths::AppPaths;
use state::AppState;
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "cangling-update",
    version,
    about = "Single-binary docker-compose update console"
)]
struct Cli {
    /// Listen address
    #[arg(long, env = "CANGLING_BIND", default_value = "0.0.0.0")]
    bind: String,

    /// Listen port
    #[arg(long, env = "CANGLING_PORT", default_value_t = 5400)]
    port: u16,

    /// Config directory (default: <executable-dir>/config)
    #[arg(long, env = "CANGLING_HOME")]
    data_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let paths = AppPaths::resolve(cli.data_dir)?;
    let conn = db::open(&paths.db_path)?;
    let docker = Docker::detect().await;

    tracing::info!(
        exe_dir = %paths.exe_dir.display(),
        config = %paths.config_dir.display(),
        db = %paths.db_path.display(),
        docker = ?docker.version,
        compose = docker.compose.as_str(),
        "starting cangling-update"
    );

    let addr: SocketAddr = format!("{}:{}", cli.bind, cli.port)
        .parse()
        .with_context(|| format!("invalid listen address {}:{}", cli.bind, cli.port))?;

    let state = AppState::new(paths, conn, docker, cli.port);
    let app = api::router(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("web interface http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
