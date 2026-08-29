//! master 侧：接收 worker 注册与心跳、维护节点表、供控制台查询；
//! master 启动后也会把自己登记进节点表并周期刷新心跳。
//!
//! 机器间接口（register/heartbeat）用集群令牌认证；控制台接口走登录会话认证。

use crate::cluster::{
    load_or_create_node_id, HEARTBEAT_INTERVAL_SECS, OFFLINE_AFTER_SECS, TOKEN_HEADER,
};
use crate::error::AppError;
use crate::hostinfo::{self, HostSnapshot};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub version: String,
    /// 工作节点架构（`x86_64` / `aarch64`）；缺省时回退到 host.cpu.arch。
    #[serde(default)]
    pub arch: String,
    pub host: HostSnapshot,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub ok: bool,
    pub heartbeat_interval_secs: u64,
    pub master_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<crate::cluster::self_update::UpgradeOffer>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub host: Option<HostSnapshot>,
}

#[derive(Debug, Serialize)]
pub struct HeartbeatResponse {
    pub ok: bool,
    /// 是否已登记该节点；false 时 worker 应重新注册。
    pub known: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<crate::cluster::self_update::UpgradeOffer>,
}

#[derive(Debug, Deserialize)]
pub struct AuthSyncRequest {
    pub username: String,
    pub password_hash: String,
}

