//! 存储模块：定义外部存储（CIFS/NFS）与主机 CIFS 共享，并可挂载外部存储、
//! 在目标主机上启动共享。
//!
//! - 外部存储：`mount -t cifs/nfs` 挂载到本机，并尽量写入 /etc/fstab。
//! - 主机 CIFS 共享：在集群主机（或本机）上写入 samba 共享片段并重启 smbd。

use crate::cluster;
use crate::error::AppError;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::Json;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path as FsPath, PathBuf};
use std::process::{Command, Stdio};
use uuid::Uuid;

pub const KIND_EXTERNAL: &str = "external";
pub const KIND_HOST_SHARE: &str = "host_share";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Storage {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub protocol: String,
    pub host_id: String,
    pub host_name: String,
    pub server: String,
    pub share: String,
    pub path: String,
    pub target_dir: String,
    pub username: String,
    #[serde(skip_serializing)]
    pub password: String,
    pub options: String,
    pub status: String,
    pub message: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Deserialize)]
pub struct StorageBody {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub host_id: String,
    #[serde(default)]
    pub host_name: String,
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub share: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub target_dir: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub options: String,
}

/// master→worker 的共享启动请求（走集群令牌）。
#[derive(Debug, Deserialize)]
pub struct ClusterShareRequest {
    pub id: String,
    pub name: String,
    pub share: String,
    pub path: String,
    #[serde(default)]
    pub options: String,
}

/// master→worker 的挂载请求（走集群令牌）。
#[derive(Debug, Deserialize)]
pub struct ClusterMountRequest {
    pub protocol: String,
    pub server: String,
    pub share: String,
    pub target_dir: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub options: String,
}

/// 控制台部署请求：指定一个或多个目标主机（空字符串 = 本机）。
#[derive(Debug, Deserialize)]
pub struct DeployBody {
    #[serde(default)]
    pub host_ids: Vec<String>,
}

/// master→worker 的卸载请求。
#[derive(Debug, Deserialize)]
pub struct ClusterUnmountRequest {
    pub target_dir: String,
}

// ---------------------------------------------------------------------------
// HTTP 处理器（控制台，登录会话）
// ---------------------------------------------------------------------------

pub async fn list_storages(
    State(state): State<AppState>,
) -> Result<Json<Vec<Storage>>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let mut list = list_storages_db(&conn)?;
    refresh_external_status(&mut list);
    Ok(Json(list))
}

pub async fn get_storage(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Storage>, AppError> {
    Ok(Json(load_storage(&state, &id)?))
}

pub async fn create_storage(
    State(state): State<AppState>,
    Json(body): Json<StorageBody>,
) -> Result<Json<Storage>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let s = normalize(&body, None, &conn)?;
    if storage_name_exists(&conn, &s.name, None)? {
        return Err(AppError::conflict("存储名称已存在"));
    }
    insert_storage_db(&conn, &s)?;
    Ok(Json(s))
}

pub async fn update_storage(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<StorageBody>,
) -> Result<Json<Storage>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let existing =
        get_storage_db(&conn, &id)?.ok_or_else(|| AppError::not_found("存储不存在"))?;
    let s = normalize(&body, Some(&existing), &conn)?;
    if storage_name_exists(&conn, &s.name, Some(&id))? {
        return Err(AppError::conflict("存储名称已存在"));
    }
    update_storage_db(&conn, &s)?;
    Ok(Json(s))
}

