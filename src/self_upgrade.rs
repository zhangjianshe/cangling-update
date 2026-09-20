use crate::binaries::{self, Arch, MIN_BYTES};
use crate::error::AppError;
use crate::service;
use crate::state::AppState;
use axum::extract::{Multipart, State};
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

const MAX_UPLOAD_BYTES: u64 = 512 * 1024 * 1024;
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
static UPGRADING: AtomicBool = AtomicBool::new(false);

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/self-update", post(upload))
}

#[derive(Debug, Serialize)]
struct SelfUpdateResult {
    current_version: &'static str,
    uploaded_version: String,
    arch: String,
    restarting: bool,
}

struct UpgradeGuard;

impl UpgradeGuard {
    fn acquire() -> Result<Self, AppError> {
        if UPGRADING.swap(true, Ordering::SeqCst) {
            return Err(AppError::conflict("已有自更新任务正在执行"));
        }
        Ok(Self)
    }
}

impl Drop for UpgradeGuard {
    fn drop(&mut self) {
        UPGRADING.store(false, Ordering::SeqCst);
    }
}

async fn upload(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<SelfUpdateResult>, AppError> {
    let _guard = UpgradeGuard::acquire()?;
    let tmp_dir = state
        .paths
        .uploads_dir
        .join(format!("self-update-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let tmp_file = tmp_dir.join("cangling-update");

    let result = receive_and_apply(&state, &mut multipart, &tmp_file).await;
    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
    result.map(Json)
}

async fn receive_and_apply(
    state: &AppState,
    multipart: &mut Multipart,
    tmp_file: &Path,
) -> Result<SelfUpdateResult, AppError> {
    let mut received = 0u64;
    let mut found = false;

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::bad(format!("无效的上传数据：{e}")))?
    {
        if field.name() != Some("file") {
            continue;
        }
        if found {
            return Err(AppError::bad("只能上传一个 cangling-update 程序"));
        }
        found = true;
        let mut file = tokio::fs::File::create(tmp_file).await?;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| AppError::bad(format!("读取上传文件失败：{e}")))?
        {
            received = received
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| AppError::bad("上传文件大小溢出"))?;
            if received > MAX_UPLOAD_BYTES {
                return Err(AppError::bad("上传文件不能超过 512 MiB"));
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
    }

    if !found {
        return Err(AppError::bad("请选择 cangling-update 程序"));
    }
    if received < MIN_BYTES {
        return Err(AppError::bad(format!(
            "上传文件过小（{received} 字节），不是有效的发布程序"
        )));
    }

    let host_arch = Arch::host()
        .ok_or_else(|| AppError::bad(format!("不支持当前架构 {}", std::env::consts::ARCH)))?;
    let uploaded_arch = binaries::elf_arch_file(tmp_file)
        .ok_or_else(|| AppError::bad("上传文件不是有效的 x86_64/ARM64 ELF 程序"))?;
    if uploaded_arch != host_arch {
        return Err(AppError::bad(format!(
            "程序架构不匹配：上传的是 {}，当前主机是 {}",
            uploaded_arch.label(),
            host_arch.label()
        )));
    }

    set_executable(tmp_file).await?;
    let uploaded_version = inspect_version(tmp_file).await?;
    let current_version = env!("CARGO_PKG_VERSION");
    if !binaries::is_newer(&uploaded_version, current_version) {
        return Err(AppError::bad(format!(
            "只允许升级到更高版本：当前 v{}，上传 v{}",
            current_version, uploaded_version
        )));
    }

    let src = tmp_file.to_path_buf();
    let exe_dir = state.paths.exe_dir.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<PathBuf> {
        binaries::install_into(&exe_dir, host_arch, &src)?;
        binaries::replace_current_exe(&src)
    })
    .await
    .map_err(|e| AppError::internal(format!("自更新任务异常退出：{e}")))??;

    schedule_restart();
    Ok(SelfUpdateResult {
        current_version,
        uploaded_version,
        arch: host_arch.slug().to_string(),
        restarting: true,
    })
}

async fn set_executable(path: &Path) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = tokio::fs::metadata(path).await?.permissions();
        permissions.set_mode(0o755);
        tokio::fs::set_permissions(path, permissions).await?;
    }
    Ok(())
}

async fn inspect_version(path: &Path) -> Result<String, AppError> {
    let output = tokio::time::timeout(
        VERSION_TIMEOUT,
        tokio::process::Command::new(path).arg("version").output(),
    )
    .await
    .map_err(|_| AppError::bad("检查上传程序版本超时"))?
    .map_err(|e| AppError::bad(format!("无法执行上传程序：{e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::bad(format!(
            "上传程序无法正常执行 version（退出码 {:?}）：{}",
            output.status.code(),
            stderr.trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_version_output(&stdout).ok_or_else(|| AppError::bad("上传程序没有返回有效版本号"))
}

fn parse_version_output(value: &str) -> Option<String> {
    let value = binaries::strip_v(value.trim());
    let mut parts = value.split('.');
    let (Some(a), Some(b), Some(c), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if [a, b, c]
        .into_iter()
        .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
    {
        Some(value.to_string())
    } else {
        None
    }
}

fn schedule_restart() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let installed = service::is_installed();
        if let Err(err) = service::request_restart() {
            tracing::error!("自更新完成，但请求重启失败：{err:#}");
            return;
        }
        if !installed {
            std::process::exit(0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cli_version_output() {
        assert_eq!(
            parse_version_output("v0.1.144\n").as_deref(),
            Some("0.1.144")
        );
        assert_eq!(parse_version_output("1.2.3").as_deref(), Some("1.2.3"));
        assert!(parse_version_output("cangling-update 1.2.3").is_none());
        assert!(parse_version_output("1.2").is_none());
        assert!(parse_version_output("1.2.3.4").is_none());
    }
}
