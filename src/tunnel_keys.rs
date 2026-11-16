//! WSS authorization management. This module never accepts account private keys.
use crate::{error::AppError, state::AppState};
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use ssh_key::{Algorithm, HashAlg, PublicKey};
use std::{fs, path::Path, sync::Mutex, time::Duration};

static KEYS_LOCK: Mutex<()> = Mutex::new(());
const FILE: &str = "tunnel_authorized_keys";
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyInfo {
    pub name: String,
    pub public_key: String,
    pub fingerprint: String,
    pub algorithm: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Grant {
    public_key: String,
    #[serde(default)]
    name: String,
}
#[derive(Deserialize)]
struct Revoke {
    fingerprint: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Account {
    server_url: String,
    username: String,
    password: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/tunnel/keys", get(list).post(grant).delete(revoke))
        .route("/api/tunnel/keys/account", post(account_keys))
}
fn key_info(key: &PublicKey) -> Result<KeyInfo, AppError> {
    Ok(KeyInfo {
        name: key.comment().to_string(),
        public_key: key.to_openssh().map_err(|e| AppError::bad(e.to_string()))?,
        fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
        algorithm: key.algorithm().to_string(),
    })
}
fn read_keys(dir: &Path) -> Result<Vec<PublicKey>, AppError> {
    let path = dir.join(FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    fs::read_to_string(path)?
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let line = line.trim();
            (!line.is_empty() && !line.starts_with('#')).then_some((i, line))
        })
        .map(|(i, line)| {
            line.parse().map_err(|_| {
                AppError::bad(format!("授权文件第 {} 行公钥格式无效；请先修复该行", i + 1))
            })
        })
        .collect()
}
fn write_keys(dir: &Path, keys: &[PublicKey]) -> Result<(), AppError> {
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".tunnel-keys-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        use std::io::Write;
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        for key in keys {
            writeln!(
                file,
                "{}",
                key.to_openssh().map_err(|e| AppError::bad(e.to_string()))?
            )?;
        }
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, dir.join(FILE))?;
        Ok(())
    })();
    if tmp.exists() {
        let _ = fs::remove_file(tmp);
    }
    result
}
fn add_key(dir: &Path, body: Grant) -> Result<KeyInfo, AppError> {
    let _guard = KEYS_LOCK
        .lock()
        .map_err(|_| AppError::internal("公钥文件锁错误"))?;
    if body.public_key.len() > 8192 {
        return Err(AppError::bad("公钥过长"));
    }
    let public_text = body.public_key.trim();
    if public_text.chars().any(char::is_control) {
        return Err(AppError::bad(
            "每次只能授权一个公钥，请勿包含多行或控制字符",
        ));
    }
    let mut key: PublicKey = body
        .public_key
        .trim()
        .parse()
        .map_err(|_| AppError::bad("请输入有效的 OpenSSH Ed25519 公钥"))?;
    if key.algorithm() != Algorithm::Ed25519 {
        return Err(AppError::bad("WSS 隧道仅支持 Ed25519 公钥"));
    }
    let name = body.name.trim();
    if name.len() > 512 || name.chars().any(char::is_control) {
        return Err(AppError::bad("名称过长或包含换行等控制字符"));
    }
    if !name.is_empty() {
        key.set_comment(name);
    }
    let fingerprint = key.fingerprint(HashAlg::Sha256);
    let mut keys = read_keys(dir)?;
    keys.retain(|existing| existing.fingerprint(HashAlg::Sha256) != fingerprint);
    let info = key_info(&key)?;
    keys.push(key);
    write_keys(dir, &keys)?;
    Ok(info)
}
fn remove_key(dir: &Path, fingerprint: &str) -> Result<(), AppError> {
    if !fingerprint.starts_with("SHA256:") || fingerprint.len() > 128 {
        return Err(AppError::bad("无效的公钥指纹"));
    }
    let _guard = KEYS_LOCK
        .lock()
        .map_err(|_| AppError::internal("公钥文件锁错误"))?;
    let mut keys = read_keys(dir)?;
    keys.retain(|key| key.fingerprint(HashAlg::Sha256).to_string() != fingerprint);
    write_keys(dir, &keys)
}
async fn list(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let _guard = KEYS_LOCK
        .lock()
        .map_err(|_| AppError::internal("公钥文件锁错误"))?;
    let keys = read_keys(&state.paths.config_dir)?
        .iter()
        .map(key_info)
        .collect::<Result<Vec<_>, _>>()?;
    let enabled = std::env::var("CANGLING_TUNNEL_ENABLED")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    Ok(Json(
        json!({"keys":keys,"path":state.paths.config_dir.join(FILE),"enabled":enabled}),
    ))
}
async fn grant(
    State(state): State<AppState>,
    Json(body): Json<Grant>,
) -> Result<Json<KeyInfo>, AppError> {
    let key = add_key(&state.paths.config_dir, body)?;
    tracing::info!(fingerprint=%key.fingerprint,"WSS tunnel key authorized");
    Ok(Json(key))
}
async fn revoke(
    State(state): State<AppState>,
    Json(body): Json<Revoke>,
) -> Result<Json<Value>, AppError> {
    remove_key(&state.paths.config_dir, &body.fingerprint)?;
    tracing::info!(fingerprint=%body.fingerprint,"WSS tunnel key revoked");
    Ok(Json(
        json!({"message":"已撤销公钥授权；后续新连接将被拒绝，现有连接不会被中断"}),
    ))
}
fn server_url(text: &str) -> Result<String, AppError> {
    let url = reqwest::Url::parse(text.trim()).map_err(|_| AppError::bad("账号服务器地址无效"))?;
    let local = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    if (url.scheme() != "https" && !(url.scheme() == "http" && local))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::bad(
            "账号服务器必须使用 HTTPS 地址，且不能包含密码或查询参数",
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}
async fn envelope(mut response: reqwest::Response) -> Result<Value, AppError> {
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(AppError::bad(
            "账号服务器尚未提供公钥列表接口，请更新 gwt-template",
        ));
    }
    let status = response.status();
    if !status.is_success() {
        return Err(AppError::bad(format!(
            "账号服务器返回 HTTP {}",
            status.as_u16()
        )));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| AppError::bad("读取账号服务器响应失败"))?
    {
        if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err(AppError::bad("账号服务器响应过大"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let body: Value =
        serde_json::from_slice(&bytes).map_err(|_| AppError::bad("账号服务器未返回有效 JSON"))?;
    if body["success"] != true || body["code"] != 200 {
        return Err(AppError::bad(
            body["message"]
                .as_str()
                .unwrap_or("账号认证或公钥读取失败")
                .chars()
                .take(500)
                .collect::<String>(),
        ));
    }
    Ok(body["data"].clone())
}
async fn account_keys(Json(body): Json<Account>) -> Result<Json<Value>, AppError> {
    let base = server_url(&body.server_url)?;
    if body.username.trim().is_empty() || body.password.is_empty() {
        return Err(AppError::bad("请输入账号和密码"));
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| AppError::internal("创建账号连接失败"))?;
    let response = client
        .post(format!("{base}/api/v1/user/login"))
        .json(&json!({"userName":body.username,"password":body.password}))
        .send()
        .await
        .map_err(|_| AppError::bad("无法连接账号服务器，请检查地址、网络和 HTTPS 证书信任"))?;
    let login = envelope(response).await?;
    let token = login["token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad("账号服务器未返回登录令牌"))?;
    let response = client
        .get(format!("{base}/api/v1/certificate/public-list"))
        .header("API-TOKEN", token)
        .send()
        .await
        .map_err(|_| AppError::bad("读取账号公钥列表失败"))?;
    let data = envelope(response).await?;
    let items = data["certificates"]
        .as_array()
        .ok_or_else(|| AppError::bad("公钥列表格式无效"))?;
    let mut keys = Vec::new();
    for item in items {
        let public = item["publicKey"]
            .as_str()
            .ok_or_else(|| AppError::bad("服务器证书缺少公钥"))?;
        let mut key: PublicKey = public
            .parse()
            .map_err(|_| AppError::bad("服务器证书公钥格式无效"))?;
        if key.algorithm() != Algorithm::Ed25519 {
            continue;
        }
        key.set_comment(item["name"].as_str().unwrap_or(""));
        keys.push(key_info(&key)?);
    }
    Ok(Json(json!({"keys":keys})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssh_key::PrivateKey;

    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("tunnel-keys-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn public() -> String {
        PrivateKey::random(&mut rand::rngs::OsRng, Algorithm::Ed25519)
            .unwrap()
            .public_key()
            .to_openssh()
            .unwrap()
    }

    #[test]
    fn authorize_deduplicates_and_revoke_preserves_other_keys() {
        let dir = Directory::new();
        let first = public();
        let a = add_key(
            &dir.0,
            Grant {
                public_key: first.clone(),
                name: "Windows Keeper".into(),
            },
        )
        .unwrap();
        let b = add_key(
            &dir.0,
            Grant {
                public_key: public(),
                name: "Linux Keeper".into(),
            },
        )
        .unwrap();
        let renamed = add_key(
            &dir.0,
            Grant {
                public_key: first,
                name: "Renamed".into(),
            },
        )
        .unwrap();
        assert_eq!(renamed.fingerprint, a.fingerprint);
        assert_eq!(read_keys(&dir.0).unwrap().len(), 2);
        assert_eq!(renamed.name, "Renamed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.0.join(FILE)).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        remove_key(&dir.0, &a.fingerprint).unwrap();
        remove_key(&dir.0, &a.fingerprint).unwrap();
        let remaining = read_keys(&dir.0).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].fingerprint(HashAlg::Sha256).to_string(),
            b.fingerprint
        );
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    fn invalid_input_or_file_never_overwrites_existing_authorizations() {
        let dir = Directory::new();
        let path = dir.0.join(FILE);
        fs::write(&path, "malformed authorization\n").unwrap();
        assert!(add_key(
            &dir.0,
            Grant {
                public_key: public(),
                name: "valid".into()
            }
        )
        .is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "malformed authorization\n"
        );
        fs::write(&path, "# comment\n").unwrap();
        for (key, name) in [
            ("not-a-key".into(), "x".into()),
            (public(), "new\nentry".into()),
            (format!("{}\n{}", public(), public()), "x".into()),
        ] {
            assert!(add_key(
                &dir.0,
                Grant {
                    public_key: key,
                    name
                }
            )
            .is_err());
        }
        assert_eq!(fs::read_to_string(path).unwrap(), "# comment\n");
    }

    #[test]
    fn management_requires_login_and_account_url_requires_https() {
        for path in [
            "/api/tunnel/keys",
            "/api/tunnel/keys/account",
            "/update/api/tunnel/keys",
        ] {
            for method in [
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::DELETE,
            ] {
                assert!(!crate::auth::is_public(&method, path));
            }
        }
        assert!(server_url("https://soft.cangling.cn:22002/").is_ok());
        assert!(server_url("http://127.0.0.1:1234").is_ok());
        for url in [
            "http://example.com",
            "https://user:secret@example.com",
            "https://example.com?token=secret",
            "https://example.com/#token",
            "file:///tmp/file",
        ] {
            assert!(server_url(url).is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn account_import_uses_only_public_endpoint_and_keeps_no_private_fields() {
        use axum::http::HeaderMap;
        let public_key = public();
        let expected = public_key.clone();
        let server = Router::new()
            .route("/api/v1/user/login", post(|Json(body): Json<Value>| async move {
                assert_eq!(body["userName"], "test-account");
                assert_eq!(body["password"], "test-password");
                Json(json!({"success":true,"code":200,"data":{"token":"test-token"}}))
            }))
            .route("/api/v1/certificate/public-list", get(move |headers: HeaderMap| async move {
                assert_eq!(headers["API-TOKEN"], "test-token");
                Json(json!({"success":true,"code":200,"data":{"certificates":[{"name":"Windows", "publicKey":public_key}]}}))
            }))
            .route("/api/v1/certificate/list", get(|| async {
                axum::http::StatusCode::FORBIDDEN
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, server).await.unwrap();
        });
        let response = account_keys(Json(Account {
            server_url: format!("http://{addr}"),
            username: "test-account".into(),
            password: "test-password".into(),
        }))
        .await
        .unwrap()
        .0;
        task.abort();
        assert_eq!(response["keys"][0]["name"], "Windows");
        assert_eq!(
            response["keys"][0]["publicKey"]
                .as_str()
                .unwrap()
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>(),
            expected.split_whitespace().take(2).collect::<Vec<_>>()
        );
        assert!(response["keys"][0].get("privateKey").is_none());
        assert!(!response.to_string().contains("test-token"));
    }
}
