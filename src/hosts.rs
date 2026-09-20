use crate::cluster::{self, OFFLINE_AFTER_SECS};
use crate::error::AppError;
use crate::state::AppState;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, put};
use axum::{Json, Router};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::IpAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path as FsPath;
use std::str::FromStr;
use uuid::Uuid;

const LOCAL_HOST_ID: &str = "local";
const KEY_FILE: &str = "host-credentials.key";
const KNOWN_HOSTS_FILE: &str = "ssh-known-hosts";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/hosts", get(list).post(create))
        .route("/api/hosts/{id}", put(update).delete(remove))
        .route("/api/hosts/{id}/terminal", get(terminal))
}

#[derive(Debug, Clone)]
struct HostRow {
    id: String,
    name: String,
    host: String,
    ssh_port: u16,
    username: String,
    password_enc: String,
    kind: String,
    cluster_node_id: String,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone)]
struct ClusterInfo {
    version: String,
    role: String,
    last_seen: String,
    status: &'static str,
    host: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct ManagedHost {
    id: String,
    name: String,
    host: String,
    ssh_port: u16,
    username: String,
    kind: String,
    cluster_node_id: String,
    cluster_status: String,
    version: String,
    role: String,
    last_seen: String,
    host_info: Option<serde_json::Value>,
    password_set: bool,
    editable: bool,
    deletable: bool,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Deserialize)]
pub struct SaveHost {
    name: String,
    host: String,
    ssh_port: Option<u16>,
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    clear_password: bool,
}

async fn list(State(state): State<AppState>) -> Result<Json<Vec<ManagedHost>>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    sync_inventory(&conn, &state)?;
    Ok(Json(list_rows(&conn)?))
}

async fn create(
    State(state): State<AppState>,
    Json(body): Json<SaveHost>,
) -> Result<Json<ManagedHost>, AppError> {
    let input = validate_input(body)?;
    let now = chrono::Utc::now().to_rfc3339();
    let id = Uuid::new_v4().to_string();
    let password_enc = if input.password.is_empty() {
        String::new()
    } else {
        encrypt_password(&state, &input.password)?
    };
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    conn.execute(
        "INSERT INTO managed_hosts
         (id,name,host,ssh_port,username,password_enc,kind,cluster_node_id,created_at,updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,'manual','',?7,?7)",
        params![
            id,
            input.name,
            input.host,
            input.ssh_port,
            input.username,
            password_enc,
            now
        ],
    )?;
    get_view(&conn, &id)?
        .ok_or_else(|| AppError::internal("保存主机后无法读取"))
        .map(Json)
}

async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SaveHost>,
) -> Result<Json<ManagedHost>, AppError> {
    if id == LOCAL_HOST_ID {
        return Err(AppError::bad("本机信息由系统维护，不能编辑"));
    }
    let input = validate_input(body)?;
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let current = get_row(&conn, &id)?.ok_or_else(|| AppError::not_found("主机不存在"))?;
    let password_enc = if input.clear_password {
        String::new()
    } else if input.password.is_empty() {
        current.password_enc
    } else {
        encrypt_password(&state, &input.password)?
    };
    let changed = conn.execute(
        "UPDATE managed_hosts SET name=?2,host=?3,ssh_port=?4,username=?5,password_enc=?6,updated_at=?7 WHERE id=?1",
        params![id, input.name, input.host, input.ssh_port, input.username, password_enc, chrono::Utc::now().to_rfc3339()],
    )?;
    if changed == 0 {
        return Err(AppError::not_found("主机不存在"));
    }
    get_view(&conn, &id)?
        .ok_or_else(|| AppError::internal("更新主机后无法读取"))
        .map(Json)
}

