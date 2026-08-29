//! worker 注册/心跳时，master 若发现其 `cangling-update` 版本落后，则下发升级要约；
//! worker 按本机架构从 master 下载对应二进制，替换后重启。

use super::http;
use crate::binaries::{self, Arch, StoredBinary, MIN_BYTES};
use crate::error::AppError;
use crate::paths::AppPaths;
use crate::service;
use crate::state::AppState;
use anyhow::{bail, Context, Result};
use axum::extract::{Path, State};
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static APPLYING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeOffer {
    pub version: String,
    pub arch: String,
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct SelfUpdateStatus {
    pub version: String,
    pub binaries: Vec<StoredBinary>,
}

pub fn offer_for(
    paths: &AppPaths,
    worker_version: &str,
    worker_arch: &str,
) -> Option<UpgradeOffer> {
    let master = env!("CARGO_PKG_VERSION");
    if !binaries::needs_upgrade(master, worker_version) {
        return None;
    }
    let arch = Arch::parse(worker_arch)?;
    let info = binaries::stored(&paths.exe_dir, arch)?;
    Some(UpgradeOffer {
        version: master.to_string(),
        arch: arch.slug().to_string(),
        size: info.size,
    })
}

pub fn parse_offer(value: &serde_json::Value) -> Option<UpgradeOffer> {
    serde_json::from_value(value.get("upgrade")?.clone()).ok()
}

pub fn warn_if_missing(paths: &AppPaths, worker_version: &str, worker_arch: &str, node: &str) {
    let master = env!("CARGO_PKG_VERSION");
    if !binaries::needs_upgrade(master, worker_version) {
        return;
    }
    match Arch::parse(worker_arch) {
        None => tracing::warn!(
            "worker {node} 版本 {worker_version} 低于 master {master}，但无法识别架构 {worker_arch:?}"
        ),
        Some(arch) if binaries::stored(&paths.exe_dir, arch).is_none() => tracing::warn!(
            "worker {node}（{}）版本 {worker_version} 低于 master {master}，但 updates/{} 不存在。请把对应架构的二进制放到 {} 或执行 cangling-update update / update --import",
            arch.label(),
            arch.asset_name(),
            binaries::updates_dir(&paths.exe_dir).display()
        ),
        Some(_) => {}
    }
}

pub fn status_payload(paths: &AppPaths) -> SelfUpdateStatus {
    SelfUpdateStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        binaries: binaries::inventory(&paths.exe_dir),
    }
}

pub async fn index(State(state): State<AppState>) -> Result<Json<SelfUpdateStatus>, AppError> {
    require_master(&state)?;
    Ok(Json(status_payload(&state.paths)))
}

pub async fn download(
    State(state): State<AppState>,
    Path(arch): Path<String>,
) -> Result<Response, AppError> {
    require_master(&state)?;
    let arch =
        Arch::parse(&arch).ok_or_else(|| AppError::bad("架构必须是 linux-amd64 或 linux-arm64"))?;
    let info = binaries::stored(&state.paths.exe_dir, arch).ok_or_else(|| {
        AppError::not_found(format!(
            "主节点没有 {} 的升级二进制（请放到 {}）",
            arch.label(),
            binaries::binary_path(&state.paths.exe_dir, arch).display()
        ))
    })?;
    let path = PathBuf::from(info.path.unwrap_or_else(|| {
        binaries::binary_path(&state.paths.exe_dir, arch)
            .display()
            .to_string()
    }));
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| AppError::internal(format!("读取 {} 失败：{e}", path.display())))?;
    let disposition = format!("attachment; filename=\"{}\"", arch.asset_name());
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    if let Ok(value) = HeaderValue::from_str(&bytes.len().to_string()) {
        headers.insert(header::CONTENT_LENGTH, value);
    }
    Ok((headers, bytes).into_response())
}

