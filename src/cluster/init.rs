//! 集群初始化：master 一键安装本机与各 worker 的基线软件（全部来自 `repo/cangling-repo/`）。
//!
//! master 角色：git、samba、docker、k3s-server（含 Traefik 端口 8020/8443）、k9s。
//! worker 角色：git、samba、docker、k3s-agent（携带 K3S_URL/K3S_TOKEN 加入集群）。

use crate::cluster::Role;
use crate::db;
use crate::error::AppError;
use crate::hostinfo;
use crate::k3s;
use crate::repo;
use crate::state::AppState;
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const MASTER_SOFTWARE: &[&str] = &[
    "git",
    "samba",
    "cifs-utils",
    "nfs-common",
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
const TRAEFIK_STEP: &str = "traefik-8020/8443";
const KUBECONFIG_STEP: &str = "~/.kube/config";
const K3S_TOKEN_PATH: &str = "/var/lib/rancher/k3s/server/node-token";

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
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerInitRequest {
    pub cluster_name: String,
    #[serde(default)]
    pub k3s_url: String,
    #[serde(default)]
    pub k3s_token: String,
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
    launch(state, Some(name), "init".to_string()).await
}

/// 控制台（登录态）入口：检查各节点并修复（含新加入节点），使用已保存的集群名。
pub async fn start_check(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, AppError> {
    let name = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::cluster_setting(&conn, CLUSTER_NAME_KEY)?.unwrap_or_default()
    };
    launch(state, Some(name), "check".to_string()).await
}

async fn launch(
    state: AppState,
    name: Option<String>,
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
    }

    let state2 = state.clone();
    let name = name.trim().to_string();
    tokio::spawn(async move {
        run_init(state2, name, mode).await;
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
    let mut packages = Vec::new();
    let mut ok = true;
    for pkg in &body.software {
        let envs = vec![
            (
                "CANGLING_CLUSTER_NAME".to_string(),
                body.cluster_name.clone(),
            ),
            ("K3S_URL".to_string(), body.k3s_url.clone()),
            ("K3S_TOKEN".to_string(), body.k3s_token.clone()),
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

async fn run_init(state: AppState, name: String, mode: String) {
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

    let result = run_init_inner(&state, &name).await;

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
    let me = node_label();
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

    if let Ok(workers) = online_workers(state) {
        for (wname, _addr) in workers {
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
    steps
}

async fn run_init_inner(state: &AppState, name: &str) -> Result<(), AppError> {
    let me = node_label();

    // 1) 本机（master）软件，依次安装，任一失败即中止。
    for pkg in MASTER_SOFTWARE {
        update_step(state, &me, pkg, "running", String::new(), 0);
        let envs = vec![("CANGLING_CLUSTER_NAME".to_string(), name.to_string())];
        let started = Instant::now();
        let res = repo::install_package(state, repo::host_platform(), pkg, &envs).await;
        let elapsed = started.elapsed().as_millis() as u64;
        match res {
            Ok(r) => {
                let output = combined_output(&r);
                if r.exit_code == Some(0) && !r.timed_out {
                    update_step(state, &me, pkg, "ok", output, elapsed);
                } else {
                    update_step(state, &me, pkg, "failed", output, elapsed);
                    return Err(AppError::internal(format!(
                        "{pkg} 安装失败（退出码 {:?}）",
                        r.exit_code
                    )));
                }
            }
            Err(e) => {
                update_step(state, &me, pkg, "failed", format!("{e:#}"), elapsed);
                return Err(e);
            }
        }
    }

    // 2) Traefik 端口覆盖（best-effort，k3s 未装好时仅告警）。
    update_step(state, &me, TRAEFIK_STEP, "running", String::new(), 0);
    let started = Instant::now();
    let elapsed = started.elapsed().as_millis() as u64;
    match k3s::ensure_traefik_config() {
        Ok(msg) => update_step(state, &me, TRAEFIK_STEP, "ok", msg, elapsed),
        Err(e) => update_step(
            state,
            &me,
            TRAEFIK_STEP,
            "failed",
            format!("{e:#}"),
            elapsed,
        ),
    }

    // 2b) kubectl/k9s 默认 kubeconfig：检查 /root/.kube/config，缺失则从 k3s.yaml 拷贝。
    update_step(state, &me, KUBECONFIG_STEP, "running", String::new(), 0);
    let started = Instant::now();
    match k3s::ensure_kubeconfig() {
        Ok(msg) => update_step(
            state,
            &me,
            KUBECONFIG_STEP,
            "ok",
            msg,
            started.elapsed().as_millis() as u64,
        ),
        Err(e) => update_step(
            state,
            &me,
            KUBECONFIG_STEP,
            "failed",
            format!("{e:#}"),
            started.elapsed().as_millis() as u64,
        ),
    }

    // 3) 读取 k3s 加入令牌与主节点地址，下发给 worker。
    let k3s_token = read_file(K3S_TOKEN_PATH).unwrap_or_default();
    let k3s_url = format!("https://{}:6443", hostinfo::primary_ip());

    // 4) 各 worker 依次初始化（失败不阻断其它 worker）。
    for (wname, waddr) in online_workers(state)? {
        init_worker(state, &wname, &waddr, name, &k3s_url, &k3s_token).await;
    }

    Ok(())
}

async fn init_worker(
    state: &AppState,
    wname: &str,
    waddr: &str,
    name: &str,
    k3s_url: &str,
    k3s_token: &str,
) {
    let token = state.cluster.token.clone().unwrap_or_default();
    let url = format!("http://{waddr}/api/cluster/init/run");
    let req = WorkerInitRequest {
        cluster_name: name.to_string(),
        k3s_url: k3s_url.to_string(),
        k3s_token: k3s_token.to_string(),
        software: WORKER_SOFTWARE.iter().map(|s| s.to_string()).collect(),
    };
    let body = serde_json::to_value(&req).unwrap_or(serde_json::Value::Null);

    let result = crate::cluster::http::post_json(&url, &token, &body).await;
    let packages = match result {
        Ok((status, value)) if status.is_success() => {
            match serde_json::from_value::<WorkerInitResult>(value) {
                Ok(r) => r.packages,
                Err(e) => {
                    mark_worker_failed(state, wname, format!("解析 worker 返回失败：{e:#}"));
                    return;
                }
            }
        }
        Ok((status, value)) => {
            mark_worker_failed(
                state,
                wname,
                format!("worker 初始化请求失败 {status}: {}", json_error(&value)),
            );
            return;
        }
        Err(e) => {
            mark_worker_failed(state, wname, format!("{e:#}"));
            return;
        }
    };

    for pkg in packages {
        let pkg_name = pkg.package.clone();
        let output = combined_output(&pkg);
        let good = pkg.exit_code == Some(0) && !pkg.timed_out;
        update_step(
            state,
            wname,
            &pkg_name,
            if good { "ok" } else { "failed" },
            output,
            pkg.elapsed_ms,
        );
    }
}

fn mark_worker_failed(state: &AppState, wname: &str, msg: String) {
    if let Ok(mut st) = state.init.lock() {
        for s in st.steps.iter_mut() {
            if s.node == wname {
                s.state = "failed".to_string();
                s.output = msg.clone();
            }
        }
    }
}

fn update_step(
    state: &AppState,
    node: &str,
    package: &str,
    new_state: &str,
    output: String,
    elapsed_ms: u64,
) {
    if let Ok(mut st) = state.init.lock() {
        for s in st.steps.iter_mut() {
            if s.node == node && s.package == package {
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

fn online_workers(state: &AppState) -> Result<Vec<(String, String)>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let mut stmt = conn
        .prepare("SELECT name, addr, last_seen FROM cluster_nodes WHERE role = 'worker'")
        .map_err(AppError::from)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(AppError::from)?;
    let mut out = Vec::new();
    for row in rows {
        let (name, addr, last_seen) = row.map_err(AppError::from)?;
        if is_online(&last_seen) {
            out.push((name, addr));
        }
    }
    Ok(out)
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
}
