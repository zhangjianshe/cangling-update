use crate::auth;
use crate::error::AppError;
use crate::state::AppState;
use anyhow::{bail, Context, Result};
use axum::extract::ws::{Message as ServerMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::IntoResponse;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use signature::{Signer, Verifier};
use ssh_key::{Algorithm, HashAlg, PrivateKey, PublicKey, Signature};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as ClientMessage;

const TARGET: &str = "127.0.0.1:22";
const DEFAULT_MAX_CONNECTIONS: usize = 4;
const DEFAULT_IDLE_SECS: u64 = 15 * 60;
static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
static USED_NONCES: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
const AUTHORIZED_KEYS_FILE: &str = "tunnel_authorized_keys";
const SIGNATURE_WINDOW_SECS: i64 = 60;
const SIGNATURE_DOMAIN: &str = "cangling-tunnel-v1";
const KEY_HEADER: &str = "x-cangling-tunnel-key";
const TIME_HEADER: &str = "x-cangling-tunnel-time";
const NONCE_HEADER: &str = "x-cangling-tunnel-nonce";
const SIGNATURE_HEADER: &str = "x-cangling-tunnel-signature";

pub async fn server(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AppError> {
    if !env_bool("CANGLING_TUNNEL_ENABLED") {
        return Err(AppError::not_found("TCP 隧道未启用"));
    }
    let principal = authenticate(&state, &headers)?;
    let max = env_usize("CANGLING_TUNNEL_MAX_CONNECTIONS", DEFAULT_MAX_CONNECTIONS).max(1);
    let permit = ConnectionPermit::acquire(max)
        .ok_or_else(|| AppError::conflict("TCP 隧道连接数已达到上限"))?;
    let idle = Duration::from_secs(env_u64("CANGLING_TUNNEL_IDLE_SECS", DEFAULT_IDLE_SECS).max(30));
    Ok(ws.on_upgrade(move |socket| run_server(socket, principal, idle, permit)))
}

fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<String, AppError> {
    if let Some(user) = auth::current_auth_user(state, headers)? {
        return Ok(format!("user:{}", user.username));
    }
    verify_public_key(state, headers)
}

fn verify_public_key(state: &AppState, headers: &HeaderMap) -> Result<String, AppError> {
    let key_id = header_text(headers, KEY_HEADER)?;
    let timestamp = header_text(headers, TIME_HEADER)?;
    let nonce = header_text(headers, NONCE_HEADER)?;
    let encoded_signature = header_text(headers, SIGNATURE_HEADER)?;
    let timestamp_value = timestamp
        .parse::<i64>()
        .map_err(|_| AppError::unauthorized("隧道签名时间无效"))?;
    if (unix_timestamp() - timestamp_value).abs() > SIGNATURE_WINDOW_SECS {
        return Err(AppError::unauthorized("隧道签名已过期"));
    }
    if nonce.len() < 20
        || nonce.len() > 128
        || !nonce.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(AppError::unauthorized("隧道 nonce 无效"));
    }

    let public_key = load_authorized_keys(&state.paths.config_dir)
        .map_err(|e| AppError::internal(format!("读取隧道公钥失败: {e:#}")))?
        .into_iter()
        .find(|key| key.fingerprint(HashAlg::Sha256).to_string() == key_id)
        .ok_or_else(|| AppError::unauthorized("隧道公钥未授权"))?;
    if public_key.algorithm() != Algorithm::Ed25519 {
        return Err(AppError::unauthorized("仅支持 Ed25519 隧道公钥"));
    }
    let raw_signature = BASE64
        .decode(encoded_signature)
        .map_err(|_| AppError::unauthorized("隧道签名格式无效"))?;
    let signature = Signature::new(Algorithm::Ed25519, raw_signature)
        .map_err(|_| AppError::unauthorized("隧道签名格式无效"))?;
    let message = signature_message(key_id, timestamp, nonce);
    Verifier::verify(&public_key, message.as_bytes(), &signature)
        .map_err(|_| AppError::unauthorized("隧道签名验证失败"))?;
    remember_nonce(key_id, nonce)?;
    Ok(format!("key:{key_id}"))
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, AppError> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| AppError::unauthorized("请先登录或提供隧道公钥签名"))
}

fn remember_nonce(key_id: &str, nonce: &str) -> Result<(), AppError> {
    let used = USED_NONCES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut used = used
        .lock()
        .map_err(|_| AppError::internal("tunnel nonce lock"))?;
    let now = Instant::now();
    used.retain(|_, seen| {
        now.duration_since(*seen) < Duration::from_secs((SIGNATURE_WINDOW_SECS * 2) as u64)
    });
    if used.insert(format!("{key_id}:{nonce}"), now).is_some() {
        return Err(AppError::unauthorized("隧道签名已被使用"));
    }
    Ok(())
}