async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let row = get_row(&conn, &id)?.ok_or_else(|| AppError::not_found("主机不存在"))?;
    if row.kind != "manual" {
        return Err(AppError::bad("本机和集群自动发现的主机不能删除"));
    }
    conn.execute("DELETE FROM managed_hosts WHERE id=?1", params![id])?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn terminal(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<crate::term::ExecQuery>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AppError> {
    let row = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        get_row(&conn, &id)?.ok_or_else(|| AppError::not_found("主机不存在"))?
    };
    let cwd = state.paths.exe_dir.clone();
    if row.kind == "local" {
        return Ok(ws.on_upgrade(move |socket| {
            crate::term::run_command_socket(
                socket,
                "/bin/bash".to_string(),
                vec!["-l".to_string()],
                cwd,
                query,
                format!("已连接本机 {}\r\n", row.name),
                None,
            )
        }));
    }

    let password = if row.password_enc.is_empty() {
        None
    } else {
        Some(decrypt_password(&state, &row.password_enc)?)
    };
    let known_hosts = state.paths.config_dir.join(KNOWN_HOSTS_FILE);
    let destination = format!("{}@{}", row.username, row.host);
    let args = vec![
        "-tt".to_string(),
        "-p".to_string(),
        row.ssh_port.to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=30".to_string(),
        "-o".to_string(),
        "ServerAliveCountMax=3".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(),
        format!("UserKnownHostsFile={}", known_hosts.display()),
        "-o".to_string(),
        "NumberOfPasswordPrompts=1".to_string(),
        destination,
    ];
    Ok(ws.on_upgrade(move |socket| {
        crate::term::run_command_socket(
            socket,
            "ssh".to_string(),
            args,
            cwd,
            query,
            format!(
                "正在通过 SSH 连接 {}@{}:{}…\r\n",
                row.username, row.host, row.ssh_port
            ),
            password,
        )
    }))
}

fn sync_inventory(conn: &Connection, state: &AppState) -> Result<(), AppError> {
    let now = chrono::Utc::now().to_rfc3339();
    let hostname = std::fs::read_to_string("/etc/hostname")
        .unwrap_or_else(|_| "本机".into())
        .trim()
        .to_string();
    let local_node_id = cluster::load_or_create_node_id(&state.paths);
    conn.execute(
        "INSERT INTO managed_hosts
         (id,name,host,ssh_port,username,password_enc,kind,cluster_node_id,created_at,updated_at)
         VALUES (?1,?2,?3,22,'root','','local',?4,?5,?5)
         ON CONFLICT(id) DO UPDATE SET name=excluded.name,host=excluded.host,cluster_node_id=excluded.cluster_node_id,updated_at=excluded.updated_at",
        params![LOCAL_HOST_ID, hostname, crate::hostinfo::primary_ip(), local_node_id, now],
    )?;

    let mut stmt =
        conn.prepare("SELECT id,name,addr,info_json FROM cluster_nodes ORDER BY name")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let nodes = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for (node_id, name, addr, info_json) in nodes {
        if node_id == local_node_id {
            continue;
        }
        let host = cluster_host(&addr, &info_json);
        if host.is_empty() {
            continue;
        }
        let id = format!("cluster:{node_id}");
        conn.execute(
            "INSERT INTO managed_hosts
             (id,name,host,ssh_port,username,password_enc,kind,cluster_node_id,created_at,updated_at)
             VALUES (?1,?2,?3,22,'root','','cluster',?4,?5,?5)
             ON CONFLICT(id) DO NOTHING",
            params![id, name, host, node_id, now],
        )?;
    }
    Ok(())
}

fn cluster_host(addr: &str, info_json: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(info_json) {
        if let Some(ip) = value.get("primary_ip").and_then(|v| v.as_str()) {
            if IpAddr::from_str(ip).is_ok() {
                return ip.to_string();
            }
        }
    }
    if let Ok(socket) = addr.parse::<std::net::SocketAddr>() {
        return socket.ip().to_string();
    }
    addr.rsplit_once(':')
        .map(|(host, _)| host.trim_matches(['[', ']']).to_string())
        .filter(|host| IpAddr::from_str(host).is_ok())
        .unwrap_or_default()
}

fn list_rows(conn: &Connection) -> Result<Vec<ManagedHost>, AppError> {
    let clusters = cluster_info(conn)?;
    let mut stmt = conn.prepare(
        "SELECT id,name,host,ssh_port,username,password_enc,kind,cluster_node_id,created_at,updated_at
         FROM managed_hosts ORDER BY CASE kind WHEN 'local' THEN 0 WHEN 'cluster' THEN 1 ELSE 2 END,name",
    )?;
    let rows = stmt.query_map([], map_row)?;
    let mut result = Vec::new();
    for row in rows {
        result.push(to_view(row?, &clusters));
    }
    Ok(result)
}

