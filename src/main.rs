mod api;
mod auth;
mod backup;
mod db;
mod docker;
mod error;
mod models;
mod paths;
mod state;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
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
    #[arg(long, env = "CANGLING_HOME", global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 在主机上重置登录密码（忘记密码时使用）
    ResetPassword {
        /// 用户名；只有一个账号时可省略
        #[arg(short, long)]
        username: Option<String>,
        /// 新密码；省略则自动生成并打印一次
        #[arg(short, long)]
        password: Option<String>,
    },
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

    if let Some(Command::ResetPassword { username, password }) = cli.command {
        return reset_password(&paths, username, password);
    }

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

fn reset_password(
    paths: &AppPaths,
    username: Option<String>,
    password: Option<String>,
) -> anyhow::Result<()> {
    let conn = db::open(&paths.db_path)?;
    let names = db::list_usernames(&conn)?;
    if names.is_empty() {
        bail!("还没有管理员账号。请先打开网页完成初始化。");
    }

    let username = match username {
        Some(u) => u,
        None if names.len() == 1 => names[0].clone(),
        None => bail!(
            "存在多个用户，请指定 --username。当前用户：{}",
            names.join(", ")
        ),
    };

    let Some(user) = db::get_user_by_name(&conn, &username)? else {
        bail!("用户不存在：{username}。当前用户：{}", names.join(", "));
    };

    let (password, generated) = match password {
        Some(p) => (p, false),
        None => (generate_password(), true),
    };
    auth::validate_password(&password).map_err(|e| anyhow::anyhow!("{e}"))?;
    let hash = auth::hash_password(&password).map_err(|e| anyhow::anyhow!("{e}"))?;
    db::update_password_hash(&conn, &user.id, &hash)?;
    db::delete_sessions_for_user(&conn, &user.id)?;

    eprintln!("已重置用户 {username} 的密码，旧登录会话已全部失效。");
    if generated {
        println!("{password}");
        eprintln!("请立即登录并自行改密。以上密码只显示一次。");
    }
    Ok(())
}

fn generate_password() -> String {
    format!(
        "{}{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8],
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
