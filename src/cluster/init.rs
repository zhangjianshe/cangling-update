//! 集群初始化：master 一键安装本机与各 worker 的基线软件（全部来自程序目录下的 `cangling-repo/`）。
//!
//! master 角色：git、samba、CIFS/NFS、docker、k3s-server（含 Traefik 端口 8020/8443）、k9s。
//! worker 角色：git、samba、CIFS/NFS、docker、k3s-agent（携带 K3S_URL/K3S_TOKEN 加入集群）。

use crate::cluster::Role;
use crate::db;
use crate::error::AppError;
use crate::hostinfo;
use crate::k3s;
use crate::models::Project;
use crate::repo;
use crate::state::AppState;
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;

pub const MASTER_SOFTWARE: &[&str] = &[
    "git",
    "samba",
    "cifs-utils",
    "nfs-common",
    "nfs-server",
    "docker",
    "docker-compose",
    "k3s-server",
    "k9s",
];
pub const WORKER_SOFTWARE: &[&str] = &[
    "git",
    "samba",
    "cifs-utils",
    "nfs-common",
    "docker",
    "docker-compose",
    "k3s-agent",
];
pub const CLUSTER_NAME_KEY: &str = "cluster_name";
pub const K3S_DATA_DIR_KEY: &str = "k3s_data_dir";
const TRAEFIK_STEP: &str = "traefik-8020/8443";
const KUBECONFIG_STEP: &str = "~/.kube/config";
const HOSTNAME_STEP: &str = "设置主机名";
const WORKER_BOOTSTRAP_STEP: &str = "安装 cangling-update Worker";
const LOADBALANCE_STEP: &str = "Load Balancer :7800/update";
const LOADBALANCE_IMAGE: &str = "hub.cangling.cn/cangling/nginx-rtmp:v4.2.35";

#[derive(Debug, Clone)]
struct BootstrapHost {
    name: String,
    host: String,
    port: u16,
    username: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct InitStep {
    pub node: String,
    pub role: String,
    pub package: String,
    /// pending / running / ok / failed
    pub state: String,
    pub output: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct InitStatus {
    pub running: bool,
    /// init / check
    pub mode: String,
    pub cluster_name: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub steps: Vec<InitStep>,
}

pub type InitState = Arc<Mutex<InitStatus>>;

#[derive(Debug, Deserialize)]
pub struct StartInitBody {
    pub name: String,
    #[serde(default)]
    pub k3s_data_dir: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerInitRequest {
    pub cluster_name: String,
    #[serde(default)]
    pub k3s_url: String,
    #[serde(default)]
    pub k3s_token: String,
    #[serde(default)]
    pub k3s_data_dir: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub software: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerInitResult {
    pub node: String,
    pub role: String,
    pub ok: bool,
    pub packages: Vec<repo::InstallResult>,
}

/// 控制台（登录态）入口：设置集群名并启动初始化。
pub async fn start_init(
    State(state): State<AppState>,
    Json(body): Json<StartInitBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::bad("集群名称不能为空"));
    }
    validate_k3s_data_dir(&body.k3s_data_dir)?;
    validate_cluster_hostnames(&state)?;
    launch(
        state,
        Some(name),
        Some(body.k3s_data_dir.trim().to_string()),
        "init".to_string(),
    )
    .await
}

fn validate_k3s_data_dir(value: &str) -> Result<(), AppError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(());
    }
    let path = Path::new(value);
    if !path.is_absolute()
        || path == Path::new("/")
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(AppError::bad(
            "K3s 数据目录必须是安全的绝对路径，且不能是根目录 /",
        ));
    }
    Ok(())
}

/// 控制台（登录态）入口：检查各节点并修复（含新加入节点），使用已保存的集群名。
pub async fn start_check(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let name = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::cluster_setting(&conn, CLUSTER_NAME_KEY)?.unwrap_or_default()
    };
    let data_dir = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::cluster_setting(&conn, K3S_DATA_DIR_KEY)?.unwrap_or_default()
    };
    launch(state, Some(name), Some(data_dir), "check".to_string()).await
}