pub async fn delete_storage(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    if !delete_storage_db(&conn, &id)? {
        return Err(AppError::not_found("存储不存在"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn deploy_storage(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DeployBody>,
) -> Result<Json<Storage>, AppError> {
    let mut s = load_storage(&state, &id)?;
    let local_id = cluster::load_or_create_node_id(&state.paths);
    let hosts = normalize_hosts(body.host_ids);

    let mut lines = Vec::new();
    let mut failed = false;
    for host in hosts {
        let label = host_label(&state, &host);
        let result = if s.kind == KIND_EXTERNAL {
            deploy_external(&state, &s, &host, &local_id).await
        } else {
            deploy_host_share(&state, &s, &host, &local_id).await
        };
        match result {
            Ok(msg) => lines.push(format!("[{label}] {msg}")),
            Err(e) => {
                failed = true;
                lines.push(format!("[{label}] 失败：{e}"));
            }
        }
    }

    s.status = if failed {
        "error".to_string()
    } else {
        "deployed".to_string()
    };
    s.message = lines.join("；");
    s.updated_at = crate::db::now_rfc3339();
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    update_storage_db(&conn, &s)?;
    Ok(Json(s))
}

pub async fn unmount_storage(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DeployBody>,
) -> Result<Json<Storage>, AppError> {
    let mut s = load_storage(&state, &id)?;
    let local_id = cluster::load_or_create_node_id(&state.paths);
    let hosts = normalize_hosts(body.host_ids);

    let mut lines = Vec::new();
    let mut failed = false;
    for host in hosts {
        let label = host_label(&state, &host);
        let result = if host.is_empty() || host == local_id {
            let s2 = s.clone();
            run_blocking(move || unmount_external(&s2)).await
        } else {
            forward_unmount(&state, &s.target_dir, &host).await
        };
        match result {
            Ok(msg) => lines.push(format!("[{label}] {msg}")),
            Err(e) => {
                failed = true;
                lines.push(format!("[{label}] 失败：{e}"));
            }
        }
    }

    s.status = if failed {
        "error".to_string()
    } else {
        "defined".to_string()
    };
    s.message = lines.join("；");
    s.updated_at = crate::db::now_rfc3339();
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    update_storage_db(&conn, &s)?;
    Ok(Json(s))
}

pub async fn start_share(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Storage>, AppError> {
    let mut s = load_storage(&state, &id)?;
    if s.kind != KIND_HOST_SHARE {
        return Err(AppError::bad("只有「主机 CIFS 共享」需要启动共享"));
    }

    let local_id = cluster::load_or_create_node_id(&state.paths);
    let result = if s.host_id.is_empty() || s.host_id == local_id {
        let s2 = s.clone();
        run_blocking(move || start_host_share_local(&s2)).await
    } else {
        forward_start_share(&state, &s).await
    };

    set_storage_result(&state, &mut s, result, "shared").await?;
    Ok(Json(s))
}

/// worker 侧（机器间接口）：在本机启动一个 samba 共享。
pub async fn cluster_start_share(
    Json(body): Json<ClusterShareRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let s = Storage {
        id: body.id,
        name: body.name,
        kind: KIND_HOST_SHARE.into(),
        protocol: "cifs".into(),
        host_id: String::new(),
        host_name: "本机".into(),
        server: String::new(),
        share: body.share,
        path: body.path,
        target_dir: String::new(),
        username: String::new(),
        password: String::new(),
        options: body.options,
        status: String::new(),
        message: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
    };
    let msg = run_blocking(move || start_host_share_local(&s)).await?;
    Ok(Json(serde_json::json!({ "ok": true, "message": msg })))
}

/// worker 侧（机器间接口）：在本机执行一次挂载。
pub async fn cluster_mount(
    Json(body): Json<ClusterMountRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let msg = run_blocking(move || mount_from_request(&body)).await?;
    Ok(Json(serde_json::json!({ "ok": true, "message": msg })))
}

/// worker 侧（机器间接口）：在本机执行一次卸载。
pub async fn cluster_unmount(
    Json(body): Json<ClusterUnmountRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let msg = run_blocking(move || unmount_dir(&body.target_dir)).await?;
    Ok(Json(serde_json::json!({ "ok": true, "message": msg })))
}

// ---------------------------------------------------------------------------
// 数据库
// ---------------------------------------------------------------------------

const STORAGE_COLS: &str = "id, name, kind, protocol, host_id, host_name, server, share, \
    path, target_dir, username, password, options, status, message, created_at, updated_at";

fn map_storage(row: &rusqlite::Row<'_>) -> rusqlite::Result<Storage> {
    Ok(Storage {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        protocol: row.get(3)?,
        host_id: row.get(4)?,
        host_name: row.get(5)?,
        server: row.get(6)?,
        share: row.get(7)?,
        path: row.get(8)?,
        target_dir: row.get(9)?,
        username: row.get(10)?,
        password: row.get(11)?,
        options: row.get(12)?,
        status: row.get(13)?,
        message: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

pub fn list_storages_db(conn: &Connection) -> rusqlite::Result<Vec<Storage>> {
    let mut stmt =
        conn.prepare(&format!("SELECT {STORAGE_COLS} FROM storages ORDER BY updated_at DESC"))?;
    let rows = stmt.query_map([], map_storage)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
}

pub fn get_storage_db(conn: &Connection, id: &str) -> rusqlite::Result<Option<Storage>> {
    let mut stmt = conn.prepare(&format!("SELECT {STORAGE_COLS} FROM storages WHERE id = ?1"))?;
    stmt.query_row(params![id], map_storage).optional()
}

pub fn insert_storage_db(conn: &Connection, s: &Storage) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO storages
            (id, name, kind, protocol, host_id, host_name, server, share, path, target_dir,
             username, password, options, status, message, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            s.id,
            s.name,
            s.kind,
            s.protocol,
            s.host_id,
            s.host_name,
            s.server,
            s.share,
            s.path,
            s.target_dir,
            s.username,
            s.password,
            s.options,
            s.status,
            s.message,
            s.created_at,
            s.updated_at,
        ],
    )?;
    Ok(())
}

pub fn update_storage_db(conn: &Connection, s: &Storage) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE storages SET
            name = ?1, kind = ?2, protocol = ?3, host_id = ?4, host_name = ?5,
            server = ?6, share = ?7, path = ?8, target_dir = ?9, username = ?10,
            password = ?11, options = ?12, status = ?13, message = ?14, updated_at = ?15
         WHERE id = ?16",
        params![
            s.name,
            s.kind,
            s.protocol,
            s.host_id,
            s.host_name,
            s.server,
            s.share,
            s.path,
            s.target_dir,
            s.username,
            s.password,
            s.options,
            s.status,
            s.message,
            s.updated_at,
            s.id,
        ],
    )?;
    Ok(n > 0)
}

pub fn delete_storage_db(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    let n = conn.execute("DELETE FROM storages WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

pub fn storage_name_exists(
    conn: &Connection,
    name: &str,
    exclude: Option<&str>,
) -> rusqlite::Result<bool> {
    let count: i64 = match exclude {
        Some(id) => conn.query_row(
            "SELECT COUNT(*) FROM storages WHERE name = ?1 AND id != ?2",
            params![name, id],
            |r| r.get(0),
        )?,
        None => conn.query_row(
            "SELECT COUNT(*) FROM storages WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )?,
    };
    Ok(count > 0)
}

pub fn node_addr(conn: &Connection, id: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT addr FROM cluster_nodes WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )
    .optional()
}

pub fn node_name(conn: &Connection, id: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT name FROM cluster_nodes WHERE id = ?1",
        params![id],
        |r| r.get(0),
    )
    .optional()
}

// ---------------------------------------------------------------------------
// 校验与辅助
// ---------------------------------------------------------------------------

fn normalize(
    body: &StorageBody,
    existing: Option<&Storage>,
    conn: &Connection,
) -> Result<Storage, AppError> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::bad("存储名称不能为空"));
    }
    let kind = body.kind.trim().to_string();
    if kind != KIND_EXTERNAL && kind != KIND_HOST_SHARE {
        return Err(AppError::bad("存储类型无效"));
    }

    let password = match existing {
        Some(e) if body.password.is_empty() => e.password.clone(),
        _ => body.password.clone(),
    };

    let mut s = Storage {
        id: existing
            .map(|e| e.id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        name,
        kind: kind.clone(),
        protocol: body.protocol.trim().to_string(),
        host_id: body.host_id.trim().to_string(),
        host_name: body.host_name.trim().to_string(),
        server: body.server.trim().to_string(),
        share: body.share.trim().to_string(),
        path: body.path.trim().to_string(),
        target_dir: body.target_dir.trim().to_string(),
        username: body.username.trim().to_string(),
        password,
        options: body.options.trim().to_string(),
        status: existing
            .map(|e| e.status.clone())
            .unwrap_or_else(|| "defined".into()),
        message: existing.map(|e| e.message.clone()).unwrap_or_default(),
        created_at: existing
            .map(|e| e.created_at.clone())
            .unwrap_or_else(crate::db::now_rfc3339),
        updated_at: crate::db::now_rfc3339(),
    };

    if kind == KIND_EXTERNAL {
        if s.protocol != "cifs" && s.protocol != "nfs" {
            return Err(AppError::bad("外部存储协议必须是 cifs 或 nfs"));
        }
        if s.server.is_empty() || s.share.is_empty() {
            return Err(AppError::bad("请填写服务器地址与共享路径"));
        }
        if s.target_dir.is_empty() {
            return Err(AppError::bad("请填写目标目录（挂载点）"));
        }
        s.path = String::new();
        s.host_id = String::new();
        s.host_name = "本机".into();
    } else {
        s.protocol = "cifs".into();
        s.server = String::new();
        if s.share.is_empty() {
            return Err(AppError::bad("请填写共享名称"));
        }
        if s.path.is_empty() {
            return Err(AppError::bad("请填写要共享的主机目录"));
        }
        if s.target_dir.is_empty() {
            return Err(AppError::bad("请填写目标目录（挂载点）"));
        }
        if !s.username.is_empty() && s.password.is_empty() {
            return Err(AppError::bad("填写了用户名时，密码不能为空"));
        }
        if s.host_id.is_empty() {
            s.host_name = "本机".into();
        } else if s.host_name.is_empty() {
            s.host_name = node_name(conn, &s.host_id)?.unwrap_or_else(|| s.host_id.clone());
        }
    }

    Ok(s)
}

fn load_storage(state: &AppState, id: &str) -> Result<Storage, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    get_storage_db(&conn, id)?.ok_or_else(|| AppError::not_found("存储不存在"))
}

async fn set_storage_result(
    state: &AppState,
    s: &mut Storage,
    result: Result<String, AppError>,
    ok_status: &str,
) -> Result<(), AppError> {
    match result {
        Ok(msg) => {
            s.status = ok_status.to_string();
            s.message = msg;
        }
        Err(e) => {
            s.status = "error".to_string();
            s.message = e.to_string();
        }
    }
    s.updated_at = crate::db::now_rfc3339();
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    update_storage_db(&conn, s)?;
    Ok(())
}

async fn run_blocking<F, T>(f: F) -> Result<T, AppError>
where
    F: FnOnce() -> Result<T, AppError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppError::internal(format!("任务执行失败：{e}")))?
}

