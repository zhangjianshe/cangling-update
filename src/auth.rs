use crate::db;
use crate::error::AppError;
use crate::models::{
    AuthStatus, AuthUser, ChangePasswordBody, ChangePasswordResponse, Credentials, SyncFailure,
};
use crate::state::AppState;
use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, Request};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::net::SocketAddr;
use uuid::Uuid;

pub const IDLE_TIMEOUT_SECS: u64 = 2 * 60 * 60;
const COOKIE_NAME: &str = "cangling_session";

pub fn is_public(method: &Method, path: &str) -> bool {
    if path.starts_with("/vendor/") || path.starts_with("/media/portal/") {
        return true;
    }
    if matches!(path, "/" | "/console") {
        return true;
    }
    if matches!(
        path,
        "/api/auth/status" | "/api/auth/login" | "/api/auth/setup" | "/api/auth/logout"
    ) {
        return true;
    }
    // 机器间接口由 cluster::require_cluster_token 单独认证，不走登录会话。
    if matches!(path, "/api/cluster/register" | "/api/cluster/heartbeat")
        || path == "/api/cluster/repo"
        || path == "/api/cluster/init/run"
        || path == "/api/cluster/auth/sync"
        || path == "/api/cluster/storage/start-share"
        || path == "/api/cluster/storage/mount"
        || path == "/api/cluster/storage/unmount"
        || path == "/api/cluster/self-update"
        || path.starts_with("/api/cluster/self-update/")
        || path == "/api/cluster/images"
        || path == "/api/cluster/images/import"
        || path == "/api/cluster/images/delete"
        || path.starts_with("/api/cluster/images/archive/")
        || (path.starts_with("/api/cluster/repo/") && path.ends_with("/download"))
    {
        return true;
    }
    method == Method::GET && path == "/api/portal"
}

fn is_hostinfo_path(path: &str) -> bool {
    matches!(path, "/hostinfo" | "/hostinfo.md")
}

pub async fn require_auth(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, AppError> {
    let path = request.uri().path().to_string();
    if is_public(request.method(), &path) || (is_hostinfo_path(&path) && addr.ip().is_loopback()) {
        return Ok(next.run(request).await);
    }

    let token = read_token(request.headers()).or_else(|| token_from_query(request.uri().query()));
    match current_user(&state, token.as_deref())? {
        Some(_) => {
            let token = token.expect("token present when user exists");
            let mut response = next.run(request).await;
            if let Ok(value) = HeaderValue::from_str(&set_cookie(&token)) {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
            Ok(response)
        }
        None => Err(AppError::unauthorized("请先登录或登录已过期")),
    }
}

pub fn auth_status(state: &AppState, headers: &HeaderMap) -> Result<AuthStatus, AppError> {
    let needs_setup = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::user_count(&conn)? == 0
    };
    let user = current_user(state, read_token(headers).as_deref())?.map(|u| AuthUser {
        id: u.id,
        username: u.username,
        token: None,
    });
    Ok(AuthStatus {
        needs_setup,
        user,
        idle_timeout_secs: IDLE_TIMEOUT_SECS,
    })
}

pub async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthStatus>, AppError> {
    Ok(Json(auth_status(&state, &headers)?))
}

pub fn current_auth_user(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<AuthUser>, AppError> {
    let token = read_token(headers);
    Ok(current_user(state, token.as_deref())?.map(|u| AuthUser {
        id: u.id,
        username: u.username,
        token: None,
    }))
}

pub async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ChangePasswordBody>,
) -> Result<Json<ChangePasswordResponse>, AppError> {
    let user = current_auth_user(&state, &headers)?
        .ok_or_else(|| AppError::unauthorized("请先登录或登录已过期"))?;

    validate_password(&body.new_password)?;

    let stored_hash = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_user_by_id(&conn, &user.id)?
            .ok_or_else(|| AppError::unauthorized("用户不存在"))?
            .password_hash
    };
    let old_password = body.old_password.clone();
    let ok = tokio::task::spawn_blocking(move || verify_password(&old_password, &stored_hash))
        .await
        .map_err(|e| AppError::internal(e.to_string()))?;
    if !ok {
        return Err(AppError::unauthorized("当前密码不正确"));
    }

    let new_hash = hash_password_async(body.new_password).await?;
    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::update_password_hash(&conn, &user.id, &new_hash)?;
        // 修改密码后使所有登录会话失效，要求重新登录。
        db::delete_sessions_for_user(&conn, &user.id)?;
    }

    let (synced, failed) = sync_password_to_workers(&state, &user.username, &new_hash).await;

    Ok(Json(ChangePasswordResponse {
        ok: true,
        synced,
        failed,
    }))
}