fn load_authorized_keys(config_dir: &Path) -> Result<Vec<PublicKey>> {
    let path = config_dir.join(AUTHORIZED_KEYS_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("读取 {}", path.display()))?;
    content
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            (!line.is_empty() && !line.starts_with('#')).then_some((index, line))
        })
        .map(|(index, line)| {
            line.parse::<PublicKey>().with_context(|| {
                format!(
                    "{} 第 {} 行不是有效 OpenSSH 公钥",
                    path.display(),
                    index + 1
                )
            })
        })
        .collect()
}

pub fn authorize_key(config_dir: &Path, public_key_file: &Path) -> Result<()> {
    let text = std::fs::read_to_string(public_key_file)
        .with_context(|| format!("读取公钥 {}", public_key_file.display()))?;
    let key = text.trim().parse::<PublicKey>().context("公钥格式无效")?;
    if key.algorithm() != Algorithm::Ed25519 {
        bail!("仅支持 Ed25519 公钥");
    }
    let destination = config_dir.join(AUTHORIZED_KEYS_FILE);
    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
    if load_authorized_keys(config_dir)?
        .iter()
        .any(|existing| existing.fingerprint(HashAlg::Sha256).to_string() == fingerprint)
    {
        println!("公钥已授权：{fingerprint}");
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&destination)
        .with_context(|| format!("写入 {}", destination.display()))?;
    writeln!(file, "{}", key.to_openssh()?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600))?;
    }
    println!("已授权隧道公钥：{fingerprint}");
    println!("配置文件：{}", destination.display());
    Ok(())
}