fn require_master(state: &AppState) -> Result<(), AppError> {
    if state.cluster.role != crate::cluster::Role::Master {
        return Err(AppError::bad("只有主节点提供程序升级"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// 已替换二进制并请求重启。
    Done,
    /// 已有一次升级在进行（避免心跳重复下载）。
    InProgress,
}

/// 从 master 下载对应架构二进制，校验后替换本机程序并请求重启。
pub async fn apply(master: &str, token: &str, offer: &UpgradeOffer) -> Result<ApplyOutcome> {
    if APPLYING.swap(true, Ordering::SeqCst) {
        return Ok(ApplyOutcome::InProgress);
    }
    match apply_inner(master, token, offer).await {
        Ok(()) => Ok(ApplyOutcome::Done),
        Err(err) => {
            APPLYING.store(false, Ordering::SeqCst);
            Err(err)
        }
    }
}

async fn apply_inner(master: &str, token: &str, offer: &UpgradeOffer) -> Result<()> {
    let arch = Arch::parse(&offer.arch).context("升级要约中的架构无法识别")?;
    let dest_dir = binaries::current_exe()?
        .parent()
        .map(PathBuf::from)
        .context("可执行文件没有父目录")?;
    let tmp = dest_dir.join(".cangling-update.download");
    let url = format!("{master}/api/cluster/self-update/{}", arch.slug());
    tracing::info!(
        "正在从 master 下载 {}（约 {} 字节）",
        arch.asset_name(),
        offer.size
    );
    let (status, bytes) = http::get_bytes(&url, token)
        .await
        .with_context(|| format!("下载 {url}"))?;
    if !status.is_success() {
        bail!("下载升级包失败 {status}");
    }
    if (bytes.len() as u64) < MIN_BYTES {
        bail!("下载文件过小（{} 字节），可能不是有效二进制", bytes.len());
    }
    if let Some(got) = binaries::elf_arch(&bytes) {
        if got != arch {
            bail!("下载的二进制架构是 {}，期望 {}", got.label(), arch.label());
        }
    } else {
        bail!("下载的文件不是 ELF 可执行文件");
    }

    let bytes = bytes.to_vec();
    let expected = offer.version.clone();
    tokio::task::spawn_blocking(move || finish_replace(&tmp, &bytes, &expected))
        .await
        .context("升级任务异常退出")??;
    Ok(())
}

fn finish_replace(tmp: &std::path::Path, bytes: &[u8], expected_version: &str) -> Result<()> {
    let _ = fs::remove_file(tmp);
    fs::write(tmp, bytes).with_context(|| format!("写入 {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(tmp)?.permissions();
        perm.set_mode(0o755);
        fs::set_permissions(tmp, perm)?;
    }
    verify_version(tmp, expected_version)?;
    let dest = binaries::replace_current_exe(tmp)?;
    let _ = fs::remove_file(tmp);
    tracing::info!(
        "已替换本机程序为 v{}：{}",
        binaries::strip_v(expected_version),
        dest.display()
    );
    service::request_restart().context("请求重启失败")?;
    if !service::is_installed() {
        std::process::exit(0);
    }
    Ok(())
}

fn verify_version(bin: &std::path::Path, expected: &str) -> Result<()> {
    let output = Command::new(bin)
        .arg("version")
        .output()
        .with_context(|| format!("执行 {} version", bin.display()))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        bail!("新二进制无法运行（exit {:?}）：{err}", output.status.code());
    }
    let got = String::from_utf8_lossy(&output.stdout);
    let got = binaries::strip_v(got.trim());
    let exp = binaries::strip_v(expected);
    if got != exp {
        bail!("新二进制版本是 {got}，期望 {exp}");
    }
    Ok(())
}

/// 升级失败后的重试间隔。
pub const RETRY_SECS: u64 = 300;

pub fn retry_delay() -> Duration {
    Duration::from_secs(RETRY_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binaries::Arch;

    #[test]
    fn offer_none_when_same_or_newer() {
        let dir = std::env::temp_dir().join(format!(
            "cangling-offer-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = dummy_paths(&dir);
        let master = env!("CARGO_PKG_VERSION");
        assert!(offer_for(&paths, master, "x86_64").is_none());
        assert!(offer_for(&paths, "99.0.0", "x86_64").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn offer_when_older_and_binary_present() {
        let dir = std::env::temp_dir().join(format!(
            "cangling-offer-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("incoming");
        std::fs::write(&fake, crate::binaries::fake_elf(Arch::Arm64)).unwrap();
        binaries::import_file(&dir, &fake).unwrap();
        let paths = dummy_paths(&dir);
        let offer = offer_for(&paths, "0.0.1", "aarch64").expect("should offer");
        assert_eq!(offer.arch, "linux-arm64");
        assert_eq!(offer.version, env!("CARGO_PKG_VERSION"));
        assert!(offer.size >= MIN_BYTES);
        assert!(offer_for(&paths, "0.0.1", "x86_64").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_offer_reads_upgrade_object() {
        let v = serde_json::json!({
            "ok": true,
            "upgrade": { "version": "0.1.58", "arch": "linux-amd64", "size": 12 }
        });
        let offer = parse_offer(&v).expect("offer");
        assert_eq!(offer.version, "0.1.58");
        assert_eq!(offer.arch, "linux-amd64");
        assert!(parse_offer(&serde_json::json!({ "ok": true })).is_none());
    }

    fn dummy_paths(exe_dir: &std::path::Path) -> AppPaths {
        AppPaths {
            exe_dir: exe_dir.to_path_buf(),
            config_dir: exe_dir.join("config"),
            db_path: exe_dir.join("config/cangling.db"),
            backups_dir: exe_dir.join("config/backups"),
            uploads_dir: exe_dir.join("config/uploads"),
            portal_dir: exe_dir.join("config/portal"),
            logs_dir: exe_dir.join("logs"),
            log_file: exe_dir.join("logs/cangling-update.log"),
        }
    }
}