async fn sync_password_to_workers(
    state: &AppState,
    username: &str,
    password_hash: &str,
) -> (Vec<String>, Vec<SyncFailure>) {
    let token = state.cluster.token.clone().unwrap_or_default();
    let workers = match crate::cluster::server::online_workers(state) {
        Ok(w) => w,
        Err(e) => {
            return (
                Vec::new(),
                vec![SyncFailure {
                    node: "本机".to_string(),
                    error: e.to_string(),
                }],
            )
        }
    };
    let mut synced = Vec::new();
    let mut failed = Vec::new();
    for (name, addr) in workers {
        let url = format!("http://{addr}/api/cluster/auth/sync");
        let body = serde_json::json!({
            "username": username,
            "password_hash": password_hash,
        });
        match crate::cluster::http::post_json(&url, &token, &body).await {
            Ok((status, _)) if status.is_success() => synced.push(name),
            Ok((status, value)) => failed.push(SyncFailure {
                node: name,
                error: format!("HTTP {status}: {}", json_error(&value)),
            }),
            Err(e) => failed.push(SyncFailure {
                node: name,
                error: format!("{e:#}"),
            }),
        }
    }
    (synced, failed)
}

/// 将 master 上所有用户推送到指定 worker（新节点加入时调用）。
pub async fn sync_users_to_worker(state: &AppState, waddr: &str) {
    let token = state.cluster.token.clone().unwrap_or_default();
    if token.is_empty() {
        return;
    }
    let users = {
        let Ok(conn) = state.db.lock() else {
            return;
        };
        match db::list_users(&conn) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("读取本机用户列表失败：{e}");
                return;
            }
        }
    };
    for user in users {
        let url = format!("http://{waddr}/api/cluster/auth/sync");
        let body = serde_json::json!({
            "username": user.username,
            "password_hash": user.password_hash,
        });
        match crate::cluster::http::post_json(&url, &token, &body).await {
            Ok((status, _)) if status.is_success() => {
                tracing::info!("已同步账号 {} 到 {}", body["username"], waddr);
            }
            Ok((status, value)) => {
                tracing::warn!(
                    "同步账号到 {waddr} 失败：HTTP {status}: {}",
                    json_error(&value)
                );
            }
            Err(e) => {
                tracing::warn!("同步账号到 {waddr} 失败：{e:#}");
            }
        }
    }
}

fn json_error(value: &serde_json::Value) -> String {
    value
        .get("error")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

pub async fn setup(
    State(state): State<AppState>,
    Json(body): Json<Credentials>,
) -> Result<impl IntoResponse, AppError> {
    let username = normalize_username(&body.username)?;
    let password = body.password;
    validate_password(&password)?;

    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        if db::user_count(&conn)? > 0 {
            return Err(AppError::Conflict("管理员已初始化，请直接登录".into()));
        }
    }

    let hash = hash_password_async(password).await?;
    let user_id = Uuid::new_v4().to_string();
    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::insert_user(&conn, &user_id, &username, &hash)?;
    }

    issue_session(&state, &user_id, &username)
}

