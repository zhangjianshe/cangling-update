use crate::cluster::init::ensure_loadbalance;
use crate::error::AppError;
use crate::hostinfo;
use crate::state::AppState;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use uuid::Uuid;

const CERT_DIR: &str = "/opt/cangling/loadbalance/certs";
const CA_KEY: &str = "/opt/cangling/loadbalance/certs/ca.key.pem";
const CA_CERT: &str = "/opt/cangling/loadbalance/certs/ca.crt.pem";
const SERVER_KEY: &str = "/opt/cangling/loadbalance/certs/loadbalance.key.pem";
const SERVER_CERT: &str = "/opt/cangling/loadbalance/certs/loadbalance.crt.pem";
const SERVER_IP: &str = "/opt/cangling/loadbalance/certs/loadbalance.ip";
const SERVER_DOMAINS: &str = "/opt/cangling/loadbalance/certs/loadbalance.domains";
const EMBEDDED_CA_KEY: &[u8] = include_bytes!("assets/cangling-root-ca.key.pem");
const EMBEDDED_CA_CERT: &[u8] = include_bytes!("assets/cangling-root-ca.crt.pem");

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/certificates", get(status))
        .route("/api/certificates/root", get(download_root).post(init_root))
        .route("/api/certificates/loadbalance", post(issue_loadbalance))
}

#[derive(Debug, Serialize)]
struct CertificateStatus {
    root_initialized: bool,
    root_subject: String,
    root_expires: String,
    root_fingerprint: String,
    loadbalance_issued: bool,
    loadbalance_ip: String,
    loadbalance_domains: Vec<String>,
    loadbalance_subject: String,
    loadbalance_expires: String,
    suggested_ip: String,
    root_certificate_path: &'static str,
    loadbalance_certificate_path: &'static str,
    https_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InitRootRequest {
    password: String,
    #[serde(default)]
    common_name: String,
}

#[derive(Debug, Deserialize)]
struct IssueRequest {
    password: String,
    ip: String,
    #[serde(default)]
    domains: String,
}

#[derive(Debug, Serialize)]
struct ActionResult {
    ok: bool,
    message: String,
}

async fn status() -> Result<Json<CertificateStatus>, AppError> {
    ensure_embedded_root()?;
    let root_initialized = Path::new(CA_KEY).is_file() && Path::new(CA_CERT).is_file();
    let loadbalance_issued = Path::new(SERVER_KEY).is_file() && Path::new(SERVER_CERT).is_file();
    let suggested_ip = hostinfo::primary_ip();
    let loadbalance_ip = std::fs::read_to_string(SERVER_IP)
        .unwrap_or_default()
        .trim()
        .to_string();
    let loadbalance_domains: Vec<String> = std::fs::read_to_string(SERVER_DOMAINS)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    let (root_subject, root_expires) = certificate_summary(CA_CERT);
    let root_fingerprint = certificate_fingerprint(CA_CERT);
    let (loadbalance_subject, loadbalance_expires) = certificate_summary(SERVER_CERT);
    let https_url = loadbalance_issued.then(|| {
        format!(
            "https://{}:7443/update/",
            if let Some(domain) = loadbalance_domains.first() {
                domain
            } else if loadbalance_ip.is_empty() {
                &suggested_ip
            } else {
                &loadbalance_ip
            }
        )
    });
    Ok(Json(CertificateStatus {
        root_initialized,
        root_subject,
        root_expires,
        root_fingerprint,
        loadbalance_issued,
        loadbalance_ip,
        loadbalance_domains,
        loadbalance_subject,
        loadbalance_expires,
        suggested_ip,
        root_certificate_path: CA_CERT,
        loadbalance_certificate_path: SERVER_CERT,
        https_url,
    }))
}

async fn download_root() -> Result<impl IntoResponse, AppError> {
    ensure_embedded_root()?;
    let certificate = std::fs::read(CA_CERT)
        .map_err(|error| AppError::internal(format!("读取根证书失败：{error}")))?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-x509-ca-cert"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"cangling-root-ca.crt\"",
            ),
        ],
        certificate,
    ))
}

