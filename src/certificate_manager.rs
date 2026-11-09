use crate::cluster::init::ensure_loadbalance;
use crate::error::AppError;
use crate::hostinfo;
use crate::state::AppState;
use axum::extract::State;
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use qrcode::{render::svg, QrCode};
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
const HELP_DIR: &str = "/opt/cangling/loadbalance/help-cert";
const HELP_PAGE: &str = "/opt/cangling/loadbalance/help-cert/index.html";
const HELP_CA_CERT: &str = "/opt/cangling/loadbalance/help-cert/cangling-root-ca.crt";
const EMBEDDED_CA_KEY: &[u8] = include_bytes!("assets/cangling-root-ca.key.pem");
const EMBEDDED_CA_CERT: &[u8] = include_bytes!("assets/cangling-root-ca.crt.pem");

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/help-cert", get(help_page))
        .route("/help-cert/", get(help_page))
        .route("/help-cert/cangling-root-ca.crt", get(download_root))
        .route("/api/certificates", get(status))
        .route("/api/certificates/root", get(download_root).post(init_root))
        .route(
            "/api/certificates/loadbalance/access-qr.svg",
            get(loadbalance_access_qr),
        )
        .route("/api/certificates/loadbalance", post(issue_loadbalance))
}

async fn help_page() -> Result<Html<String>, AppError> {
    ensure_embedded_root()?;
    ensure_help_page()?;
    let page = std::fs::read_to_string(HELP_PAGE)
        .map_err(|error| AppError::internal(format!("读取证书帮助页面失败：{error}")))?;
    Ok(Html(page))
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
    loadbalance_fingerprint: String,
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
    ensure_help_page()?;
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
    let loadbalance_fingerprint = certificate_fingerprint(SERVER_CERT);
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
        loadbalance_fingerprint,
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

async fn loadbalance_access_qr() -> Result<impl IntoResponse, AppError> {
    ensure_embedded_root()?;
    let suggested_ip = hostinfo::primary_ip();
    let loadbalance_ip = std::fs::read_to_string(SERVER_IP)
        .unwrap_or_default()
        .trim()
        .to_string();
    let host = if loadbalance_ip.is_empty() {
        &suggested_ip
    } else {
        &loadbalance_ip
    };
    let access_url = format!("https://{host}:7443/update/");
    let image = access_qr_svg(&access_url)?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache, must-revalidate"),
        ],
        image,
    ))
}

fn access_qr_svg(access_url: &str) -> Result<String, AppError> {
    let code = QrCode::new(access_url)
        .map_err(|error| AppError::internal(format!("生成访问地址二维码失败：{error}")))?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(160, 160)
        .dark_color(svg::Color("#263746"))
        .light_color(svg::Color("#fff8d5"))
        .build())
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
    ensure_help_page()?;
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
    ensure_help_page()?;
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
    parse_certificate_fingerprint(&String::from_utf8_lossy(&output.stdout))
}

fn parse_certificate_fingerprint(output: &str) -> String {
    output
        .lines()
        .find_map(|line| {
            let (label, value) = line.trim().split_once('=')?;
            let normalized = label
                .chars()
                .filter(|character| !character.is_ascii_whitespace())
                .flat_map(char::to_lowercase)
                .collect::<String>();
            (normalized == "sha256fingerprint").then(|| value.trim().to_string())
        })
        .unwrap_or_default()
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
    ensure_help_page()?;
    Ok(true)
}

/// Generates the static certificate help site served by the load balancer.
/// The CA certificate is public material; the encrypted private key is never
/// copied into the web directory.
pub(crate) fn ensure_help_page() -> Result<(), AppError> {
    if !Path::new(CA_CERT).is_file() {
        return Ok(());
    }
    std::fs::create_dir_all(HELP_DIR)?;
    set_mode(Path::new(HELP_DIR), 0o755)?;
    std::fs::copy(CA_CERT, HELP_CA_CERT)?;
    set_mode(Path::new(HELP_CA_CERT), 0o644)?;

    let primary_ip = hostinfo::primary_ip();
    let domains = std::fs::read_to_string(SERVER_DOMAINS)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let fingerprint = certificate_fingerprint(CA_CERT);
    std::fs::write(
        HELP_PAGE,
        build_help_page(&primary_ip, &domains, &fingerprint),
    )?;
    set_mode(Path::new(HELP_PAGE), 0o644)
}