fn get_view(conn: &Connection, id: &str) -> Result<Option<ManagedHost>, AppError> {
    let row = get_row(conn, id)?;
    let clusters = cluster_info(conn)?;
    Ok(row.map(|row| to_view(row, &clusters)))
}

fn get_row(conn: &Connection, id: &str) -> Result<Option<HostRow>, AppError> {
    conn.query_row(
        "SELECT id,name,host,ssh_port,username,password_enc,kind,cluster_node_id,created_at,updated_at FROM managed_hosts WHERE id=?1",
        params![id],
        map_row,
    )
    .optional()
    .map_err(AppError::from)
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HostRow> {
    let port: i64 = row.get(3)?;
    Ok(HostRow {
        id: row.get(0)?,
        name: row.get(1)?,
        host: row.get(2)?,
        ssh_port: port.clamp(1, u16::MAX as i64) as u16,
        username: row.get(4)?,
        password_enc: row.get(5)?,
        kind: row.get(6)?,
        cluster_node_id: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn cluster_info(conn: &Connection) -> Result<HashMap<String, ClusterInfo>, AppError> {
    let mut stmt = conn.prepare("SELECT id,version,role,last_seen,info_json FROM cluster_nodes")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut result = HashMap::new();
    for row in rows {
        let (id, version, role, last_seen, info_json) = row?;
        result.insert(
            id,
            ClusterInfo {
                version,
                role,
                status: if cluster_online(&last_seen) {
                    "online"
                } else {
                    "offline"
                },
                host: serde_json::from_str(&info_json).ok(),
                last_seen,
            },
        );
    }
    Ok(result)
}

fn cluster_online(last_seen: &str) -> bool {
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(last_seen) else {
        return false;
    };
    let age = chrono::Utc::now().signed_duration_since(ts.with_timezone(&chrono::Utc));
    age.num_seconds() >= 0 && age.num_seconds() < OFFLINE_AFTER_SECS as i64
}

fn to_view(row: HostRow, clusters: &HashMap<String, ClusterInfo>) -> ManagedHost {
    let cluster = clusters.get(&row.cluster_node_id);
    ManagedHost {
        id: row.id,
        name: row.name,
        host: row.host,
        ssh_port: row.ssh_port,
        username: row.username,
        kind: row.kind.clone(),
        cluster_node_id: row.cluster_node_id,
        cluster_status: if row.kind == "local" {
            "local".into()
        } else if let Some(info) = cluster {
            info.status.into()
        } else {
            "unmanaged".into()
        },
        version: cluster.map(|v| v.version.clone()).unwrap_or_default(),
        role: cluster.map(|v| v.role.clone()).unwrap_or_default(),
        last_seen: cluster.map(|v| v.last_seen.clone()).unwrap_or_default(),
        host_info: cluster.and_then(|v| v.host.clone()),
        password_set: !row.password_enc.is_empty(),
        editable: row.kind != "local",
        deletable: row.kind == "manual",
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn validate_input(mut body: SaveHost) -> Result<SaveHost, AppError> {
    body.name = body.name.trim().to_string();
    body.host = body.host.trim().trim_matches(['[', ']']).to_string();
    body.username = body.username.trim().to_string();
    body.ssh_port = Some(body.ssh_port.unwrap_or(22));
    if body.name.is_empty() || body.name.chars().count() > 80 {
        return Err(AppError::bad("主机名称不能为空且不能超过 80 个字符"));
    }
    if IpAddr::from_str(&body.host).is_err() {
        return Err(AppError::bad("主机地址必须是有效的 IPv4 或 IPv6 地址"));
    }
    if body.ssh_port == Some(0) {
        return Err(AppError::bad("SSH 端口必须在 1–65535 之间"));
    }
    if body.username.is_empty()
        || body.username.len() > 32
        || !body
            .username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(AppError::bad(
            "SSH 用户名只能包含字母、数字、下划线、短横线或点",
        ));
    }
    if body.password.len() > 512 {
        return Err(AppError::bad("SSH 密码过长"));
    }
    Ok(body)
}

fn encrypt_password(state: &AppState, password: &str) -> Result<String, AppError> {
    encrypt_password_in(&state.paths.config_dir, password)
}

fn encrypt_password_in(config_dir: &FsPath, password: &str) -> Result<String, AppError> {
    let key = credential_key(config_dir)?;
    let nonce = random_bytes(24)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|_| AppError::internal("无法初始化主机密码加密器"))?;
    let encrypted = cipher
        .encrypt(XNonce::from_slice(&nonce), password.as_bytes())
        .map_err(|_| AppError::internal("加密主机密码失败"))?;
    Ok(format!("v1:{}:{}", hex(&nonce), hex(&encrypted)))
}

fn decrypt_password(state: &AppState, value: &str) -> Result<String, AppError> {
    decrypt_password_in(&state.paths.config_dir, value)
}

fn decrypt_password_in(config_dir: &FsPath, value: &str) -> Result<String, AppError> {
    let mut parts = value.split(':');
    let (Some("v1"), Some(nonce), Some(ciphertext), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(AppError::internal("主机密码格式无效"));
    };
    let nonce = unhex(nonce)?;
    let ciphertext = unhex(ciphertext)?;
    if nonce.len() != 24 {
        return Err(AppError::internal("主机密码 nonce 无效"));
    }
    let key = credential_key(config_dir)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|_| AppError::internal("无法初始化主机密码解密器"))?;
    let plain = cipher
        .decrypt(XNonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| AppError::internal("无法解密主机密码"))?;
    String::from_utf8(plain).map_err(|_| AppError::internal("主机密码不是有效文本"))
}

fn credential_key(config_dir: &FsPath) -> Result<[u8; 32], AppError> {
    let path = config_dir.join(KEY_FILE);
    if let Ok(bytes) = std::fs::read(&path) {
        return bytes
            .try_into()
            .map_err(|_| AppError::internal(format!("{} 长度无效", path.display())));
    }
    let key = random_bytes(32)?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(&key)?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let bytes = std::fs::read(&path)?;
            return bytes
                .try_into()
                .map_err(|_| AppError::internal(format!("{} 长度无效", path.display())));
        }
        Err(error) => return Err(error.into()),
    }
    key.try_into()
        .map_err(|_| AppError::internal("生成主机密码密钥失败"))
}

fn random_bytes(size: usize) -> Result<Vec<u8>, AppError> {
    let mut file = File::open("/dev/urandom")?;
    let mut bytes = vec![0u8; size];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(TABLE[(byte >> 4) as usize] as char);
        result.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    result
}

fn unhex(value: &str) -> Result<Vec<u8>, AppError> {
    let bytes = value.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err(AppError::internal("主机密码编码无效"));
    }
    bytes
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_digit(value: u8) -> Result<u8, AppError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(AppError::internal("主机密码编码无效")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_host_input() {
        assert!(validate_input(SaveHost {
            name: "n01".into(),
            host: "192.168.3.121".into(),
            ssh_port: Some(22),
            username: "root".into(),
            password: "secret".into(),
            clear_password: false,
        })
        .is_ok());
        assert!(validate_input(SaveHost {
            name: "bad".into(),
            host: "host;rm".into(),
            ssh_port: Some(22),
            username: "root".into(),
            password: String::new(),
            clear_password: false,
        })
        .is_err());
    }

    #[test]
    fn hex_round_trip() {
        let bytes = b"host password\0\xff";
        assert_eq!(unhex(&hex(bytes)).unwrap(), bytes);
    }

    #[test]
    fn password_encryption_round_trip_and_key_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cangling-host-key-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let encrypted = encrypt_password_in(&dir, "Inspur@123").unwrap();
        assert!(encrypted.starts_with("v1:"));
        assert!(!encrypted.contains("Inspur@123"));
        assert_eq!(decrypt_password_in(&dir, &encrypted).unwrap(), "Inspur@123");
        assert_eq!(
            std::fs::metadata(dir.join(KEY_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