async fn launch(
    state: AppState,
    name: Option<String>,
    k3s_data_dir: Option<String>,
    mode: String,
) -> Result<Json<serde_json::Value>, AppError> {
    let name = name.unwrap_or_default();
    if state.cluster.role != Role::Master {
        return Err(AppError::bad("只有主节点可以执行此操作"));
    }
    {
        let st = state
            .init
            .lock()
            .map_err(|_| AppError::internal("初始化状态锁不可用"))?;
        if st.running {
            return Err(AppError::conflict("集群正在初始化/检查中，请稍候"));
        }
    }
    if !name.trim().is_empty() {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::set_cluster_setting(&conn, CLUSTER_NAME_KEY, name.trim())?;
        db::set_cluster_setting(
            &conn,
            K3S_DATA_DIR_KEY,
            k3s_data_dir.as_deref().unwrap_or_default(),
        )?;
    }

    let state2 = state.clone();
    let name = name.trim().to_string();
    let k3s_data_dir = k3s_data_dir.unwrap_or_default();
    tokio::spawn(async move {
        run_init(state2, name, k3s_data_dir, mode).await;
    });
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// 控制台（登录态）查询初始化进度。
pub async fn status(State(state): State<AppState>) -> Result<Json<InitStatus>, AppError> {
    let st = state
        .init
        .lock()
        .map_err(|_| AppError::internal("初始化状态锁不可用"))?;
    Ok(Json(st.clone()))
}

/// 机器间（令牌态）接口：worker 执行自己的角色软件安装。
pub async fn run_worker_init(
    State(state): State<AppState>,
    Json(body): Json<WorkerInitRequest>,
) -> Result<Json<WorkerInitResult>, AppError> {
    if state.cluster.role != Role::Worker {
        return Err(AppError::bad("只有工作节点可以执行工作节点初始化"));
    }
    let mut packages = Vec::new();
    let mut ok = true;
    if !body.hostname.trim().is_empty() {
        match set_system_hostname(&body.hostname) {
            Ok(output) => packages.push(step_result(HOSTNAME_STEP, output, true)),
            Err(error) => {
                ok = false;
                packages.push(step_result(HOSTNAME_STEP, format!("{error:#}"), false));
            }
        }
    }
    for pkg in &body.software {
        let envs = vec![
            (
                "CANGLING_CLUSTER_NAME".to_string(),
                body.cluster_name.clone(),
            ),
            ("K3S_URL".to_string(), body.k3s_url.clone()),
            ("K3S_TOKEN".to_string(), body.k3s_token.clone()),
            ("K3S_DATA_DIR".to_string(), body.k3s_data_dir.clone()),
        ];
        match repo::install_package(&state, repo::host_platform(), pkg, &envs).await {
            Ok(r) => {
                if r.exit_code != Some(0) || r.timed_out {
                    ok = false;
                }
                packages.push(r);
            }
            Err(e) => {
                ok = false;
                packages.push(repo::InstallResult {
                    package: pkg.clone(),
                    installer: String::new(),
                    stdout: String::new(),
                    stderr: e.to_string(),
                    exit_code: None,
                    timed_out: false,
                    elapsed_ms: 0,
                });
            }
        }
    }
    Ok(Json(WorkerInitResult {
        node: node_label(),
        role: "worker".to_string(),
        ok,
        packages,
    }))
}

async fn run_init(state: AppState, name: String, k3s_data_dir: String, mode: String) {
    let now = now_rfc3339();
    {
        let mut st = match state.init.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        st.running = true;
        st.mode = mode;
        st.cluster_name = name.clone();
        st.started_at = Some(now.clone());
        st.finished_at = None;
        st.error = None;
        st.steps = build_steps(&state);
    }

    let result = run_init_inner(&state, &name, &k3s_data_dir).await;

    let mut st = match state.init.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    st.running = false;
    st.finished_at = Some(now_rfc3339());
    if result.is_err() {
        st.error = Some(format!("{:#}", result.as_ref().unwrap_err()));
    } else if st.steps.iter().any(|s| s.state == "failed") {
        st.error = Some("部分步骤失败，详见下方步骤列表".to_string());
    }
}

fn build_steps(state: &AppState) -> Vec<InitStep> {
    let mut steps = Vec::new();
    let me = managed_master_name(state).unwrap_or_else(|_| node_label());
    steps.push(InitStep {
        node: me.clone(),
        role: "master".to_string(),
        package: HOSTNAME_STEP.to_string(),
        state: "pending".to_string(),
        output: String::new(),
        elapsed_ms: 0,
    });
    for pkg in MASTER_SOFTWARE {
        steps.push(InitStep {
            node: me.clone(),
            role: "master".to_string(),
            package: pkg.to_string(),
            state: "pending".to_string(),
            output: String::new(),
            elapsed_ms: 0,
        });
    }
    steps.push(InitStep {
        node: me.clone(),
        role: "master".to_string(),
        package: TRAEFIK_STEP.to_string(),
        state: "pending".to_string(),
        output: String::new(),
        elapsed_ms: 0,
    });
    steps.push(InitStep {
        node: me.clone(),
        role: "master".to_string(),
        package: KUBECONFIG_STEP.to_string(),
        state: "pending".to_string(),
        output: String::new(),
        elapsed_ms: 0,
    });

    if let Ok(workers) = managed_worker_hosts(state) {
        for worker in workers {
            let wname = worker.name;
            steps.push(InitStep {
                node: wname.clone(),
                role: "worker".to_string(),
                package: HOSTNAME_STEP.to_string(),
                state: "pending".to_string(),
                output: String::new(),
                elapsed_ms: 0,
            });
            steps.push(InitStep {
                node: wname.clone(),
                role: "worker".to_string(),
                package: WORKER_BOOTSTRAP_STEP.to_string(),
                state: "pending".to_string(),
                output: String::new(),
                elapsed_ms: 0,
            });
            for pkg in WORKER_SOFTWARE {
                steps.push(InitStep {
                    node: wname.clone(),
                    role: "worker".to_string(),
                    package: pkg.to_string(),
                    state: "pending".to_string(),
                    output: String::new(),
                    elapsed_ms: 0,
                });
            }
        }
    }
    steps.push(InitStep {
        node: me,
        role: "master".to_string(),
        package: LOADBALANCE_STEP.to_string(),
        state: "pending".to_string(),
        output: String::new(),
        elapsed_ms: 0,
    });
    steps
}

async fn run_init_inner(state: &AppState, name: &str, k3s_data_dir: &str) -> Result<(), AppError> {
    let me = managed_master_name(state)?;

    update_step(
        state,
        &me,
        "master",
        HOSTNAME_STEP,
        "running",
        String::new(),
        0,
    );
    let started = Instant::now();
    match set_system_hostname(&me) {
        Ok(output) => update_step(
            state,
            &me,
            "master",
            HOSTNAME_STEP,
            "ok",
            output,
            started.elapsed().as_millis() as u64,
        ),
        Err(error) => {
            update_step(
                state,
                &me,
                "master",
                HOSTNAME_STEP,
                "failed",
                format!("{error:#}"),
                started.elapsed().as_millis() as u64,
            );
            return Err(error);
        }
    }

    bootstrap_workers(state).await?;

    // 1) 本机（master）软件依次安装。单个软件失败只记录结果，继续后续项目；
    // 最终状态会根据 failed 步骤提示“部分步骤失败”。
    for pkg in MASTER_SOFTWARE {
        update_step(state, &me, "master", pkg, "running", String::new(), 0);
        let envs = vec![
            ("CANGLING_CLUSTER_NAME".to_string(), name.to_string()),
            ("K3S_DATA_DIR".to_string(), k3s_data_dir.to_string()),
        ];
        let started = Instant::now();
        let res = repo::install_package(state, repo::host_platform(), pkg, &envs).await;
        let elapsed = started.elapsed().as_millis() as u64;
        match res {
            Ok(r) => {
                let output = combined_output(&r);
                if r.exit_code == Some(0) && !r.timed_out {
                    update_step(state, &me, "master", pkg, "ok", output, elapsed);
                } else {
                    update_step(state, &me, "master", pkg, "failed", output, elapsed);
                }
            }
            Err(e) => {
                update_step(
                    &state,
                    &me,
                    "master",
                    pkg,
                    "failed",
                    format!("{e:#}"),
                    elapsed,
                );
            }
        }
    }

    // 2) Traefik 端口覆盖（best-effort，k3s 未装好时仅告警）。
    update_step(
        state,
        &me,
        "master",
        TRAEFIK_STEP,
        "running",
        String::new(),
        0,
    );
    let started = Instant::now();
    let elapsed = started.elapsed().as_millis() as u64;
    let effective_data_dir = k3s::effective_data_dir(k3s_data_dir);
    match k3s::ensure_traefik_config_at(&effective_data_dir) {
        Ok(msg) => update_step(state, &me, "master", TRAEFIK_STEP, "ok", msg, elapsed),
        Err(e) => update_step(
            state,
            &me,
            "master",
            TRAEFIK_STEP,
            "failed",
            format!("{e:#}"),
            elapsed,
        ),
    }

    // 2b) kubectl/k9s 默认 kubeconfig：检查 /root/.kube/config，缺失则从 k3s.yaml 拷贝。
    update_step(
        state,
        &me,
        "master",
        KUBECONFIG_STEP,
        "running",
        String::new(),
        0,
    );
    let started = Instant::now();
    match k3s::ensure_kubeconfig() {
        Ok(msg) => update_step(
            state,
            &me,
            "master",
            KUBECONFIG_STEP,
            "ok",
            msg,
            started.elapsed().as_millis() as u64,
        ),
        Err(e) => update_step(
            state,
            &me,
            "master",
            KUBECONFIG_STEP,
            "failed",
            format!("{e:#}"),
            started.elapsed().as_millis() as u64,
        ),
    }

    // 3) 读取 k3s 加入令牌与主节点地址，下发给 worker。
    let token_path = format!("{effective_data_dir}/server/node-token");
    let k3s_token = read_file(&token_path).unwrap_or_default();
    let k3s_url = format!("https://{}:6443", hostinfo::primary_ip());

    // 4) 各 worker 依次初始化（失败不阻断其它 worker）。
    for (wname, waddr) in online_workers(state)? {
        init_worker(
            state,
            &wname,
            &waddr,
            name,
            &k3s_url,
            &k3s_token,
            k3s_data_dir,
        )
        .await;
    }

    update_step(
        state,
        &me,
        "master",
        LOADBALANCE_STEP,
        "running",
        String::new(),
        0,
    );
    let started = Instant::now();
    match ensure_loadbalance(state) {
        Ok(output) => update_step(
            state,
            &me,
            "master",
            LOADBALANCE_STEP,
            "ok",
            output,
            started.elapsed().as_millis() as u64,
        ),
        Err(error) => update_step(
            state,
            &me,
            "master",
            LOADBALANCE_STEP,
            "failed",
            format!("{error:#}"),
            started.elapsed().as_millis() as u64,
        ),
    }

    Ok(())
}

pub(crate) fn ensure_loadbalance(state: &AppState) -> Result<String, AppError> {
    const DIR: &str = "/opt/cangling/loadbalance";
    let image_action = ensure_loadbalance_image(state)?;
    const NGINX: &str = r#"worker_processes auto;
worker_rlimit_nofile 65535;
events {
  worker_connections 16384;
  multi_accept on;
  use epoll;
}
http {
  map $http_upgrade $connection_upgrade { default upgrade; '' close; }
  keepalive_timeout 65s;
  keepalive_requests 10000;
  client_max_body_size 0;
  access_log /var/log/nginx/access.log;
  error_log /var/log/nginx/error.log warn;
  server {
    listen 7800;
    server_name _;
    location = /update { return 308 /update/; }
    location /update/ {
      proxy_pass http://127.0.0.1:5400/;
      proxy_http_version 1.1;
      proxy_cache off;
      proxy_request_buffering off;
      proxy_buffering off;
      proxy_max_temp_file_size 0;
      proxy_set_header Host $host;
      proxy_set_header X-Real-IP $remote_addr;
      proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
      proxy_set_header X-Forwarded-Proto $scheme;
      proxy_set_header X-Forwarded-Prefix /update;
      proxy_set_header Upgrade $http_upgrade;
      proxy_set_header Connection $connection_upgrade;
      proxy_read_timeout 3600s;
      proxy_send_timeout 3600s;
    }
  }
}
"#;
    crate::certificate_manager::ensure_embedded_root()?;
    let nginx = if Path::new("/opt/cangling/loadbalance/certs/loadbalance.crt.pem").is_file()
        && Path::new("/opt/cangling/loadbalance/certs/loadbalance.key.pem").is_file()
    {
        NGINX.replace(
            "    listen 7800;\n",
            "    listen 7800;\n    listen 7443 ssl;\n    ssl_certificate /etc/nginx/certs/loadbalance.crt.pem;\n    ssl_certificate_key /etc/nginx/certs/loadbalance.key.pem;\n    ssl_protocols TLSv1.2 TLSv1.3;\n    ssl_session_cache shared:TLS:20m;\n    ssl_session_timeout 1d;\n",
        )
    } else {
        NGINX.to_string()
    };
    let compose = format!(
        "services:\n  loadbalance:\n    image: {LOADBALANCE_IMAGE}\n    container_name: cangling-loadbalance\n    network_mode: host\n    restart: unless-stopped\n    ulimits:\n      nofile:\n        soft: 65535\n        hard: 65535\n    volumes:\n      - ./nginx.conf:/etc/nginx/nginx.conf:ro\n      - ./logs:/var/log/nginx\n      - ./certs:/etc/nginx/certs:ro\n    command: [\"/usr/local/nginx/sbin/nginx\", \"-g\", \"daemon off;\"]\n"
    );
    std::fs::create_dir_all(format!("{DIR}/logs"))
        .map_err(|error| AppError::internal(format!("创建 Load Balancer 目录失败：{error}")))?;
    std::fs::create_dir_all(format!("{DIR}/certs"))
        .map_err(|error| AppError::internal(format!("创建证书目录失败：{error}")))?;
    std::fs::write(format!("{DIR}/nginx.conf"), nginx)
        .map_err(|error| AppError::internal(format!("写入 Nginx 配置失败：{error}")))?;
    std::fs::write(format!("{DIR}/docker-compose.yaml"), compose)
        .map_err(|error| AppError::internal(format!("写入 Compose 配置失败：{error}")))?;
    let output = Command::new("docker")
        .args([
            "compose",
            "-f",
            &format!("{DIR}/docker-compose.yaml"),
            "up",
            "-d",
        ])
        .output()
        .map_err(|error| AppError::internal(format!("启动 Load Balancer 失败：{error}")))?;
    if !output.status.success() {
        return Err(AppError::internal(format!(
            "启动 Load Balancer 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    ensure_loadbalance_log_rotation()?;
    let project_action = register_loadbalance_project(state, DIR)?;
    let traffic = crate::nginx_stats::configure_loadbalance_logs(state)?;
    Ok(format!(
        "{image_action}；Load Balancer 已启动：http://{}:7800/update/ → http://127.0.0.1:5400/；日志每 5 分钟检查并按 500 MB 轮转；{}；项目面板{}“负载均衡”项目",
        hostinfo::primary_ip(),
        traffic,
        project_action
    ))
}

fn ensure_loadbalance_image(state: &AppState) -> Result<String, AppError> {
    let inspect = Command::new("docker")
        .args(["image", "inspect", LOADBALANCE_IMAGE])
        .output()
        .map_err(|error| AppError::internal(format!("检查负载均衡镜像失败：{error}")))?;
    if inspect.status.success() {
        return Ok(format!("镜像已存在：{LOADBALANCE_IMAGE}"));
    }

    let arch = loadbalance_image_arch(std::env::consts::ARCH)?;
    let filename = format!("nginx-rtmp-v4.2.35-linux-{arch}.tar.gz");
    let image_dir = repo::repo_root(&state.paths)
        .join("apps")
        .join("base-images")
        .join("linux")
        .join(arch);
    let archive = image_dir.join(&filename);
    let sums = image_dir.join("SHA256SUMS");
    if !archive.is_file() {
        return Err(AppError::internal(format!(
            "缺少 {arch} 负载均衡离线镜像：{}",
            archive.display()
        )));
    }
    verify_image_checksum(&archive, &sums)?;

    let output = Command::new("docker")
        .args(["load", "-i"])
        .arg(&archive)
        .output()
        .map_err(|error| AppError::internal(format!("导入负载均衡镜像失败：{error}")))?;
    if !output.status.success() {
        return Err(AppError::internal(format!(
            "导入负载均衡镜像失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let inspect = Command::new("docker")
        .args(["image", "inspect", LOADBALANCE_IMAGE])
        .output()
        .map_err(|error| AppError::internal(format!("验证负载均衡镜像失败：{error}")))?;
    if !inspect.status.success() {
        return Err(AppError::internal(format!(
            "离线包导入后仍未找到镜像：{LOADBALANCE_IMAGE}"
        )));
    }
    Ok(format!("已从离线仓库导入镜像：{filename}"))
}

fn loadbalance_image_arch(arch: &str) -> Result<&'static str, AppError> {
    match arch {
        "x86_64" | "amd64" => Ok("amd64"),
        "aarch64" | "arm64" => Ok("arm64"),
        other => Err(AppError::internal(format!(
            "负载均衡离线镜像不支持当前 CPU 架构：{other}"
        ))),
    }
}

fn verify_image_checksum(archive: &Path, sums: &Path) -> Result<(), AppError> {
    let filename = archive
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AppError::internal("负载均衡镜像文件名不是有效 UTF-8"))?;
    let sum_text = std::fs::read_to_string(sums).map_err(|error| {
        AppError::internal(format!("读取镜像校验清单 {} 失败：{error}", sums.display()))
    })?;
    let expected = sum_text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?, fields.next()?.trim_start_matches('*')))
        })
        .find_map(|(sum, name)| (name == filename).then_some(sum))
        .ok_or_else(|| AppError::internal(format!("SHA256SUMS 中未找到 {filename}")))?;

    let mut file = File::open(archive).map_err(|error| {
        AppError::internal(format!("打开离线镜像 {} 失败：{error}", archive.display()))
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            AppError::internal(format!("校验离线镜像 {} 失败：{error}", archive.display()))
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(AppError::internal(format!(
            "离线镜像 {filename} SHA-256 校验失败：期望 {expected}，实际 {actual}"
        )));
    }
    Ok(())
}

fn ensure_loadbalance_log_rotation() -> Result<(), AppError> {
    const LOGROTATE_PATH: &str = "/etc/logrotate.d/cangling-loadbalance";
    const SERVICE_PATH: &str = "/etc/systemd/system/cangling-loadbalance-logrotate.service";
    const TIMER_PATH: &str = "/etc/systemd/system/cangling-loadbalance-logrotate.timer";
    const LOGROTATE: &str = r#"/opt/cangling/loadbalance/logs/*.log {
    size 500M
    rotate 10
    compress
    delaycompress
    dateext
    missingok
    notifempty
    create 0640 root root
    su root root
    sharedscripts
    postrotate
        /usr/bin/docker kill -s USR1 cangling-loadbalance >/dev/null 2>&1 || true
    endscript
}
"#;
    const SERVICE: &str = r#"[Unit]
Description=Rotate cangling load balancer logs
After=docker.service

[Service]
Type=oneshot
ExecStart=/usr/sbin/logrotate /etc/logrotate.d/cangling-loadbalance
Nice=10
"#;
    const TIMER: &str = r#"[Unit]
Description=Check cangling load balancer logs every five minutes

[Timer]
OnBootSec=2min
OnUnitActiveSec=5min
AccuracySec=30s
Unit=cangling-loadbalance-logrotate.service

[Install]
WantedBy=timers.target
"#;

    for (path, content) in [
        (LOGROTATE_PATH, LOGROTATE),
        (SERVICE_PATH, SERVICE),
        (TIMER_PATH, TIMER),
    ] {
        std::fs::write(path, content).map_err(|error| {
            AppError::internal(format!("写入日志轮转配置 {path} 失败：{error}"))
        })?;
    }

    let check = Command::new("/usr/sbin/logrotate")
        .args(["--debug", LOGROTATE_PATH])
        .output()
        .map_err(|error| AppError::internal(format!("检查日志轮转配置失败：{error}")))?;
    if !check.status.success() {
        return Err(AppError::internal(format!(
            "日志轮转配置无效：{}",
            String::from_utf8_lossy(&check.stderr).trim()
        )));
    }

    let reload = Command::new("systemctl")
        .arg("daemon-reload")
        .output()
        .map_err(|error| AppError::internal(format!("刷新 systemd 配置失败：{error}")))?;
    if !reload.status.success() {
        return Err(AppError::internal(format!(
            "刷新 systemd 配置失败：{}",
            String::from_utf8_lossy(&reload.stderr).trim()
        )));
    }
    let enable = Command::new("systemctl")
        .args(["enable", "--now", "cangling-loadbalance-logrotate.timer"])
        .output()
        .map_err(|error| AppError::internal(format!("启用日志轮转定时器失败：{error}")))?;
    if !enable.status.success() {
        return Err(AppError::internal(format!(
            "启用日志轮转定时器失败：{}",
            String::from_utf8_lossy(&enable.stderr).trim()
        )));
    }
    Ok(())
}

fn register_loadbalance_project(
    state: &AppState,
    directory: &str,
) -> Result<&'static str, AppError> {
    const NAME: &str = "负载均衡";
    const DESCRIPTION: &str = "集群负载均衡入口：7800 端口的 /update/ 转发到主节点 cangling-update";

    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let now = db::now_rfc3339();
    if let Some(existing) = db::list_projects(&conn)?
        .into_iter()
        .find(|project| project.name == NAME)
    {
        db::update_project(&conn, &existing.id, NAME, DESCRIPTION, directory, &now)?;
        return Ok("已更新");
    }

    db::insert_project(
        &conn,
        &Project {
            id: Uuid::new_v4().to_string(),
            name: NAME.to_string(),
            description: DESCRIPTION.to_string(),
            directory: directory.to_string(),
            created_at: now.clone(),
            updated_at: now,
            current_version_no: None,
            current_version_id: None,
            version_count: 0,
        },
    )?;
    Ok("已创建")
}

async fn init_worker(
    state: &AppState,
    wname: &str,
    waddr: &str,
    name: &str,
    k3s_url: &str,
    k3s_token: &str,
    k3s_data_dir: &str,
) {
    let token = state.cluster.token.clone().unwrap_or_default();
    let url = format!("http://{waddr}/api/cluster/init/run");
    update_step(
        state,
        wname,
        "worker",
        HOSTNAME_STEP,
        "running",
        String::new(),
        0,
    );
    let hostname_started = Instant::now();
    let hostname_request = WorkerInitRequest {
        cluster_name: name.to_string(),
        k3s_url: k3s_url.to_string(),
        k3s_token: k3s_token.to_string(),
        k3s_data_dir: k3s_data_dir.to_string(),
        hostname: wname.to_string(),
        software: Vec::new(),
    };
    let hostname_body = serde_json::to_value(&hostname_request).unwrap_or_default();
    let hostname_result = crate::cluster::http::post_json(&url, &token, &hostname_body).await;
    let (hostname_ok, hostname_output) = match hostname_result {
        Ok((status, value)) if status.is_success() => {
            match serde_json::from_value::<WorkerInitResult>(value) {
                Ok(mut response) => {
                    let output = response
                        .packages
                        .pop()
                        .map(|result| combined_output(&result))
                        .unwrap_or_else(|| "工作节点未返回主机名修改结果".to_string());
                    (response.ok, output)
                }
                Err(error) => (false, format!("解析工作节点返回失败：{error:#}")),
            }
        }
        Ok((status, value)) => (
            false,
            format!("工作节点请求失败 {status}: {}", json_error(&value)),
        ),
        Err(error) => (false, format!("{error:#}")),
    };
    update_step(
        state,
        wname,
        "worker",
        HOSTNAME_STEP,
        if hostname_ok { "ok" } else { "failed" },
        hostname_output,
        hostname_started.elapsed().as_millis() as u64,
    );
    if !hostname_ok {
        mark_worker_remaining_failed(state, wname, 0, "设置主机名失败，已停止该节点初始化".into());
        return;
    }
    for (index, package) in WORKER_SOFTWARE.iter().enumerate() {
        update_step(state, wname, "worker", package, "running", String::new(), 0);
        let req = WorkerInitRequest {
            cluster_name: name.to_string(),
            k3s_url: k3s_url.to_string(),
            k3s_token: k3s_token.to_string(),
            k3s_data_dir: k3s_data_dir.to_string(),
            hostname: String::new(),
            software: vec![package.to_string()],
        };
        let body = serde_json::to_value(&req).unwrap_or(serde_json::Value::Null);
        let result = crate::cluster::http::post_json(&url, &token, &body).await;
        let package_result = match result {
            Ok((status, value)) if status.is_success() => {
                match serde_json::from_value::<WorkerInitResult>(value) {
                    Ok(mut response) => response.packages.pop(),
                    Err(error) => {
                        mark_worker_remaining_failed(
                            state,
                            wname,
                            index,
                            format!("解析 worker 返回失败：{error:#}"),
                        );
                        return;
                    }
                }
            }
            Ok((status, value)) => {
                mark_worker_remaining_failed(
                    &state,
                    wname,
                    index,
                    format!("worker 初始化请求失败 {status}: {}", json_error(&value)),
                );
                return;
            }
            Err(error) => {
                mark_worker_remaining_failed(state, wname, index, format!("{error:#}"));
                return;
            }
        };
        let Some(result) = package_result else {
            mark_worker_remaining_failed(state, wname, index, "worker 未返回安装结果".into());
            return;
        };
        let output = combined_output(&result);
        let good = result.exit_code == Some(0) && !result.timed_out;
        update_step(
            state,
            wname,
            "worker",
            package,
            if good { "ok" } else { "failed" },
            output,
            result.elapsed_ms,
        );
    }
}

fn mark_worker_remaining_failed(state: &AppState, wname: &str, from: usize, msg: String) {
    if let Ok(mut st) = state.init.lock() {
        for s in st.steps.iter_mut() {
            if s.node == wname
                && s.role == "worker"
                && WORKER_SOFTWARE
                    .iter()
                    .skip(from)
                    .any(|package| *package == s.package)
            {
                s.state = "failed".to_string();
                s.output = msg.clone();
            }
        }
    }
}

fn update_step(
    state: &AppState,
    node: &str,
    role: &str,
    package: &str,
    new_state: &str,
    output: String,
    elapsed_ms: u64,
) {
    if let Ok(mut st) = state.init.lock() {
        for s in st.steps.iter_mut() {
            if s.node == node && s.role == role && s.package == package {
                s.state = new_state.to_string();
                s.output = output.clone();
                s.elapsed_ms = elapsed_ms;
            }
        }
    }
}

fn combined_output(r: &repo::InstallResult) -> String {
    let mut out = String::new();
    if !r.stdout.trim().is_empty() {
        out.push_str(r.stdout.trim());
    }
    if !r.stderr.trim().is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(r.stderr.trim());
    }
    out
}

fn managed_worker_hosts(state: &AppState) -> Result<Vec<BootstrapHost>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let mut stmt = conn.prepare(
        "SELECT name, host, ssh_port, username FROM managed_hosts
         WHERE kind != 'local' ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(BootstrapHost {
            name: row.get(0)?,
            host: row.get(1)?,
            port: row.get::<_, u16>(2)?,
            username: row.get(3)?,
        })
    })?;
    let mut hosts = Vec::new();
    for row in rows {
        let host = row?;
        validate_hostname(&host.name)?;
        if !hosts
            .iter()
            .any(|item: &BootstrapHost| item.host == host.host)
        {
            hosts.push(host);
        }
    }
    Ok(hosts)
}