async fn init_root(Json(body): Json<InitRootRequest>) -> Result<Json<ActionResult>, AppError> {
    ensure_embedded_root()?;
    validate_password(&body.password)?;
    if Path::new(CA_KEY).exists() || Path::new(CA_CERT).exists() {
        return Err(AppError::conflict(
            "根证书已经存在；为保护现有证书链，不允许直接覆盖",
        ));
    }
    let common_name = if body.common_name.trim().is_empty() {
        "Cangling Local Root CA"
    } else {
        body.common_name.trim()
    };
    validate_common_name(common_name)?;
    prepare_cert_dir()?;
    require_openssl()?;

    let nonce = Uuid::new_v4();
    let temp_key = PathBuf::from(CERT_DIR).join(format!(".ca-{nonce}.key"));
    let temp_cert = PathBuf::from(CERT_DIR).join(format!(".ca-{nonce}.crt"));
    let result = (|| -> Result<(), AppError> {
        run_openssl(
            &[
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:4096",
                "-aes-256-cbc",
                "-pass",
                "env:CANGLING_CA_PASSWORD",
                "-out",
                path_text(&temp_key)?,
            ],
            Some(&body.password),
        )?;
        run_openssl(
            &[
                "req",
                "-x509",
                "-new",
                "-sha256",
                "-days",
                "3650",
                "-key",
                path_text(&temp_key)?,
                "-passin",
                "env:CANGLING_CA_PASSWORD",
                "-subj",
                &format!("/CN={common_name}"),
                "-addext",
                "basicConstraints=critical,CA:TRUE,pathlen:1",
                "-addext",
                "keyUsage=critical,keyCertSign,cRLSign",
                "-out",
                path_text(&temp_cert)?,
            ],
            Some(&body.password),
        )?;
        set_mode(&temp_key, 0o600)?;
        set_mode(&temp_cert, 0o644)?;
        std::fs::rename(&temp_key, CA_KEY)?;
        std::fs::rename(&temp_cert, CA_CERT)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temp_key);
    let _ = std::fs::remove_file(&temp_cert);
    result?;
    Ok(Json(ActionResult {
        ok: true,
        message: "根证书已创建，私钥使用 AES-256 密码保护，有效期 10 年".to_string(),
    }))
}

async fn issue_loadbalance(
    State(state): State<AppState>,
    Json(body): Json<IssueRequest>,
) -> Result<Json<ActionResult>, AppError> {
    ensure_embedded_root()?;
    validate_password(&body.password)?;
    let ip: IpAddr = body
        .ip
        .trim()
        .parse()
        .map_err(|_| AppError::bad("请输入有效的 IPv4 或 IPv6 地址"))?;
    let domains = parse_domains(&body.domains)?;
    if !Path::new(CA_KEY).is_file() || !Path::new(CA_CERT).is_file() {
        return Err(AppError::bad("请先初始化根证书"));
    }
    prepare_cert_dir()?;
    require_openssl()?;
    run_openssl(
        &[
            "pkey",
            "-in",
            CA_KEY,
            "-passin",
            "env:CANGLING_CA_PASSWORD",
            "-noout",
        ],
        Some(&body.password),
    )?;

    let nonce = Uuid::new_v4();
    let base = PathBuf::from(CERT_DIR);
    let temp_key = base.join(format!(".loadbalance-{nonce}.key"));
    let temp_csr = base.join(format!(".loadbalance-{nonce}.csr"));
    let temp_cert = base.join(format!(".loadbalance-{nonce}.crt"));
    let temp_ext = base.join(format!(".loadbalance-{nonce}.ext"));
    let mut san = vec![format!("IP:{ip}")];
    san.extend(domains.iter().map(|domain| format!("DNS:{domain}")));
    let ext = format!(
        "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName={}\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid,issuer\n",
        san.join(",")
    );
    std::fs::write(&temp_ext, ext)?;
    let result = (|| -> Result<(), AppError> {
        run_openssl(
            &[
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:3072",
                "-out",
                path_text(&temp_key)?,
            ],
            None,
        )?;
        run_openssl(
            &[
                "req",
                "-new",
                "-key",
                path_text(&temp_key)?,
                "-subj",
                &format!(
                    "/CN={}",
                    domains
                        .first()
                        .map(String::as_str)
                        .unwrap_or(body.ip.trim())
                ),
                "-out",
                path_text(&temp_csr)?,
            ],
            None,
        )?;
        run_openssl(
            &[
                "x509",
                "-req",
                "-sha256",
                "-days",
                "825",
                "-in",
                path_text(&temp_csr)?,
                "-CA",
                CA_CERT,
                "-CAkey",
                CA_KEY,
                "-passin",
                "env:CANGLING_CA_PASSWORD",
                "-CAserial",
                "/opt/cangling/loadbalance/certs/ca.srl",
                "-CAcreateserial",
                "-extfile",
                path_text(&temp_ext)?,
                "-out",
                path_text(&temp_cert)?,
            ],
            Some(&body.password),
        )?;
        set_mode(&temp_key, 0o600)?;
        set_mode(&temp_cert, 0o644)?;
        std::fs::rename(&temp_key, SERVER_KEY)?;
        std::fs::rename(&temp_cert, SERVER_CERT)?;
        std::fs::write(SERVER_IP, format!("{ip}\n"))?;
        set_mode(Path::new(SERVER_IP), 0o644)?;
        std::fs::write(
            SERVER_DOMAINS,
            if domains.is_empty() {
                String::new()
            } else {
                format!("{}\n", domains.join("\n"))
            },
        )?;
        set_mode(Path::new(SERVER_DOMAINS), 0o644)?;
        Ok(())
    })();
    for path in [&temp_key, &temp_csr, &temp_cert, &temp_ext] {
        let _ = std::fs::remove_file(path);
    }
    result?;
    let loadbalance = ensure_loadbalance(&state)?;
    Ok(Json(ActionResult {
        ok: true,
        message: format!(
            "已为 {ip}{}签发负载均衡证书并启用 HTTPS 7443；{loadbalance}",
            if domains.is_empty() {
                String::new()
            } else {
                format!("（{}）", domains.join("、"))
            }
        ),
    }))
}

fn certificate_summary(path: &str) -> (String, String) {
    if !Path::new(path).is_file() {
        return (String::new(), String::new());
    }
    let output = Command::new("openssl")
        .args(["x509", "-in", path, "-noout", "-subject", "-enddate"])
        .output();
    let Ok(output) = output else {
        return (String::new(), String::new());
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let subject = text
        .lines()
        .find_map(|line| line.strip_prefix("subject="))
        .unwrap_or_default()
        .trim()
        .to_string();
    let expires = text
        .lines()
        .find_map(|line| line.strip_prefix("notAfter="))
        .unwrap_or_default()
        .trim()
        .to_string();
    let expires = chrono::NaiveDateTime::parse_from_str(&expires, "%b %e %H:%M:%S %Y GMT")
        .map(|value| value.and_utc().to_rfc3339())
        .unwrap_or(expires);
    (subject, expires)
}

fn certificate_fingerprint(path: &str) -> String {
    let output = Command::new("openssl")
        .args(["x509", "-in", path, "-noout", "-fingerprint", "-sha256"])
        .output();
    let Ok(output) = output else {
        return String::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .strip_prefix("sha256 Fingerprint=")
        .unwrap_or_default()
        .to_string()
}

fn validate_password(password: &str) -> Result<(), AppError> {
    if password.len() < 12 {
        return Err(AppError::bad("根证书密码至少需要 12 个字符"));
    }
    if password.as_bytes().contains(&0) || password.contains('\n') || password.contains('\r') {
        return Err(AppError::bad("根证书密码包含不允许的字符"));
    }
    Ok(())
}

fn validate_common_name(value: &str) -> Result<(), AppError> {
    if value.len() > 100
        || value.contains('/')
        || value.chars().any(char::is_control)
        || value.is_empty()
    {
        return Err(AppError::bad("根证书名称无效"));
    }
    Ok(())
}

fn parse_domains(value: &str) -> Result<Vec<String>, AppError> {
    let mut domains = Vec::new();
    for domain in value
        .split(|character: char| character == ',' || character == ';' || character.is_whitespace())
        .map(str::trim)
        .filter(|domain| !domain.is_empty())
    {
        let domain = domain.to_ascii_lowercase();
        let valid = domain.len() <= 253
            && domain.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '-')
            });
        if !valid {
            return Err(AppError::bad(format!("域名无效：{domain}")));
        }
        if !domains.contains(&domain) {
            domains.push(domain);
        }
    }
    Ok(domains)
}