async fn forward_start_share(state: &AppState, s: &Storage) -> Result<String, AppError> {
    let addr = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        node_addr(&conn, &s.host_id)?
            .ok_or_else(|| AppError::bad("目标主机不在集群节点列表中"))?
    };
    let token = state
        .cluster
        .token
        .clone()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AppError::bad("集群未启用或未设置令牌，无法在远程主机上启动共享"))?;

    let url = format!("http://{addr}/api/cluster/storage/start-share");
    let body = serde_json::json!({
        "id": s.id,
        "name": s.name,
        "share": s.share,
        "path": s.path,
        "options": s.options,
    });
    let (status, value) = cluster::http::post_json(&url, &token, &body)
        .await
        .map_err(|e| AppError::internal(format!("请求目标主机失败：{e}")))?;

    if status.is_success() {
        Ok(value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("已启动共享")
            .to_string())
    } else {
        let msg = value
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("启动共享失败");
        Err(AppError::internal(msg.to_string()))
    }
}

async fn deploy_external(
    state: &AppState,
    s: &Storage,
    target_host: &str,
    local_id: &str,
) -> Result<String, AppError> {
    if target_host.is_empty() || target_host == local_id {
        let s2 = s.clone();
        run_blocking(move || mount_external(&s2)).await
    } else {
        let req = ClusterMountRequest {
            protocol: s.protocol.clone(),
            server: s.server.clone(),
            share: s.share.clone(),
            target_dir: s.target_dir.clone(),
            username: s.username.clone(),
            password: s.password.clone(),
            options: s.options.clone(),
        };
        forward_mount(state, &req, target_host).await
    }
}

