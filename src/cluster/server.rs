//! master 侧：接收 worker 注册与心跳、维护节点表、供控制台查询；
//! master 启动后也会把自己登记进节点表并周期刷新心跳。
//!
//! 机器间接口（register/heartbeat）用集群令牌认证；控制台接口走登录会话认证。

use crate::cluster::{load_or_create_node_id, HEARTBEAT_INTERVAL_SECS, OFFLINE_AFTER_SECS, TOKEN_HEADER};
use crate::error::AppError;
use crate::hostinfo::{self, HostSnapshot};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub version: String,
    pub host: HostSnapshot,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub ok: bool,
    pub heartbeat_interval_secs: u64,
    pub master_version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub id: String,
    #[serde(default)]
    pub host: Option<HostSnapshot>,
}

#[derive(Debug, Serialize)]
pub struct HeartbeatResponse {
    pub ok: bool,
    /// 是否已登记该节点；false 时 worker 应重新注册。
    pub known: bool,
}

#[derive(Debug, Serialize)]
pub struct ClusterNode {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub version: String,
    pub role: String,
    pub status: &'static str,
    pub registered_at: String,
    pub last_seen: String,
    pub host: Option<HostSnapshot>,
}

#[derive(Debug, Serialize)]
pub struct ClusterStatus {
    pub name: String,
    pub role: &'static str,
    pub token_set: bool,
    pub master_url: Option<String>,
    pub discovery_port: u16,
    pub node_count: usize,
    pub online: usize,
}

/// 机器间接口的令牌中间件（配合外层 auth::require_auth 放行公共路径）。
pub async fn require_cluster_token(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, AppError> {
    let Some(expected) = state.cluster.token.as_deref() else {
        return Err(AppError::unauthorized("集群未启用或未设置令牌"));
    };
    let provided = request
        .headers()
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok());
    if provided != Some(expected) {
        return Err(AppError::unauthorized("集群令牌无效"));
    }
    Ok(next.run(request).await)
}

pub fn m2m_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/api/cluster/register", post(register))
        .route("/api/cluster/heartbeat", post(heartbeat))
        .route("/api/cluster/repo", get(repo_index))
        .route(
            "/api/cluster/repo/{tab}/{package}/download",
            get(repo_download),
        )
        .route(
            "/api/cluster/init/run",
            post(crate::cluster::init::run_worker_init),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_cluster_token,
        ))
}

/// master 自身也登记进节点表：启动时写入本机信息，之后每 15 秒刷新心跳、
/// 每 5 分钟重新采集一次完整主机信息，保证自己在节点列表里保持在线。
pub async fn maintain_self_node(state: AppState) {
    const SNAPSHOT_REFRESH: Duration = Duration::from_secs(300);
    let node_id = load_or_create_node_id(&state.paths);
    let mut last_snapshot: Option<Instant> = None;

    loop {
        let want_snapshot = last_snapshot
            .map(|t| t.elapsed() >= SNAPSHOT_REFRESH)
            .unwrap_or(true);

        if want_snapshot {
            let paths = state.paths.clone();
            let snap = tokio::task::spawn_blocking(move || {
                hostinfo::collect(&paths).unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            let now = chrono::Utc::now().to_rfc3339();
            let addr = format!("{}:{}", snap.primary_ip, state.port);
            let info = serde_json::to_string(&snap).unwrap_or_else(|_| "{}".into());
            if let Ok(conn) = state.db.lock() {
                if let Err(err) = upsert_node(
                    &conn,
                    &node_id,
                    &snap.hostname,
                    &addr,
                    env!("CARGO_PKG_VERSION"),
                    "master",
                    &info,
                    &now,
                ) {
                    tracing::warn!("写入本机节点信息失败：{err}");
                }
            }
            last_snapshot = Some(Instant::now());
        } else {
            let now = chrono::Utc::now().to_rfc3339();
            if let Ok(conn) = state.db.lock() {
                if let Err(err) = touch_node(&conn, &node_id, &now, None) {
                    tracing::warn!("刷新本机节点心跳失败：{err}");
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)).await;
    }
}

pub async fn register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, AppError> {
    if body.id.trim().is_empty() {
        return Err(AppError::bad("节点 id 不能为空"));
    }
    let now = chrono::Utc::now().to_rfc3339();
    let info = serde_json::to_string(&body.host).unwrap_or_else(|_| "{}".into());
    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        upsert_node(
            &conn,
            &body.id,
            &body.name,
            &body.addr,
            &body.version,
            "worker",
            &info,
            &now,
        )?;
    }
    Ok(Json(RegisterResponse {
        ok: true,
        heartbeat_interval_secs: HEARTBEAT_INTERVAL_SECS,
        master_version: env!("CARGO_PKG_VERSION").to_string(),
    }))
}

pub async fn heartbeat(
    State(state): State<AppState>,
    Json(body): Json<HeartbeatRequest>,
) -> Result<Json<HeartbeatResponse>, AppError> {
    let now = chrono::Utc::now().to_rfc3339();
    let info = body
        .host
        .as_ref()
        .and_then(|h| serde_json::to_string(h).ok());
    let known = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        touch_node(&conn, &body.id, &now, info.as_deref())?
    };
    Ok(Json(HeartbeatResponse { ok: true, known }))
}

pub async fn list_nodes(
    State(state): State<AppState>,
) -> Result<Json<Vec<ClusterNode>>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    Ok(Json(list_cluster_nodes(&conn)?))
}

