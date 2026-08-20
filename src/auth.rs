use crate::db;
use crate::error::AppError;
use crate::models::{AuthStatus, AuthUser, Credentials};
use crate::state::AppState;
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Method, Request};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
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
    method == Method::GET && path == "/api/portal"
}

pub async fn require_auth(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, AppError> {
    let path = request.uri().path().to_string();
    if is_public(request.method(), &path) {
        return Ok(next.run(request).await);
    }

    let token = read_token(request.headers());
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
    });
    Ok(AuthStatus {
        needs_setup,
        user,
        idle_timeout_secs: IDLE_TIMEOUT_SECS,
    })
}

pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<AuthStatus>, AppError> {
    Ok(Json(auth_status(&state, &headers)?))
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

    let user = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_user_by_name(&conn, &username)?
    };
    let Some(user) = user else {
        return Err(AppError::unauthorized("用户名或密码错误"));
    };

    let hash = user.password_hash.clone();
    let ok = tokio::task::spawn_blocking(move || verify_password(&password, &hash))
        .await
        .map_err(|e| AppError::internal(e.to_string()))?;
    if !ok {
        return Err(AppError::unauthorized("用户名或密码错误"));
    }

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

fn read_token(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix(&format!("{COOKIE_NAME}="))
            .map(|v| v.to_string())
    })
}

fn set_cookie(token: &str) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={IDLE_TIMEOUT_SECS}"
    )
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
        assert!(!is_public(&Method::GET, "/api/projects"));
    }
}