#[derive(Debug, Serialize)]
pub struct AuthSyncResponse {
    pub ok: bool,
    pub username: String,
    pub created: bool,
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
    pub version: String,
    #[serde(default)]
    pub binaries: Vec<crate::binaries::StoredBinary>,
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
        .route("/api/cluster/auth/sync", post(auth_sync))
        .route(
            "/api/cluster/self-update",
            get(crate::cluster::self_update::index),
        )
        .route(
            "/api/cluster/self-update/{arch}",
            get(crate::cluster::self_update::download),
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
            let snap =
                tokio::task::spawn_blocking(move || hostinfo::collect(&paths).unwrap_or_default())
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

pub async fn auth_sync(
    State(state): State<AppState>,
    Json(body): Json<AuthSyncRequest>,
) -> Result<Json<AuthSyncResponse>, AppError> {
    let username = body.username.trim().to_string();
    if username.is_empty() {
        return Err(AppError::bad("username 不能为空"));
    }
    if body.password_hash.is_empty() {
        return Err(AppError::bad("password_hash 不能为空"));
    }
    let mut created = false;
    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        match crate::db::get_user_by_name(&conn, &username)? {
            Some(user) => {
                crate::db::update_password_hash(&conn, &user.id, &body.password_hash)?;
                crate::db::delete_sessions_for_user(&conn, &user.id)?;
            }
            None => {
                crate::db::insert_user(
                    &conn,
                    &Uuid::new_v4().to_string(),
                    &username,
                    &body.password_hash,
                )?;
                created = true;
            }
        }
    }
    Ok(Json(AuthSyncResponse {
        ok: true,
        username,
        created,
    }))
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

    // 新加入/重新注册的 worker：把 master 的登录账号同步过去，保证各节点同一套账号密码。
    {
        let state = state.clone();
        let addr = body.addr.clone();
        tokio::spawn(async move {
            crate::auth::sync_users_to_worker(&state, &addr).await;
        });
    }

    let arch = worker_arch(&body.arch, Some(&body.host));
    crate::cluster::self_update::warn_if_missing(&state.paths, &body.version, &arch, &body.name);
    let upgrade = crate::cluster::self_update::offer_for(&state.paths, &body.version, &arch);
    if let Some(ref offer) = upgrade {
        tracing::info!(
            "worker {}（{}）版本 {} 低于 master {}，将下发升级包",
            body.name,
            offer.arch,
            body.version,
            offer.version
        );
    }

    Ok(Json(RegisterResponse {
        ok: true,
        heartbeat_interval_secs: HEARTBEAT_INTERVAL_SECS,
        master_version: env!("CARGO_PKG_VERSION").to_string(),
        upgrade,
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
    let (known, reconnect_addr, stored) = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let prev = node_online_state(&conn, &body.id)?;
        let stored = node_version_and_arch(&conn, &body.id)?;
        let known = touch_node(&conn, &body.id, &now, info.as_deref())?;
        // 从离线→在线的重连，需要重新同步账号（新加入的节点走 register 分支）。
        let reconnect_addr = match (&prev, known) {
            (Some((addr, false)), true) => Some(addr.clone()),
            _ => None,
        };
        (known, reconnect_addr, stored)
    };

    if let Some(addr) = reconnect_addr {
        let state = state.clone();
        tokio::spawn(async move {
            crate::auth::sync_users_to_worker(&state, &addr).await;
        });
    }

    let version = if body.version.trim().is_empty() {
        stored.0
    } else {
        body.version.clone()
    };
    let arch = {
        let from_req = worker_arch(&body.arch, body.host.as_ref());
        if from_req.is_empty() {
            stored.1
        } else {
            from_req
        }
    };
    let upgrade = if known {
        crate::cluster::self_update::offer_for(&state.paths, &version, &arch)
    } else {
        None
    };

    Ok(Json(HeartbeatResponse {
        ok: true,
        known,
        upgrade,
    }))
}

pub async fn list_nodes(State(state): State<AppState>) -> Result<Json<Vec<ClusterNode>>, AppError> {
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
    let binaries = if state.cluster.role == crate::cluster::Role::Master {
        crate::binaries::inventory(&state.paths.exe_dir)
    } else {
        Vec::new()
    };
    Ok(Json(ClusterStatus {
        name,
        role: state.cluster.role.as_str(),
        token_set: state.cluster.token.is_some(),
        master_url: state.cluster.master_url.clone(),
        discovery_port: state.cluster.discovery_port,
        node_count: nodes.len(),
        online,
        version: env!("CARGO_PKG_VERSION").to_string(),
        binaries,
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

fn worker_arch(explicit: &str, host: Option<&HostSnapshot>) -> String {
    let explicit = explicit.trim();
    if !explicit.is_empty() {
        return explicit.to_string();
    }
    host.map(|h| h.cpu.arch.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

fn node_version_and_arch(conn: &Connection, id: &str) -> rusqlite::Result<(String, String)> {
    let mut stmt = conn.prepare("SELECT version, info_json FROM cluster_nodes WHERE id = ?1")?;
    let row: Option<(String, String)> = stmt
        .query_row(params![id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(match row {
        Some((ver, info)) => {
            let arch = serde_json::from_str::<HostSnapshot>(&info)
                .ok()
                .map(|h| h.cpu.arch)
                .unwrap_or_default();
            (ver, arch)
        }
        None => (String::new(), String::new()),
    })
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

/// 读取节点上一轮是否在线：返回 Some((addr, 是否在线))；不存在则 None。
fn node_online_state(conn: &Connection, id: &str) -> rusqlite::Result<Option<(String, bool)>> {
    let mut stmt = conn.prepare("SELECT addr, last_seen FROM cluster_nodes WHERE id = ?1")?;
    let row: Option<(String, String)> = stmt
        .query_row(params![id], |r| {
            let addr: String = r.get(0)?;
            let last_seen: String = r.get(1)?;
            Ok((addr, last_seen))
        })
        .optional()?;
    Ok(row.map(|(addr, last_seen)| (addr, is_online(&last_seen))))
}

pub fn online_workers_in(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT name, addr, last_seen FROM cluster_nodes WHERE role = 'worker' ORDER BY name",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (name, addr, last_seen) = row?;
        if is_online(&last_seen) {
            out.push((name, addr));
        }
    }
    Ok(out)
}

pub fn online_workers(state: &AppState) -> Result<Vec<(String, String)>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    online_workers_in(&conn).map_err(AppError::from)
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