async fn deploy_host_share(
    state: &AppState,
    s: &Storage,
    target_host: &str,
    local_id: &str,
) -> Result<String, AppError> {
    let owning = s.host_id.trim().to_string();
    let owning_resolved = if owning.is_empty() {
        local_id.to_string()
    } else {
        owning.clone()
    };
    let target_resolved = if target_host.is_empty() {
        local_id.to_string()
    } else {
        target_host.to_string()
    };
    let owning_is_local = owning_resolved == local_id;
    let target_is_owning = target_resolved == owning_resolved;

    let start_result = if owning_is_local {
        let s2 = s.clone();
        run_blocking(move || start_host_share_local(&s2)).await
    } else {
        forward_start_share(state, s).await
    };

    // 部署到源主机本身，只需启动共享。
    if target_is_owning {
        return start_result;
    }

    // 部署到其它主机：先确保源主机共享已启动，再挂载。
    start_result.map_err(|e| AppError::internal(format!("源主机共享启动失败：{e}")))?;

    let owning_ip = if owning_is_local {
        crate::hostinfo::primary_ip()
    } else {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        node_primary_ip(&conn, &owning)?.ok_or_else(|| AppError::bad("无法获取源主机 IP"))?
    };

    let req = ClusterMountRequest {
        protocol: "cifs".into(),
        server: owning_ip,
        share: s.share.clone(),
        target_dir: s.target_dir.clone(),
        username: s.username.clone(),
        password: s.password.clone(),
        options: s.options.clone(),
    };

    if target_host.is_empty() || target_host == local_id {
        run_blocking(move || mount_from_request(&req)).await
    } else {
        forward_mount(state, &req, target_host).await
    }
}