pub async fn login(
    State(state): State<AppState>,
    Json(body): Json<Credentials>,
) -> Result<impl IntoResponse, AppError> {
    let username = body.username.trim().to_string();
    let password = body.password;
    if username.is_empty() || password.is_empty() {
        return Err(AppError::bad("请输入用户名和密码"));
    }

    if let Some(secs) = state.login_guard.lockout_remaining_secs(&username) {
        let minutes = (secs + 59) / 60;
        return Err(AppError::unauthorized(format!(
            "登录失败次数过多，请 {minutes} 分钟后再试"
        )));
    }

    let user = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_user_by_name(&conn, &username)?
    };
    let Some(user) = user else {
        state.login_guard.record_failure(&username);
        return Err(AppError::unauthorized("用户名或密码错误"));
    };

    let hash = user.password_hash.clone();
    let ok = tokio::task::spawn_blocking(move || verify_password(&password, &hash))
        .await
        .map_err(|e| AppError::internal(e.to_string()))?;
    if !ok {
        state.login_guard.record_failure(&username);
        return Err(AppError::unauthorized("用户名或密码错误"));
    }

    state.login_guard.clear(&username);
    issue_session(&state, &user.id, &user.username)
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(token) = read_token(&headers) {
        if let Ok(conn) = state.db.lock() {
            let _ = db::delete_session(&conn, &token);
        }
    }
    let mut res = Json(serde_json::json!({ "ok": true })).into_response();
    if let Ok(value) = HeaderValue::from_str(&clear_cookie()) {
        res.headers_mut().insert(header::SET_COOKIE, value);
    }
    res
}

fn issue_session(
    state: &AppState,
    user_id: &str,
    username: &str,
) -> Result<impl IntoResponse, AppError> {
    let token = Uuid::new_v4().to_string();
    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::create_session(&conn, &token, user_id)?;
    }
    let body = Json(AuthUser {
        id: user_id.to_string(),
        username: username.to_string(),
        token: Some(token.clone()),
    });
    let mut res = body.into_response();
    if let Ok(value) = HeaderValue::from_str(&set_cookie(&token)) {
        res.headers_mut().insert(header::SET_COOKIE, value);
    }
    Ok(res)
}

struct SessionUser {
    id: String,
    username: String,
}

fn current_user(state: &AppState, token: Option<&str>) -> Result<Option<SessionUser>, AppError> {
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let Some((user_id, last_seen)) = db::session_user_id(&conn, token)? else {
        return Ok(None);
    };
    if session_expired(&last_seen) {
        let _ = db::delete_session(&conn, token);
        return Ok(None);
    }
    db::touch_session(&conn, token)?;
    let Some(user) = db::get_user_by_id(&conn, &user_id)? else {
        let _ = db::delete_session(&conn, token);
        return Ok(None);
    };
    Ok(Some(SessionUser {
        id: user.id,
        username: user.username,
    }))
}

fn session_expired(last_seen: &str) -> bool {
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(last_seen) else {
        return true;
    };
    let age = chrono::Utc::now().signed_duration_since(ts.with_timezone(&chrono::Utc));
    age.num_seconds() >= IDLE_TIMEOUT_SECS as i64
}