async fn bootstrap_workers(state: &AppState) -> Result<(), AppError> {
    let hosts = managed_worker_hosts(state)?;
    let token = state
        .cluster
        .token
        .as_deref()
        .ok_or_else(|| AppError::bad("未配置集群令牌"))?
        .to_string();
    let master = format!("http://{}:5400", hostinfo::primary_ip());
    for host in hosts.clone() {
        let state = state.clone();
        let token = token.clone();
        let master = master.clone();
        let host_for_task = host.clone();
        update_step(
            &state,
            &host.name,
            "worker",
            HOSTNAME_STEP,
            "running",
            String::new(),
            0,
        );
        let started = Instant::now();
        let task_state = state.clone();
        let result = tokio::task::spawn_blocking(move || {
            bootstrap_worker_blocking(&task_state, &host_for_task, &master, &token)
        })
        .await
        .map_err(|error| AppError::internal(format!("Worker 引导任务失败：{error}")))?;
        match result {
            Ok(output) => {
                update_step(
                    &state,
                    &host.name,
                    "worker",
                    HOSTNAME_STEP,
                    "ok",
                    format!("主机名已设置为 {}", host.name),
                    started.elapsed().as_millis() as u64,
                );
                update_step(
                    &state,
                    &host.name,
                    "worker",
                    WORKER_BOOTSTRAP_STEP,
                    "ok",
                    output,
                    started.elapsed().as_millis() as u64,
                );
            }
            Err(error) => {
                update_step(
                    &state,
                    &host.name,
                    "worker",
                    HOSTNAME_STEP,
                    "failed",
                    format!("{error:#}"),
                    started.elapsed().as_millis() as u64,
                );
                update_step(
                    &state,
                    &host.name,
                    "worker",
                    WORKER_BOOTSTRAP_STEP,
                    "failed",
                    format!("{error:#}"),
                    started.elapsed().as_millis() as u64,
                );
                return Err(error);
            }
        }
    }
    for _ in 0..30 {
        let online = online_workers(state)?;
        if hosts
            .iter()
            .all(|host| online.iter().any(|(name, _)| name == &host.name))
        {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    let online = online_workers(state)?;
    let missing = hosts
        .iter()
        .filter(|host| !online.iter().any(|(name, _)| name == &host.name))
        .map(|host| host.name.as_str())
        .collect::<Vec<_>>()
        .join("、");
    Err(AppError::internal(format!(
        "Worker 安装后 30 秒内未注册到主节点：{missing}"
    )))
}

fn bootstrap_worker_blocking(
    state: &AppState,
    host: &BootstrapHost,
    master: &str,
    token: &str,
) -> Result<String, AppError> {
    let target = format!("{}@{}", host.username, host.host);
    let ssh_base = || {
        let mut command = Command::new("ssh");
        command.args([
            "-p",
            &host.port.to_string(),
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=12",
            "-o",
            "StrictHostKeyChecking=accept-new",
            &target,
        ]);
        command
    };
    let arch_output = ssh_base()
        .arg("uname -m")
        .output()
        .map_err(|error| AppError::internal(format!("连接 {} 失败：{error}", host.name)))?;
    if !arch_output.status.success() {
        return Err(AppError::internal(format!(
            "连接 {} 失败：{}",
            host.name,
            String::from_utf8_lossy(&arch_output.stderr).trim()
        )));
    }
    let arch_name = String::from_utf8_lossy(&arch_output.stdout);
    let arch = crate::binaries::Arch::parse(arch_name.trim()).ok_or_else(|| {
        AppError::bad(format!(
            "{} 的 CPU 架构不受支持：{}",
            host.name,
            arch_name.trim()
        ))
    })?;
    let slot = crate::binaries::binary_path(&state.paths.exe_dir, arch);
    let binary = if crate::binaries::elf_arch_file(&slot) == Some(arch) {
        slot.display().to_string()
    } else {
        crate::binaries::stored(&state.paths.exe_dir, arch)
            .and_then(|item| item.path)
            .ok_or_else(|| AppError::not_found(format!("缺少 {} Worker 安装程序", arch.label())))?
    };
    let remote_temp = "/tmp/cangling-update-worker.new";
    let copy = Command::new("scp")
        .args([
            "-P",
            &host.port.to_string(),
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=12",
            &binary,
            &format!("{target}:{remote_temp}"),
        ])
        .output()
        .map_err(|error| {
            AppError::internal(format!("复制 Worker 到 {} 失败：{error}", host.name))
        })?;
    if !copy.status.success() {
        return Err(AppError::internal(format!(
            "复制 Worker 到 {} 失败：{}",
            host.name,
            String::from_utf8_lossy(&copy.stderr).trim()
        )));
    }
    let script = format!(
        "set -eu; test \"$(id -u)\" = 0; hostnamectl set-hostname {}; mkdir -p /opt/cangling-update; systemctl stop cangling-update 2>/dev/null || true; install -m 0755 {} /opt/cangling-update/cangling-update; /opt/cangling-update/cangling-update --bind 0.0.0.0 --port 5400 --role worker --master {} --cluster-token {} install-service; systemctl is-active --quiet cangling-update; systemctl show -p ExecStart --value cangling-update | grep -q -- '--role worker'; /opt/cangling-update/cangling-update version",
        shell_quote(&host.name),
        shell_quote(remote_temp),
        shell_quote(master),
        shell_quote(token),
    );
    let install = ssh_base()
        .arg(script)
        .output()
        .map_err(|error| AppError::internal(format!("安装 {} Worker 失败：{error}", host.name)))?;
    if !install.status.success() {
        return Err(AppError::internal(format!(
            "安装 {} Worker 失败：{}",
            host.name,
            String::from_utf8_lossy(&install.stderr).trim()
        )));
    }
    Ok(format!(
        "已通过 SSH 安装 {} Worker，并连接到 {}",
        arch.label(),
        master
    ))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn online_workers(state: &AppState) -> Result<Vec<(String, String)>, AppError> {
    let self_id = crate::cluster::load_or_create_node_id(&state.paths);
    let self_ip = hostinfo::primary_ip().parse::<std::net::IpAddr>().ok();
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let mut stmt = conn
        .prepare(
            "SELECT cn.id, COALESCE(NULLIF(mh.name, ''), cn.name), cn.addr, cn.last_seen
             FROM cluster_nodes cn
             LEFT JOIN managed_hosts mh ON mh.cluster_node_id = cn.id
             WHERE cn.role = 'worker'",
        )
        .map_err(AppError::from)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(AppError::from)?;
    let mut out = Vec::new();
    for row in rows {
        let (id, name, addr, last_seen) = row.map_err(AppError::from)?;
        let worker_ip = worker_addr_host(&addr);
        if is_online(&last_seen) && !is_self_worker(&id, worker_ip, &self_id, self_ip) {
            out.push((name, addr));
        }
    }
    Ok(out)
}

fn managed_master_name(state: &AppState) -> Result<String, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let name = conn
        .query_row(
            "SELECT name FROM managed_hosts WHERE kind = 'local' ORDER BY updated_at DESC LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_else(|_| node_label());
    validate_hostname(&name)?;
    Ok(name)
}

fn validate_cluster_hostnames(state: &AppState) -> Result<(), AppError> {
    validate_hostname(&managed_master_name(state)?)?;
    for (name, _) in online_workers(state)? {
        validate_hostname(&name)?;
    }
    Ok(())
}

fn validate_hostname(name: &str) -> Result<(), AppError> {
    let name = name.trim();
    let valid = !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        });
    if !valid {
        return Err(AppError::bad(format!(
            "主机名称“{name}”不是有效的 Linux hostname；仅允许字母、数字、短横线和点"
        )));
    }
    Ok(())
}

fn set_system_hostname(name: &str) -> Result<String, AppError> {
    validate_hostname(name)?;
    let current = node_label();
    if current == name {
        return Ok(format!("主机名已经是 {name}"));
    }
    let output = Command::new("hostnamectl")
        .args(["set-hostname", name])
        .output()
        .map_err(|error| AppError::internal(format!("执行 hostnamectl 失败：{error}")))?;
    if !output.status.success() {
        return Err(AppError::internal(format!(
            "修改主机名失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(format!("主机名已由 {current} 修改为 {name}"))
}

fn step_result(package: &str, output: String, ok: bool) -> repo::InstallResult {
    let (stdout, stderr) = if ok {
        (output, String::new())
    } else {
        (String::new(), output)
    };
    repo::InstallResult {
        package: package.to_string(),
        installer: "hostnamectl".to_string(),
        stdout,
        stderr,
        exit_code: Some(if ok { 0 } else { 1 }),
        timed_out: false,
        elapsed_ms: 0,
    }
}

fn is_self_worker(
    id: &str,
    worker_ip: Option<std::net::IpAddr>,
    self_id: &str,
    self_ip: Option<std::net::IpAddr>,
) -> bool {
    id == self_id
        || (self_ip.is_some() && worker_ip == self_ip)
        || worker_ip.is_some_and(|ip| ip.is_loopback())
}

fn worker_addr_host(addr: &str) -> Option<std::net::IpAddr> {
    let addr = addr.trim().trim_start_matches("http://");
    addr.parse::<std::net::SocketAddr>()
        .map(|socket| socket.ip())
        .or_else(|_| addr.parse::<std::net::IpAddr>())
        .ok()
}

fn is_online(last_seen: &str) -> bool {
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(last_seen) else {
        return false;
    };
    let age = chrono::Utc::now().signed_duration_since(ts.with_timezone(&chrono::Utc));
    age.num_seconds() >= 0 && age.num_seconds() < crate::cluster::OFFLINE_AFTER_SECS as i64
}

fn node_label() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "本机".to_string())
}

fn read_file(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn json_error(value: &serde_json::Value) -> String {
    value
        .get("error")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kubeconfig_is_a_post_k3s_step() {
        assert!(!MASTER_SOFTWARE.contains(&KUBECONFIG_STEP));
        assert_eq!(KUBECONFIG_STEP, "~/.kube/config");
    }

    #[test]
    fn nfs_server_is_installed_only_on_the_master() {
        assert!(MASTER_SOFTWARE.contains(&"nfs-server"));
        assert!(!WORKER_SOFTWARE.contains(&"nfs-server"));
        assert!(MASTER_SOFTWARE.contains(&"nfs-common"));
        assert!(WORKER_SOFTWARE.contains(&"nfs-common"));
    }

    #[test]
    fn loadbalance_image_arch_uses_repository_directory_names() {
        assert_eq!(loadbalance_image_arch("x86_64").unwrap(), "amd64");
        assert_eq!(loadbalance_image_arch("amd64").unwrap(), "amd64");
        assert_eq!(loadbalance_image_arch("aarch64").unwrap(), "arm64");
        assert_eq!(loadbalance_image_arch("arm64").unwrap(), "arm64");
        assert!(loadbalance_image_arch("riscv64").is_err());
    }

    #[test]
    fn loadbalance_image_checksum_matches_named_archive() {
        let root = std::env::temp_dir().join(format!("cangling-image-sum-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let archive = root.join("image.tar.gz");
        let sums = root.join("SHA256SUMS");
        std::fs::write(&archive, b"offline image").unwrap();
        std::fs::write(
            &sums,
            "173b4e756a894d63256ab6dac256c9c9b5b761284cb9d6cfeb8ddca3143456a1  image.tar.gz\n",
        )
        .unwrap();
        assert!(verify_image_checksum(&archive, &sums).is_ok());
        std::fs::write(&archive, b"changed").unwrap();
        assert!(verify_image_checksum(&archive, &sums).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_address_parser_supports_ipv4_and_ipv6() {
        assert_eq!(
            worker_addr_host("192.168.3.121:5400"),
            Some("192.168.3.121".parse().unwrap())
        );
        assert_eq!(
            worker_addr_host("http://[::1]:5400"),
            Some("::1".parse().unwrap())
        );
    }

    #[test]
    fn stale_self_worker_is_excluded_by_id_or_address() {
        let local: std::net::IpAddr = "192.168.3.215".parse().unwrap();
        assert!(is_self_worker("same", Some(local), "same", Some(local)));
        assert!(is_self_worker(
            "old-worker-id",
            Some(local),
            "master-id",
            Some(local)
        ));
        assert!(is_self_worker(
            "other",
            Some("127.0.0.1".parse().unwrap()),
            "master-id",
            Some(local)
        ));
        assert!(!is_self_worker(
            "worker-id",
            Some("192.168.3.216".parse().unwrap()),
            "master-id",
            Some(local)
        ));
    }
}