async fn run_server(
    socket: WebSocket,
    principal: String,
    idle: Duration,
    _permit: ConnectionPermit,
) {
    let tcp = match TcpStream::connect(TARGET).await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(principal = %principal, target = TARGET, "tunnel target unavailable: {error}");
            return;
        }
    };
    let (mut ws_out, mut ws_in) = socket.split();
    let (mut tcp_in, mut tcp_out) = tcp.into_split();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut received = 0_u64;
    let mut sent = 0_u64;
    tracing::info!(principal = %principal, target = TARGET, "tunnel opened");

    loop {
        enum Event {
            WebSocket(Option<Result<ServerMessage, axum::Error>>),
            Tcp(std::io::Result<usize>),
        }
        let event = tokio::time::timeout(idle, async {
            tokio::select! {
                message = ws_in.next() => Event::WebSocket(message),
                read = tcp_in.read(&mut buffer) => Event::Tcp(read),
            }
        })
        .await;
        match event {
            Err(_) => break,
            Ok(Event::WebSocket(Some(Ok(ServerMessage::Binary(data))))) => {
                received += data.len() as u64;
                if tcp_out.write_all(&data).await.is_err() {
                    break;
                }
            }
            Ok(Event::WebSocket(Some(Ok(ServerMessage::Ping(data))))) => {
                if ws_out.send(ServerMessage::Pong(data)).await.is_err() {
                    break;
                }
            }
            Ok(Event::WebSocket(Some(Ok(ServerMessage::Pong(_))))) => {}
            Ok(Event::WebSocket(Some(Ok(ServerMessage::Close(_)))))
            | Ok(Event::WebSocket(None))
            | Ok(Event::WebSocket(Some(Err(_)))) => break,
            Ok(Event::WebSocket(Some(Ok(ServerMessage::Text(_))))) => break,
            Ok(Event::Tcp(Ok(0))) | Ok(Event::Tcp(Err(_))) => break,
            Ok(Event::Tcp(Ok(size))) => {
                sent += size as u64;
                if ws_out
                    .send(ServerMessage::Binary(buffer[..size].to_vec().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
    let _ = tcp_out.shutdown().await;
    let _ = ws_out.send(ServerMessage::Close(None)).await;
    tracing::info!(principal = %principal, received, sent, "tunnel closed");
}

pub async fn client(
    url: &str,
    listen: &str,
    token_file: Option<&Path>,
    identity: Option<&Path>,
) -> Result<()> {
    let credentials = match (token_file, identity) {
        (Some(path), None) => ClientCredentials::Token(read_secret_file(path, "令牌")?),
        (None, Some(path)) => ClientCredentials::Identity(Box::new(read_private_key(path)?)),
        _ => bail!("必须且只能指定 --token-file 或 --identity"),
    };
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("无法监听 {listen}"))?;
    eprintln!("WebSocket TCP 隧道已启动：{listen} -> {url} -> {TARGET}；按 Ctrl+C 停止");
    loop {
        let (tcp, peer) = listener.accept().await.context("接受本地连接失败")?;
        let url = url.to_string();
        let credentials = credentials.clone();
        tokio::spawn(async move {
            if let Err(error) = client_connection(tcp, &url, &credentials).await {
                eprintln!("隧道连接失败（{peer}）：{error:#}");
            }
        });
    }
}

#[derive(Clone)]
enum ClientCredentials {
    Token(String),
    Identity(Box<PrivateKey>),
}

async fn client_connection(
    tcp: TcpStream,
    url: &str,
    credentials: &ClientCredentials,
) -> Result<()> {
    let mut request = url.into_client_request().context("无效的 WebSocket URL")?;
    match credentials {
        ClientCredentials::Token(token) => {
            request.headers_mut().insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}")).context("无效的认证令牌")?,
            );
        }
        ClientCredentials::Identity(private_key) => {
            add_signature_headers(request.headers_mut(), private_key)?;
        }
    }
    let (socket, response) = tokio_tungstenite::connect_async(request)
        .await
        .context("WebSocket 握手失败")?;
    if response.status() != 101 {
        bail!("WebSocket 握手返回 HTTP {}", response.status());
    }
    let (mut ws_out, mut ws_in) = socket.split();
    let (mut tcp_in, mut tcp_out) = tcp.into_split();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        tokio::select! {
            read = tcp_in.read(&mut buffer) => match read {
                Ok(0) | Err(_) => break,
                Ok(size) => ws_out.send(ClientMessage::Binary(buffer[..size].to_vec().into())).await?,
            },
            message = ws_in.next() => match message {
                Some(Ok(ClientMessage::Binary(data))) => tcp_out.write_all(&data).await?,
                Some(Ok(ClientMessage::Ping(data))) => ws_out.send(ClientMessage::Pong(data)).await?,
                Some(Ok(ClientMessage::Pong(_))) => {},
                Some(Ok(ClientMessage::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => break,
            }
        }
    }
    let _ = tcp_out.shutdown().await;
    let _ = ws_out.send(ClientMessage::Close(None)).await;
    Ok(())
}

fn check_secret_permissions(path: &Path, label: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("读取令牌文件元数据 {}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            bail!("{label}文件权限过宽，请执行 chmod 600 {}", path.display());
        }
    }
    Ok(())
}

fn read_secret_file(path: &Path, label: &str) -> Result<String> {
    check_secret_permissions(path, label)?;
    let value = std::fs::read_to_string(path)
        .with_context(|| format!("读取{label}文件 {}", path.display()))?;
    let value = value.trim().to_string();
    if value.is_empty() {
        bail!("{label}文件为空");
    }
    Ok(value)
}

fn read_private_key(path: &Path) -> Result<PrivateKey> {
    let text = read_secret_file(path, "私钥")?;
    let key = text
        .parse::<PrivateKey>()
        .context("OpenSSH 私钥格式无效或私钥已加密")?;
    if key.algorithm() != Algorithm::Ed25519 {
        bail!("仅支持 Ed25519 私钥");
    }
    Ok(key)
}

fn add_signature_headers(headers: &mut HeaderMap, private_key: &PrivateKey) -> Result<()> {
    let timestamp = unix_timestamp().to_string();
    let nonce = uuid::Uuid::new_v4().to_string();
    let key_id = private_key.fingerprint(HashAlg::Sha256).to_string();
    let message = signature_message(&key_id, &timestamp, &nonce);
    let signature: Signature = private_key
        .try_sign(message.as_bytes())
        .context("私钥签名失败")?;
    insert_header(headers, KEY_HEADER, &key_id)?;
    insert_header(headers, TIME_HEADER, &timestamp)?;
    insert_header(headers, NONCE_HEADER, &nonce)?;
    insert_header(
        headers,
        SIGNATURE_HEADER,
        &BASE64.encode(signature.as_bytes()),
    )?;
    Ok(())
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &str) -> Result<()> {
    headers.insert(
        name,
        HeaderValue::from_str(value).context("隧道认证头无效")?,
    );
    Ok(())
}

fn signature_message(key_id: &str, timestamp: &str, nonce: &str) -> String {
    format!("{SIGNATURE_DOMAIN}\nGET\n/api/tunnel/ws\n{key_id}\n{timestamp}\n{nonce}")
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn env_bool(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES")
    )
}
fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct ConnectionPermit;
impl ConnectionPermit {
    fn acquire(max: usize) -> Option<Self> {
        ACTIVE_CONNECTIONS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < max).then_some(n + 1)
            })
            .ok()?;
        Some(Self)
    }
}
impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_limit_is_enforced() {
        ACTIVE_CONNECTIONS.store(0, Ordering::Release);
        let first = ConnectionPermit::acquire(1).unwrap();
        assert!(ConnectionPermit::acquire(1).is_none());
        drop(first);
        assert!(ConnectionPermit::acquire(1).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn token_file_must_not_be_group_or_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let path =
            std::env::temp_dir().join(format!("cangling-tunnel-{}.token", uuid::Uuid::new_v4()));
        std::fs::write(&path, "secret-token\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_secret_file(&path, "令牌").is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_secret_file(&path, "令牌").unwrap(), "secret-token");
        std::fs::remove_file(path).unwrap();
    }
}