fn build_help_page(primary_ip: &str, domains: &[String], fingerprint: &str) -> String {
    let preferred_host = domains.first().map(String::as_str).unwrap_or(primary_ip);
    let title_host = html_escape(preferred_host);
    let primary_ip = html_escape(primary_ip);
    let local_domain = html_escape(domains.first().map(String::as_str).unwrap_or("4np.tjj"));
    let domain_note = if domains.is_empty() {
        format!(
            "当前证书尚未包含域名。使用 <code>{local_domain}</code> 前，请先在证书管理中重新签发并加入该域名。"
        )
    } else {
        format!("当前证书已包含域名 <code>{local_domain}</code>。")
    };
    let fingerprint = html_escape(fingerprint);
    format!(
        r##"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>苍灵根证书安装帮助</title>
  <style>
    :root {{ color-scheme:light; --accent:#3884ff; --line:#e6e9ed; --muted:#6b7480; --sidebar:#f7f8fa; }}
    * {{ box-sizing:border-box; }} html {{ scroll-behavior:smooth; }} body {{ margin:0; color:#24292f; background:#fff; font:15px/1.75 system-ui,-apple-system,"Segoe UI","Microsoft YaHei",sans-serif; }}
    aside {{ position:fixed; inset:0 auto 0 0; width:250px; padding:28px 24px; overflow:auto; border-right:1px solid var(--line); background:var(--sidebar); }}
    aside strong {{ display:block; margin-bottom:18px; font-size:17px; }} nav a {{ display:block; padding:6px 0; color:#57606a; text-decoration:none; }} nav a:hover {{ color:var(--accent); }}
    main {{ margin-left:250px; padding:52px 48px 80px; }} article {{ width:860px; max-width:100%; margin:0 auto; }}
    h1 {{ margin:0 0 10px; font-size:34px; line-height:1.25; }} h2 {{ margin:42px 0 14px; padding-bottom:8px; border-bottom:1px solid var(--line); font-size:23px; }} h3 {{ margin:24px 0 8px; font-size:17px; }}
    p {{ margin:8px 0; }} .muted {{ color:var(--muted); }} a {{ color:var(--accent); }} .download {{ display:inline-block; margin:16px 0; padding:8px 14px; border:1px solid var(--accent); border-radius:4px; text-decoration:none; font-weight:600; }}
    ol,ul {{ padding-left:24px; }} code {{ padding:2px 5px; border-radius:3px; background:#f1f3f5; font-family:ui-monospace,SFMono-Regular,Consolas,monospace; overflow-wrap:anywhere; }}
    pre {{ overflow:auto; padding:13px 15px; border:1px solid var(--line); border-radius:4px; background:#f7f8fa; font-size:13px; }} .fingerprint {{ word-break:break-all; font-family:ui-monospace,SFMono-Regular,Consolas,monospace; }}
    .note {{ padding:12px 14px; border-left:3px solid var(--accent); background:#f5f9ff; }} .warning {{ border-left-color:#d29922; background:#fff8e6; }}
    @media(max-width:800px) {{ aside {{ position:static; width:auto; border-right:0; border-bottom:1px solid var(--line); }} nav {{ display:flex; gap:8px 18px; flex-wrap:wrap; }} main {{ width:auto; padding:28px 20px 60px; }} }}
  </style>
</head>
<body>
<aside><strong>证书使用指南</strong><nav>
  <a href="#download">下载根证书</a><a href="#hosts">配置本地域名</a><a href="#windows">Windows</a><a href="#firefox">Firefox</a><a href="#macos">macOS</a><a href="#linux">Linux</a><a href="#security">安全提示</a>
</nav></aside>
<main><article>
  <header id="download">
    <h1>苍灵根证书安装帮助</h1>
    <p class="muted">安装根证书后，浏览器才能信任负载均衡的内部 HTTPS 证书。</p>
    <a class="download" href="cangling-root-ca.crt" download>下载苍灵根证书</a>
    <p>HTTPS 入口：<a href="https://{title_host}:7443/">https://{title_host}:7443/</a></p>
    <p>SHA-256 指纹：<span class="fingerprint">{fingerprint}</span></p>
  </header>

  <section id="hosts"><h2>配置本地域名</h2>
    <p>如果局域网没有 DNS 服务，可通过本机 <code>hosts</code> 文件把域名指向 master。{domain_note}</p>
    <h3>Windows</h3><p>使用管理员权限打开记事本，编辑：</p><pre>C:\Windows\System32\drivers\etc\hosts</pre>
    <p>在文件末尾添加：</p><pre>{primary_ip}    {local_domain}</pre>
    <p>保存后执行 <code>ipconfig /flushdns</code>，再访问 <a href="https://{local_domain}:7443/">https://{local_domain}:7443/</a>。</p>
    <h3>Linux / macOS</h3><p>执行：</p><pre>echo "{primary_ip}    {local_domain}" | sudo tee -a /etc/hosts</pre>
    <p>然后访问 <a href="https://{local_domain}:7443/">https://{local_domain}:7443/</a>。如已有同名记录，请修改原记录，不要重复添加。</p>
  </section>

  <section id="windows"><h2>Windows · Chrome / Edge</h2><ol>
      <li>下载并双击 <code>cangling-root-ca.crt</code>。</li><li>选择“安装证书”，存储位置选择“本地计算机”。</li>
      <li>选择“将所有证书放入下列存储”，指定“受信任的根证书颁发机构”。</li><li>完成导入后彻底退出并重新打开浏览器。</li>
    </ol></section>
    <section id="firefox"><h2>Firefox</h2><ol>
      <li>打开“设置 → 隐私与安全 → 证书 → 查看证书”。</li><li>在“证书颁发机构”页点击“导入”。</li>
      <li>选择根证书，并勾选“信任此 CA 以标识网站”。</li>
    </ol></section>
    <section id="macos"><h2>macOS · Safari / Chrome</h2><ol>
      <li>用“钥匙串访问”打开下载的根证书，添加到“系统”钥匙串。</li><li>双击证书，展开“信任”，将“使用此证书时”设为“始终信任”。</li>
      <li>输入管理员密码确认，然后重新打开浏览器。</li>
    </ol></section>
    <section id="linux"><h2>Linux</h2>
      <h3>Kylin / RHEL / CentOS</h3><pre>sudo cp cangling-root-ca.crt /etc/pki/ca-trust/source/anchors/
sudo update-ca-trust extract</pre>
      <h3>Ubuntu / Debian</h3><pre>sudo cp cangling-root-ca.crt /usr/local/share/ca-certificates/
sudo update-ca-certificates</pre>
      <h3>Chrome / Edge 仍不信任</h3><p>部分版本还需要导入当前用户的 NSS 数据库：</p><pre>certutil -A -d sql:$HOME/.pki/nssdb \
  -n "Cangling Local Root CA" -t "CT,C,C" \
  -i cangling-root-ca.crt</pre><p class="muted">缺少 certutil 时安装 <code>nss-tools</code> 或 <code>libnss3-tools</code>。完成后重新启动浏览器。</p>
    </section>
  <section id="security"><h2>安全提示</h2><p class="note warning">根证书会使本机信任由该 CA 签发的内部站点证书。请只在受管理设备上安装，并在导入前核对上方 SHA-256 指纹。不要下载或分发根证书私钥。</p></section>
</article></main></body></html>"##
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
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

    #[test]
    fn help_page_contains_download_and_platform_instructions() {
        let html = build_help_page("192.168.3.121", &["4np.tjj".to_string()], "AA:BB:CC");
        assert!(html.contains("cangling-root-ca.crt"));
        assert!(html.contains("https://4np.tjj:7443/"));
        assert!(html.contains("Windows · Chrome / Edge"));
        assert!(html.contains("Kylin / RHEL / CentOS"));
        assert!(html.contains("C:\\Windows\\System32\\drivers\\etc\\hosts"));
        assert!(html.contains("192.168.3.121    4np.tjj"));
        assert!(html.contains("/etc/hosts"));
        assert!(html.contains("AA:BB:CC"));
    }

    #[test]
    fn parses_openssl_fingerprint_variants() {
        assert_eq!(
            parse_certificate_fingerprint("SHA256 Fingerprint=AA:BB:CC\n"),
            "AA:BB:CC"
        );
        assert_eq!(
            parse_certificate_fingerprint("sha256 Fingerprint=11:22:33\n"),
            "11:22:33"
        );
    }

    #[test]
    fn access_qr_is_self_contained_svg() {
        let svg = access_qr_svg("https://4np.tjj:7443/update/").unwrap();
        assert!(svg.starts_with("<?xml"));
        assert!(svg.contains("<svg"));
        assert!(svg.contains("#263746"));
        assert!(svg.contains("#fff8d5"));
    }
}