fn mount_from_request(req: &ClusterMountRequest) -> Result<String, AppError> {
    mount_from_spec(
        &req.protocol,
        &req.server,
        &req.share,
        &req.target_dir,
        &req.username,
        &req.password,
        &req.options,
    )
}

async fn forward_mount(
    state: &AppState,
    req: &ClusterMountRequest,
    target_host: &str,
) -> Result<String, AppError> {
    let addr = node_addr_for(state, target_host)?;
    let token = cluster_token(state)?;
    let url = format!("http://{addr}/api/cluster/storage/mount");
    let body = serde_json::json!({
        "protocol": req.protocol,
        "server": req.server,
        "share": req.share,
        "target_dir": req.target_dir,
        "username": req.username,
        "password": req.password,
        "options": req.options,
    });
    let (status, value) = cluster::http::post_json(&url, &token, &body)
        .await
        .map_err(|e| AppError::internal(format!("请求目标主机失败：{e}")))?;
    if status.is_success() {
        Ok(value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("已挂载")
            .to_string())
    } else {
        let msg = value
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("挂载失败");
        Err(AppError::internal(msg.to_string()))
    }
}

async fn forward_unmount(
    state: &AppState,
    target_dir: &str,
    target_host: &str,
) -> Result<String, AppError> {
    let addr = node_addr_for(state, target_host)?;
    let token = cluster_token(state)?;
    let url = format!("http://{addr}/api/cluster/storage/unmount");
    let body = serde_json::json!({ "target_dir": target_dir });
    let (status, value) = cluster::http::post_json(&url, &token, &body)
        .await
        .map_err(|e| AppError::internal(format!("请求目标主机失败：{e}")))?;
    if status.is_success() {
        Ok(value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("已卸载")
            .to_string())
    } else {
        let msg = value
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("卸载失败");
        Err(AppError::internal(msg.to_string()))
    }
}

fn node_addr_for(state: &AppState, host_id: &str) -> Result<String, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    node_addr(&conn, host_id)?.ok_or_else(|| AppError::bad("目标主机不在集群节点列表中"))
}

fn cluster_token(state: &AppState) -> Result<String, AppError> {
    state
        .cluster
        .token
        .clone()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AppError::bad("集群未启用或未设置令牌，无法在远程主机上执行操作"))
}

fn node_primary_ip(conn: &Connection, id: &str) -> rusqlite::Result<Option<String>> {
    let info: Option<String> = conn
        .query_row(
            "SELECT info_json FROM cluster_nodes WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(info.and_then(|s| {
        serde_json::from_str::<crate::hostinfo::HostSnapshot>(&s)
            .ok()
            .map(|h| h.primary_ip)
    }))
}