fn prepare_cert_dir() -> Result<(), AppError> {
    std::fs::create_dir_all(CERT_DIR)?;
    set_mode(Path::new(CERT_DIR), 0o700)
}

/// Installs the encrypted, deployment-wide CA only when no CA exists locally.
/// Existing certificate material is never replaced, including partial pairs.
pub(crate) fn ensure_embedded_root() -> Result<bool, AppError> {
    let key_exists = Path::new(CA_KEY).exists();
    let cert_exists = Path::new(CA_CERT).exists();
    if key_exists && cert_exists {
        return Ok(false);
    }
    if key_exists || cert_exists {
        return Err(AppError::conflict(
            "根证书文件不完整；为避免覆盖现有密钥，未安装内置根证书",
        ));
    }
    prepare_cert_dir()?;
    let nonce = Uuid::new_v4();
    let temp_key = PathBuf::from(CERT_DIR).join(format!(".embedded-ca-{nonce}.key"));
    let temp_cert = PathBuf::from(CERT_DIR).join(format!(".embedded-ca-{nonce}.crt"));
    let result = (|| -> Result<(), AppError> {
        write_new(&temp_key, EMBEDDED_CA_KEY, 0o600)?;
        write_new(&temp_cert, EMBEDDED_CA_CERT, 0o644)?;
        std::fs::rename(&temp_key, CA_KEY)?;
        std::fs::rename(&temp_cert, CA_CERT)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temp_key);
    let _ = std::fs::remove_file(&temp_cert);
    result?;
    Ok(true)
}

fn write_new(path: &Path, content: &[u8], mode: u32) -> Result<(), AppError> {
    use std::io::Write;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let mut file = options.open(path)?;
    file.write_all(content)?;
    file.sync_all()?;
    set_mode(path, mode)
}

fn require_openssl() -> Result<(), AppError> {
    let output = Command::new("openssl")
        .arg("version")
        .output()
        .map_err(|error| AppError::internal(format!("OpenSSL 不可用：{error}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error("OpenSSL 不可用", &output))
    }
}

fn run_openssl(args: &[&str], password: Option<&str>) -> Result<(), AppError> {
    let mut command = Command::new("openssl");
    command.args(args);
    if let Some(password) = password {
        command.env("CANGLING_CA_PASSWORD", password);
    }
    let output = command
        .output()
        .map_err(|error| AppError::internal(format!("执行 OpenSSL 失败：{error}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error("OpenSSL 操作失败，请检查根证书密码", &output))
    }
}

fn command_error(prefix: &str, output: &Output) -> AppError {
    let detail = String::from_utf8_lossy(&output.stderr);
    AppError::bad(format!("{prefix}：{}", detail.trim()))
}

fn path_text(path: &Path) -> Result<&str, AppError> {
    path.to_str()
        .ok_or_else(|| AppError::internal("证书路径不是有效 UTF-8"))
}

fn set_mode(path: &Path, mode: u32) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_certificate_inputs() {
        assert!(validate_password("short").is_err());
        assert!(validate_password("StrongPassword-2026").is_ok());
        assert!(validate_common_name("Cangling Root CA").is_ok());
        assert!(validate_common_name("bad/name").is_err());
        assert_eq!(
            parse_domains("4np.tjj, NP4.local").unwrap(),
            ["4np.tjj", "np4.local"]
        );
        assert!(parse_domains("bad_name.local").is_err());
        assert!("192.168.3.121".parse::<IpAddr>().is_ok());
    }
}