pub async fn cluster_status(
    State(state): State<AppState>,
) -> Result<Json<ClusterStatus>, AppError> {
    let (nodes, name) = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let nodes = list_cluster_nodes(&conn)?;
        let name = crate::db::cluster_setting(&conn, crate::cluster::init::CLUSTER_NAME_KEY)?
            .unwrap_or_default();
        (nodes, name)
    };
    let online = nodes.iter().filter(|n| n.status == "online").count();
    Ok(Json(ClusterStatus {
        name,
        role: state.cluster.role.as_str(),
        token_set: state.cluster.token.is_some(),
        master_url: state.cluster.master_url.clone(),
        discovery_port: state.cluster.discovery_port,
        node_count: nodes.len(),
        online,
    }))
}

/// 机器间仓库清单（master 本地扫描）。
pub async fn repo_index(
    State(state): State<AppState>,
) -> Result<Json<crate::repo::RepoIndex>, AppError> {
    let paths = state.paths.clone();
    let idx = tokio::task::spawn_blocking(move || crate::repo::scan_index(&paths))
        .await
        .map_err(|e| AppError::internal(e.to_string()))?;
    Ok(Json(idx))
}

/// 机器间仓库下载（返回软件包 tar.gz）。
pub async fn repo_download(
    State(state): State<AppState>,
    Path((tab, package)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let root = crate::repo::repo_root(&state.paths);
    let t = tab.clone();
    let p = package.clone();
    let bytes = tokio::task::spawn_blocking(move || crate::repo::build_tarball(&root, &t, &p))
        .await
        .map_err(|e| AppError::internal(e.to_string()))??;
    Ok(crate::repo::tarball_response(&package, bytes))
}

fn upsert_node(
    conn: &Connection,
    id: &str,
    name: &str,
    addr: &str,
    version: &str,
    role: &str,
    info_json: &str,
    now: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO cluster_nodes
            (id, name, addr, version, role, info_json, registered_at, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
         ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            addr = excluded.addr,
            version = excluded.version,
            role = excluded.role,
            info_json = excluded.info_json,
            last_seen = excluded.last_seen",
        params![id, name, addr, version, role, info_json, now],
    )?;
    Ok(())
}

fn touch_node(
    conn: &Connection,
    id: &str,
    now: &str,
    info_json: Option<&str>,
) -> rusqlite::Result<bool> {
    let changed = match info_json {
        Some(info) => conn.execute(
            "UPDATE cluster_nodes SET last_seen = ?2, info_json = ?3 WHERE id = ?1",
            params![id, now, info],
        )?,
        None => conn.execute(
            "UPDATE cluster_nodes SET last_seen = ?2 WHERE id = ?1",
            params![id, now],
        )?,
    };
    Ok(changed > 0)
}

fn list_cluster_nodes(conn: &Connection) -> rusqlite::Result<Vec<ClusterNode>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, addr, version, role, info_json, registered_at, last_seen
         FROM cluster_nodes ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (id, name, addr, version, role, info_json, registered_at, last_seen) = row?;
        let status = if is_online(&last_seen) {
            "online"
        } else {
            "offline"
        };
        let host = serde_json::from_str::<HostSnapshot>(&info_json).ok();
        out.push(ClusterNode {
            id,
            name,
            addr,
            version,
            role,
            status,
            registered_at,
            last_seen,
            host,
        });
    }
    Ok(out)
}

fn is_online(last_seen: &str) -> bool {
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(last_seen) else {
        return false;
    };
    let age = chrono::Utc::now().signed_duration_since(ts.with_timezone(&chrono::Utc));
    age.num_seconds() >= 0 && age.num_seconds() < OFFLINE_AFTER_SECS as i64
}