fn normalize_hosts(host_ids: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in host_ids {
        let h = raw.trim().to_string();
        if h.is_empty() {
            if !out.iter().any(|x| x.is_empty()) {
                out.push(String::new());
            }
        } else if !out.contains(&h) {
            out.push(h);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn host_label(state: &AppState, host_id: &str) -> String {
    if host_id.is_empty() {
        return "本机".to_string();
    }
    if let Ok(conn) = state.db.lock() {
        if let Ok(Some(name)) = node_name(&conn, host_id) {
            if !name.is_empty() {
                return name;
            }
        }
    }
    host_id.to_string()
}

fn refresh_external_status(list: &mut [Storage]) {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    for s in list.iter_mut() {
        if s.kind == KIND_EXTERNAL && s.status != "error" {
            let mnt = s.target_dir.trim();
            let mounted = !mnt.is_empty()
                && mounts
                    .lines()
                    .any(|l| l.split_whitespace().nth(1) == Some(mnt));
            s.status = if mounted { "mounted" } else { "defined" }.into();
        }
    }
}

// ---------------------------------------------------------------------------
// 外部存储：挂载 / 卸载
// ---------------------------------------------------------------------------

fn mount_external(s: &Storage) -> Result<String, AppError> {
    mount_from_spec(
        &s.protocol,
        &s.server,
        &s.share,
        &s.target_dir,
        &s.username,
        &s.password,
        &s.options,
    )
}

fn mount_from_spec(
    protocol: &str,
    server: &str,
    share: &str,
    target_dir: &str,
    username: &str,
    password: &str,
    options: &str,
) -> Result<String, AppError> {
    let mnt = target_dir.trim();
    if mnt.is_empty() {
        return Err(AppError::bad("请填写目标目录（挂载点）"));
    }
    std::fs::create_dir_all(mnt).map_err(|e| AppError::internal(format!("创建挂载点失败：{e}")))?;
    if is_mounted(mnt) {
        return Ok(format!("已挂载（无需重复挂载）：{mnt}"));
    }

    let (fstype, src, opts) = build_mount_spec_parts(protocol, server, share, username, password, options)?;
    ensure_mount_helper(&fstype)?;
    let mut cmd = Command::new("mount");
    cmd.arg("-t").arg(&fstype);
    if !opts.is_empty() {
        cmd.arg("-o").arg(&opts);
    }
    cmd.arg(&src).arg(mnt);
    let out = cmd
        .output()
        .map_err(|e| AppError::internal(format!("执行 mount 失败：{e}")))?;
    if !out.status.success() {
        let msg = cmd_message(&out);
        return Err(AppError::internal(if msg.is_empty() {
            "挂载失败".to_string()
        } else {
            format!("挂载失败：{msg}")
        }));
    }

    let mut note = format!("已挂载 {src} → {mnt}");
    match persist_fstab(&fstype, &src, mnt, &opts) {
        Ok(true) => note.push_str("，并已写入 /etc/fstab"),
        Ok(false) => note.push_str("（/etc/fstab 已有该挂载点条目）"),
        Err(e) => note.push_str(&format!("；但写入 /etc/fstab 失败：{e}")),
    }
    Ok(note)
}

fn unmount_external(s: &Storage) -> Result<String, AppError> {
    unmount_dir(&s.target_dir)
}

fn unmount_dir(target_dir: &str) -> Result<String, AppError> {
    let mnt = target_dir.trim();
    if mnt.is_empty() {
        return Err(AppError::bad("目标目录（挂载点）为空"));
    }
    let mounted = is_mounted(mnt);
    if mounted {
        let out = Command::new("umount")
            .arg(mnt)
            .output()
            .map_err(|e| AppError::internal(format!("执行 umount 失败：{e}")))?;
        if !out.status.success() {
            let msg = cmd_message(&out);
            return Err(AppError::internal(if msg.is_empty() {
                "卸载失败".to_string()
            } else {
                format!("卸载失败：{msg}")
            }));
        }
    }

    let mut note = if mounted {
        "已卸载".to_string()
    } else {
        "该挂载点未挂载".to_string()
    };
    match remove_fstab_entry(mnt) {
        Ok(true) => note.push_str("，并已从 /etc/fstab 移除"),
        Ok(false) => {}
        Err(e) => note.push_str(&format!("；但清理 /etc/fstab 失败：{e}")),
    }
    Ok(note)
}

fn build_mount_spec_parts(
    protocol: &str,
    server: &str,
    share: &str,
    username: &str,
    password: &str,
    options: &str,
) -> Result<(String, String, String), AppError> {
    match protocol.trim() {
        "nfs" => {
            let server = server.trim();
            let share = share.trim();
            if server.is_empty() || share.is_empty() {
                return Err(AppError::bad("请填写 NFS 服务器与共享路径"));
            }
            let src = format!("{}:{}", server, share.trim_start_matches(':'));
            Ok(("nfs".to_string(), src, options.trim().to_string()))
        }
        _ => {
            let server = server.trim().trim_matches('/');
            let share = share.trim().trim_matches('/');
            if server.is_empty() || share.is_empty() {
                return Err(AppError::bad("请填写 CIFS 服务器与共享名称"));
            }
            let src = format!("//{server}/{share}");
            let mut opts = options.trim().to_string();
            if username.is_empty() {
                // 未设置用户名/密码时按匿名 guest 挂载（自动带入 guest 选项）。
                if !opts.is_empty() {
                    opts.push(',');
                }
                opts.push_str("guest");
            } else {
                // 设置了用户名/密码时自动带入挂载凭据。
                if !opts.is_empty() {
                    opts.push(',');
                }
                opts.push_str(&format!("username={}", username));
                if !password.is_empty() {
                    opts.push_str(&format!(",password={}", password));
                }
            }
            Ok(("cifs".to_string(), src, opts))
        }
    }
}

fn is_mounted(mnt: &str) -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|s| s.lines().any(|l| l.split_whitespace().nth(1) == Some(mnt)))
        .unwrap_or(false)
}

fn ensure_mount_helper(fstype: &str) -> Result<(), AppError> {
    let (helper, pkg) = match fstype {
        "cifs" => ("/sbin/mount.cifs", "cifs-utils"),
        "nfs" => ("/sbin/mount.nfs", "nfs-common"),
        _ => return Ok(()),
    };
    if FsPath::new(helper).exists() || FsPath::new(&format!("/usr{helper}")).exists() {
        return Ok(());
    }
    Err(AppError::internal(format!(
        "未找到 {helper}，请先在目标主机安装 {pkg}"
    )))
}

fn persist_fstab(fstype: &str, src: &str, mnt: &str, opts: &str) -> Result<bool, String> {
    let fstab = FsPath::new("/etc/fstab");
    let existing = std::fs::read_to_string(fstab).map_err(|e| e.to_string())?;
    if existing
        .lines()
        .any(|l| l.split_whitespace().nth(1) == Some(mnt))
    {
        return Ok(false);
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(fstab)
        .map_err(|e| e.to_string())?;
    writeln!(f, "\n# cangling-storage {}", mnt).map_err(|e| e.to_string())?;
    writeln!(f, "{src} {mnt} {fstype} {opts} 0 0").map_err(|e| e.to_string())?;
    Ok(true)
}

fn remove_fstab_entry(mnt: &str) -> Result<bool, String> {
    let fstab = FsPath::new("/etc/fstab");
    let content = std::fs::read_to_string(fstab).map_err(|e| e.to_string())?;
    let mut removed = false;
    let mut out = String::new();
    for line in content.lines() {
        if line.split_whitespace().nth(1) == Some(mnt) {
            removed = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if removed {
        std::fs::write(fstab, out).map_err(|e| e.to_string())?;
    }
    Ok(removed)
}

fn cmd_message(out: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    [stdout, stderr]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("；")
}

// ---------------------------------------------------------------------------
// 主机 CIFS 共享：samba 配置 + 启动
// ---------------------------------------------------------------------------

fn start_host_share_local(s: &Storage) -> Result<String, AppError> {
    let dir = s.path.trim();
    if dir.is_empty() {
        return Err(AppError::bad("请填写要共享的目录"));
    }
    if !FsPath::new(dir).is_dir() {
        std::fs::create_dir_all(dir)
            .map_err(|e| AppError::internal(format!("创建共享目录失败 {dir}：{e}")))?;
    }
    let share_name = sanitize_share_name(&s.share)?;

    if find_smbd().is_none() {
        return Err(AppError::internal(
            "未检测到 samba（smbd）。请先在主机安装 samba 后再启动共享。",
        ));
    }

    let conf_dir = FsPath::new("/etc/samba");
    std::fs::create_dir_all(conf_dir)
        .map_err(|e| AppError::internal(format!("创建 {} 失败：{e}", conf_dir.display())))?;

    // 认证：填写用户名/密码时使用用户认证；否则允许匿名 guest 访问（适合集群内网）。
    let auth_note = if s.username.is_empty() {
        "匿名 guest 访问".to_string()
    } else {
        ensure_samba_user(&s.username, &s.password)?;
        format!("用户认证（{}）", s.username)
    };

    let conf_file = conf_dir.join(format!("cangling-{}.conf", s.id));
    let mut snippet = format!(
        "[{share_name}]\n   path = {dir}\n   browseable = yes\n   read only = no\n"
    );
    if s.username.is_empty() {
        snippet.push_str("   guest ok = yes\n");
    } else {
        snippet.push_str(&format!("   valid users = {}\n   guest ok = no\n", s.username));
    }
    for line in s.options.lines() {
        let l = line.trim();
        if !l.is_empty() {
            snippet.push_str("   ");
            snippet.push_str(l);
            snippet.push('\n');
        }
    }
    std::fs::write(&conf_file, snippet)
        .map_err(|e| AppError::internal(format!("写入 {} 失败：{e}", conf_file.display())))?;

    let smb_conf = conf_dir.join("smb.conf");
    ensure_include(&smb_conf, &format!("include = {}", conf_file.display()))?;

    let note = restart_smbd()?;
    Ok(format!(
        "共享 [{share_name}] 已配置（目录 {dir}，{auth_note}）并重启 smbd：{note}"
    ))
}

fn sanitize_share_name(raw: &str) -> Result<String, AppError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(AppError::bad("共享名称不能为空"));
    }
    if name
        .chars()
        .any(|c| matches!(c, '/' | '\\' | '[' | ']' | '\0' | '\n' | '\r'))
    {
        return Err(AppError::bad("共享名称包含非法字符"));
    }
    Ok(name.to_string())
}

fn ensure_include(smb_conf: &FsPath, include_line: &str) -> Result<(), AppError> {
    let content = std::fs::read_to_string(smb_conf).unwrap_or_else(|_| "[global]\n".to_string());
    if content.lines().any(|l| l.trim() == include_line.trim()) {
        return Ok(());
    }
    let mut out = content;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(include_line);
    out.push('\n');
    std::fs::write(smb_conf, out)
        .map_err(|e| AppError::internal(format!("更新 {} 失败：{e}", smb_conf.display())))
}

fn restart_smbd() -> Result<String, AppError> {
    for unit in ["smbd", "smb", "samba"] {
        if let Ok(out) = Command::new("systemctl").args(["restart", unit]).output() {
            if out.status.success() {
                return Ok(format!("systemctl restart {unit}"));
            }
        }
    }
    for svc in ["smbd", "samba"] {
        if let Ok(out) = Command::new("service").args([svc, "restart"]).output() {
            if out.status.success() {
                return Ok(format!("service {svc} restart"));
            }
        }
    }
    Err(AppError::internal(
        "无法重启 samba 服务（请确认已安装并启用 smbd）",
    ))
}

fn find_smbd() -> Option<PathBuf> {
    for cand in ["smbd", "/usr/sbin/smbd", "/usr/local/sbin/smbd"] {
        let p = FsPath::new(cand);
        if p.is_absolute() {
            if p.is_file() {
                return Some(p.to_path_buf());
            }
        } else if let Some(found) = which(cand) {
            return Some(found);
        }
    }
    None
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn ensure_samba_user(username: &str, password: &str) -> Result<(), AppError> {
    let exists = samba_user_exists(username);
    let mut cmd = Command::new("smbpasswd");
    cmd.arg("-s");
    if !exists {
        cmd.arg("-a");
    }
    cmd.arg(username)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::internal(format!("执行 smbpasswd 失败：{e}")))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| AppError::internal("无法写入 smbpasswd 输入"))?;
        writeln!(stdin, "{password}")
            .map_err(|e| AppError::internal(format!("写入 smbpasswd 失败：{e}")))?;
        writeln!(stdin, "{password}")
            .map_err(|e| AppError::internal(format!("写入 smbpasswd 失败：{e}")))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| AppError::internal(format!("等待 smbpasswd 失败：{e}")))?;
    if !out.status.success() {
        let msg = cmd_message(&out);
        return Err(AppError::internal(format!(
            "设置 samba 用户 {username} 失败：{msg}"
        )));
    }
    Ok(())
}

