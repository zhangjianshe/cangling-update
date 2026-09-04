mod api;
mod auth;
mod backup;
mod binaries;
mod cluster;
mod db;
mod dbadmin;
mod docker;
mod error;
mod gitrepo;
mod hostinfo;
mod images;
mod k3s;
mod models;
mod paths;
mod portal;
mod progress;
mod repo;
mod service;
mod state;
mod storage;
mod term;
mod update;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use docker::Docker;
use paths::AppPaths;
use state::AppState;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

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

    /// 容器镜像离线包目录
    #[arg(long, env = "CANGLING_IMAGES_DIR", default_value = "/opt/cangling/images")]
    images_dir: PathBuf,

    /// Config directory (default: <executable-dir>/config)
    #[arg(long, env = "CANGLING_HOME", global = true)]
    data_dir: Option<PathBuf>,

    /// 集群角色：standalone（默认）/ master / worker
    #[arg(long, env = "CANGLING_ROLE", default_value = "standalone")]
    role: String,

    /// master 地址（worker 角色用），例如 http://10.1.1.5:5400；不填则 UDP 广播自动发现
    #[arg(long, env = "CANGLING_MASTER")]
    master: Option<String>,

    /// 集群共享令牌（master / worker 角色必须一致）
    #[arg(long, env = "CANGLING_CLUSTER_TOKEN", global = true)]
    cluster_token: Option<String>,

    /// UDP 发现端口（默认 5401）
    #[arg(long, env = "CANGLING_DISCOVERY_PORT", default_value_t = cluster::DEFAULT_DISCOVERY_PORT)]
    discovery_port: u16,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 签发控制台登录会话（供维护中心自动登录，不打印密码）
    IssueSession {
        /// 用户名；只有一个账号时可省略
        #[arg(short, long)]
        username: Option<String>,
    },
    /// 在主机上重置登录密码（忘记密码时使用）
    ResetPassword {
        /// 用户名；只有一个账号时可省略
        #[arg(short, long)]
        username: Option<String>,
        /// 新密码；省略则自动生成并打印一次
        #[arg(short, long)]
        password: Option<String>,
    },
    /// 修改登录密码（可同步到所有在线工作节点）
    ChangePassword {
        /// 用户名；只有一个账号时可省略
        #[arg(short, long)]
        username: Option<String>,
        /// 新密码；省略则自动生成并打印一次
        #[arg(short, long)]
        password: Option<String>,
        /// 同步到所有在线工作节点（需要集群令牌）
        #[arg(long)]
        sync: bool,
    },
    /// 将当前程序安装为 systemd 服务（工作目录为程序所在目录）
    InstallService,
    /// 从 systemd 卸载本服务
    UninstallService,
    /// 显示当前程序版本
    Version,
    /// 重启本服务
    Restart,
    /// 检查 GitHub Release 并下载新版本（同时保存 x86_64 与 ARM64，供 master 给 worker 升级；不重启服务）
    Update {
        /// 只检查是否有新版本，不下载
        #[arg(long)]
        check: bool,
        /// 即使版本相同或更旧也强制下载替换
        #[arg(long)]
        force: bool,
        /// HTTP/HTTPS/SOCKS 代理，例如 http://10.1.1.2:7890（也可设 https_proxy）
        #[arg(long, env = "HTTPS_PROXY")]
        proxy: Option<String>,
        /// 把一份二进制导入 updates/（按 ELF 识别架构），离线时给 master 补齐另一架构
        #[arg(long, value_name = "FILE")]
        import: Option<PathBuf>,
    },
    /// 采集主机信息，写入程序目录下的 info.md
    Hostinfo {
        /// 输出路径（默认：程序所在目录/info.md）
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// 检测 k3s：写入 Traefik 入口端口配置（HTTP 8020 / HTTPS 8443），并确保 /root/.kube/config
    #[command(name = "fix-k3s")]
    FixK3s,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::IssueSession { username }) => {
            let paths = AppPaths::resolve(cli.data_dir)?;
            return issue_session_cli(&paths, username);
        }
        Some(Command::ResetPassword { username, password }) => {
            let paths = AppPaths::resolve(cli.data_dir)?;
            return reset_password(&paths, username, password);
        }
        Some(Command::ChangePassword {
            username,
            password,
            sync,
        }) => {
            let paths = AppPaths::resolve(cli.data_dir)?;
            return change_password(
                &paths,
                cli.cluster_token.as_deref(),
                username,
                password,
                sync,
            )
            .await;
        }
        Some(Command::InstallService) => {
            return service::install(&cli.bind, cli.port, cli.data_dir.as_deref());
        }
        Some(Command::UninstallService) => {
            return service::uninstall();
        }
        Some(Command::Version) => {
            println!("v{}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some(Command::Restart) => {
            return service::restart();
        }
        Some(Command::Update {
            check,
            force,
            proxy,
            import,
        }) => {
            if let Some(path) = import {
                return update::import(&path);
            }
            return update::run(check, force, proxy);
        }
        Some(Command::Hostinfo { output }) => {
            let paths = AppPaths::resolve(cli.data_dir)?;
            return hostinfo::run(&paths, output);
        }
        Some(Command::FixK3s) => {
            return k3s::fix();
        }
        None => {
            if service::is_installed() && !service::running_as_systemd_service() {
                return service::print_installed_access();
            }
        }
    }

    let paths = AppPaths::resolve(cli.data_dir)?;
    init_logging(&paths.log_file);

    let cluster_cfg = cluster::ClusterConfig {
        role: cluster::Role::parse(&cli.role).map_err(|e| anyhow::anyhow!("{e}"))?,
        token: cli.cluster_token.clone(),
        master_url: cli.master.clone(),
        discovery_port: cli.discovery_port,
    };
    if matches!(
        cluster_cfg.role,
        cluster::Role::Master | cluster::Role::Worker
    ) && cluster_cfg
        .token
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        bail!("集群角色 master/worker 需要 --cluster-token（或 CANGLING_CLUSTER_TOKEN）");
    }

    let conn = db::open(&paths.db_path)?;
    let docker = Docker::detect().await;
    let docker_meta = docker.meta().await;

    tracing::info!(
        exe_dir = %paths.exe_dir.display(),
        config = %paths.config_dir.display(),
        db = %paths.db_path.display(),
        logs = %paths.logs_dir.display(),
        log = %paths.log_file.display(),
        docker = ?docker_meta.version,
        compose = %docker_meta.compose,
        docker_available = docker_meta.available,
        "starting cangling-update"
    );

    let addr: SocketAddr = format!("{}:{}", cli.bind, cli.port)
        .parse()
        .with_context(|| format!("invalid listen address {}:{}", cli.bind, cli.port))?;

    let state = AppState::new(
        paths,
        conn,
        docker,
        cli.port,
        cluster_cfg.clone(),
        cli.images_dir,
    );
    let app = api::router(state.clone());

    // 集群后台任务
    if cluster_cfg.role == cluster::Role::Master {
        match crate::binaries::seed_own_binary(&state.paths.exe_dir) {
            Ok(arch) => {
                let missing: Vec<_> = crate::binaries::inventory(&state.paths.exe_dir)
                    .into_iter()
                    .filter(|b| !b.available)
                    .map(|b| b.label)
                    .collect();
                if missing.is_empty() {
                    tracing::info!(
                        "升级二进制已就绪（本机 {}）：{}",
                        arch.label(),
                        crate::binaries::updates_dir(&state.paths.exe_dir).display()
                    );
                } else {
                    tracing::warn!(
                        "已写入本机 {} 升级二进制；缺少 {}。不同架构的 worker 无法自动升级。请把对应文件放到 {}，或执行 cangling-update update / update --import",
                        arch.label(),
                        missing.join("、"),
                        crate::binaries::updates_dir(&state.paths.exe_dir).display()
                    );
                }
            }
            Err(err) => tracing::warn!("无法写入本机升级二进制：{err:#}"),
        }
        tokio::spawn(cluster::server::maintain_self_node(state.clone()));
        if let Some(cid) = cluster_cfg.cluster_id() {
            let announce = hostinfo::primary_ip();
            let port = cluster_cfg.discovery_port;
            let http_port = cli.port;
            let cid = cid.clone();
            tokio::spawn(async move {
                if let Err(err) = cluster::discovery::serve(port, &cid, &announce, http_port).await
                {
                    tracing::error!("集群发现服务退出：{err:#}");
                }
            });
        }
    } else if cluster_cfg.role == cluster::Role::Worker {
        tokio::spawn(cluster::client::run(state.clone()));
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("web interface http://{addr}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

async fn change_password(
    paths: &AppPaths,
    token: Option<&str>,
    username: Option<String>,
    password: Option<String>,
    sync: bool,
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

    eprintln!("已修改用户 {username} 的密码，旧登录会话已全部失效。");
    if generated {
        println!("{password}");
        eprintln!("请立即登录。以上密码只显示一次。");
    }

    if sync {
        let token = token.map(str::trim).filter(|s| !s.is_empty());
        let Some(token) = token else {
            bail!("--sync 需要集群令牌（--cluster-token 或 CANGLING_CLUSTER_TOKEN）");
        };
        let workers = cluster::server::online_workers_in(&conn)?;
        let mut synced = 0usize;
        let mut failures = Vec::new();
        for (name, addr) in workers {
            let url = format!("http://{addr}/api/cluster/auth/sync");
            let body = serde_json::json!({
                "username": username.clone(),
                "password_hash": hash.clone(),
            });
            match cluster::http::post_json(&url, token, &body).await {
                Ok((status, _)) if status.is_success() => {
                    eprintln!("已同步到 {name}");
                    synced += 1;
                }
                Ok((status, value)) => {
                    let msg = value
                        .get("error")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| value.to_string());
                    eprintln!("同步失败 {name}：HTTP {status}: {msg}");
                    failures.push(name);
                }
                Err(e) => {
                    eprintln!("同步失败 {name}：{e:#}");
                    failures.push(name);
                }
            }
        }
        eprintln!("同步完成：成功 {synced} 个，失败 {} 个。", failures.len());
    }
    Ok(())
}

fn issue_session_cli(paths: &AppPaths, username: Option<String>) -> anyhow::Result<()> {
    let conn = db::open(&paths.db_path)?;
    let names = db::list_usernames(&conn)?;
    if names.is_empty() {
        println!("CK_SESSION|ok=0|error=needs_setup");
        return Ok(());
    }
    let username = match username {
        Some(u) => u,
        None if names.len() == 1 => names[0].clone(),
        None => {
            println!("CK_SESSION|ok=0|error=multiple_users");
            bail!(
                "存在多个用户，请指定 --username。当前用户：{}",
                names.join(", ")
            );
        }
    };
    let Some(user) = db::get_user_by_name(&conn, &username)? else {
        println!("CK_SESSION|ok=0|error=no_user");
        bail!("用户不存在：{username}");
    };
    let token = uuid::Uuid::new_v4().to_string();
    db::create_session(&conn, &token, &user.id)?;
    println!(
        "CK_SESSION|ok=1|username={}|token={}",
        user.username, token
    );
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

fn init_logging(log_file: &Path) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal());

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
    {
        Ok(file) => {
            let file_layer = fmt::layer()
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false);
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .with(file_layer)
                .init();
        }
        Err(err) => {
            eprintln!(
                "无法写入日志文件 {}：{err}，仅输出到终端",
                log_file.display()
            );
            tracing_subscriber::registry()
                .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
                .with(stderr_layer)
                .init();
        }
    }
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