fn token_from_query(query: Option<&str>) -> Option<String> {
    let query = query?;
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        if k == "token" || k == "access_token" {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn read_token(headers: &HeaderMap) -> Option<String> {
    if let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let auth = auth.trim();
        let token = auth
            .strip_prefix("Bearer ")
            .or_else(|| auth.strip_prefix("bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty());
        if let Some(token) = token {
            return Some(token.to_string());
        }
    }
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix(&format!("{COOKIE_NAME}="))
            .map(|v| v.to_string())
    })
}

fn set_cookie(token: &str) -> String {
    format!("{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={IDLE_TIMEOUT_SECS}")
}

fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

fn normalize_username(raw: &str) -> Result<String, AppError> {
    let name = raw.trim();
    if name.len() < 2 || name.len() > 32 {
        return Err(AppError::bad("用户名长度需要 2–32 个字符"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(AppError::bad("用户名只能包含字母、数字、下划线或短横线"));
    }
    Ok(name.to_string())
}

pub fn validate_password(password: &str) -> Result<(), AppError> {
    if password.len() < 8 {
        return Err(AppError::bad("密码至少 8 位"));
    }
    if password.len() > 128 {
        return Err(AppError::bad("密码过长"));
    }
    Ok(())
}

async fn hash_password_async(password: String) -> Result<String, AppError> {
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| AppError::internal(e.to_string()))?
}

pub fn hash_password(password: &str) -> Result<String, AppError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::internal(e.to_string()))
}

fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_paths_allow_portal_read_but_not_write() {
        assert!(is_public(&Method::GET, "/"));
        assert!(is_public(&Method::GET, "/console"));
        assert!(is_public(&Method::GET, "/api/portal"));
        assert!(!is_public(&Method::PUT, "/api/portal"));
        assert!(!is_public(&Method::POST, "/api/portal/items"));
        assert!(is_public(&Method::GET, "/vendor/portal.jpg"));
        assert!(is_public(&Method::GET, "/media/portal/background"));
        assert!(is_public(&Method::GET, "/media/portal/icon/abc"));
        assert!(is_public(&Method::GET, "/api/auth/status"));
        assert!(!is_public(&Method::GET, "/hostinfo"));
        assert!(!is_public(&Method::GET, "/hostinfo.md"));
        assert!(is_hostinfo_path("/hostinfo"));
        assert!(is_hostinfo_path("/hostinfo.md"));
        assert!(!is_hostinfo_path("/hostinfo?color=0"));
        assert!(!is_public(&Method::GET, "/api/projects"));
        assert!(is_public(&Method::POST, "/api/cluster/register"));
        assert!(is_public(&Method::POST, "/api/cluster/heartbeat"));
        assert!(is_public(&Method::GET, "/api/cluster/repo"));
        assert!(is_public(
            &Method::GET,
            "/api/cluster/repo/linux-x86/demo/download"
        ));
        assert!(is_public(&Method::POST, "/api/cluster/auth/sync"));
        assert!(is_public(&Method::POST, "/api/cluster/storage/start-share"));
        assert!(is_public(&Method::POST, "/api/cluster/storage/mount"));
        assert!(is_public(&Method::POST, "/api/cluster/storage/unmount"));
        assert!(is_public(&Method::GET, "/api/cluster/self-update"));
        assert!(is_public(
            &Method::GET,
            "/api/cluster/self-update/linux-amd64"
        ));
        assert!(is_public(
            &Method::GET,
            "/api/cluster/self-update/linux-arm64"
        ));
        assert!(is_public(&Method::GET, "/api/cluster/images"));
        assert!(is_public(&Method::POST, "/api/cluster/images/import"));
        assert!(is_public(&Method::POST, "/api/cluster/images/delete"));
        assert!(is_public(
            &Method::GET,
            "/api/cluster/images/archive/images.tar.gz"
        ));
        assert!(!is_public(&Method::GET, "/api/cluster/nodes"));
        assert!(!is_public(&Method::GET, "/api/cluster/repo/linux-x86/demo"));
        assert!(!is_public(&Method::GET, "/api/cluster/packages/abc/file"));
        assert!(!is_public(&Method::POST, "/api/cluster/tasks/run"));
    }

    #[test]
    fn read_token_prefers_bearer_over_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer header-token"),
        );
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("cangling_session=cookie-token"),
        );
        assert_eq!(read_token(&headers).as_deref(), Some("header-token"));
    }

    #[test]
    fn read_token_falls_back_to_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=1; cangling_session=cookie-token"),
        );
        assert_eq!(read_token(&headers).as_deref(), Some("cookie-token"));
    }
}