fn samba_user_exists(username: &str) -> bool {
    Command::new("pdbedit")
        .arg("-L")
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.split(':').next() == Some(username))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_share_name_rejects_bad_chars() {
        assert!(sanitize_share_name("data").is_ok());
        assert!(sanitize_share_name(" 数据 ").is_ok());
        assert!(sanitize_share_name("a/b").is_err());
        assert!(sanitize_share_name("a[b]").is_err());
        assert!(sanitize_share_name("").is_err());
    }

    #[test]
    fn mount_spec_builds_cifs_and_nfs() {
        let (fstype, src, opts) =
            build_mount_spec_parts("cifs", "10.0.0.2", "backup", "u", "p", "iocharset=utf8")
                .unwrap();
        assert_eq!(fstype, "cifs");
        assert_eq!(src, "//10.0.0.2/backup");
        assert!(opts.contains("username=u"));
        assert!(opts.contains("password=p"));
        assert!(opts.contains("iocharset=utf8"));

        let (fstype, src, _) =
            build_mount_spec_parts("nfs", "10.0.0.2", "/data", "", "", "").unwrap();
        assert_eq!(fstype, "nfs");
        assert_eq!(src, "10.0.0.2:/data");

        // 未设置用户名时自动带入 guest 选项。
        let (_, _, guest_opts) =
            build_mount_spec_parts("cifs", "10.0.0.2", "backup", "", "", "").unwrap();
        assert!(guest_opts.contains("guest"));
    }
}
