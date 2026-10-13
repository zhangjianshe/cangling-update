use crate::error::AppError;
use crate::state::AppState;
use axum::extract::{Multipart, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

const DEFAULT_DEPLOY_DIR: &str = "/opt/cangling/jar-deploy";
const MAX_JAR_BYTES: u64 = 1024 * 1024 * 1024;
const REQUIRED_JARS: [&str; 4] = [
    "cis-server-1.0.0.jar",
    "cis-map-1.0.0.jar",
    "cis-k8s-1.0.0.jar",
    "np4-base-server-1.0.0.jar",
];
static UPGRADING: AtomicBool = AtomicBool::new(false);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jar-upgrade/status", get(status))
        .route("/api/jar-upgrade/upload", post(upload))
        .route("/api/jar-upgrade/run", post(run))
}

fn deploy_dir() -> PathBuf {
    std::env::var_os("CANGLING_JAR_DEPLOY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DEPLOY_DIR))
}

fn checked_name(name: &str) -> Result<&'static str, AppError> {
    REQUIRED_JARS
        .iter()
        .copied()
        .find(|expected| *expected == name)
        .ok_or_else(|| AppError::bad(format!("不允许的 JAR 文件名：{name}")))
}

#[derive(Debug, Serialize)]
struct JarFileStatus {
    name: &'static str,
    present: bool,
    bytes: u64,
    modified: Option<u64>,
}

#[derive(Debug, Serialize)]
struct JarUpgradeStatus {
    incoming_dir: String,
    script: String,
    ready: bool,
    upgrading: bool,
    files: Vec<JarFileStatus>,
}

async fn load_status() -> Result<JarUpgradeStatus, AppError> {
    let root = deploy_dir();
    let incoming = root.join("incoming");
    let script = root.join("upgrade-jars.sh");
    let mut files = Vec::with_capacity(REQUIRED_JARS.len());
    for name in REQUIRED_JARS {
        let metadata = tokio::fs::metadata(incoming.join(name)).await.ok();
        files.push(JarFileStatus {
            name,
            present: metadata
                .as_ref()
                .is_some_and(|item| item.is_file() && item.len() > 0),
            bytes: metadata.as_ref().map_or(0, |item| item.len()),
            modified: metadata
                .and_then(|item| item.modified().ok())
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_secs()),
        });
    }
    let ready = files.iter().any(|file| file.present)
        && tokio::fs::metadata(&script)
            .await
            .is_ok_and(|item| item.is_file());
    Ok(JarUpgradeStatus {
        incoming_dir: incoming.display().to_string(),
        script: script.display().to_string(),
        ready,
        upgrading: UPGRADING.load(Ordering::SeqCst),
        files,
    })
}

async fn status() -> Result<Json<JarUpgradeStatus>, AppError> {
    load_status().await.map(Json)
}

async fn upload(mut multipart: Multipart) -> Result<Json<JarUpgradeStatus>, AppError> {
    if UPGRADING.load(Ordering::SeqCst) {
        return Err(AppError::conflict("JAR 升级正在执行，暂时不能覆盖上传目录"));
    }
    let incoming = deploy_dir().join("incoming");
    tokio::fs::create_dir_all(&incoming).await?;
    let mut received = Vec::new();
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| AppError::bad(format!("无效的上传数据：{error}")))?
    {
        if field.name() != Some("files") {
            continue;
        }
        let raw_name = field
            .file_name()
            .ok_or_else(|| AppError::bad("上传文件缺少文件名"))?;
        let name = checked_name(raw_name)?;
        let temp = incoming.join(format!(".{name}.{}.part", Uuid::new_v4()));
        let target = incoming.join(name);
        let write_result: Result<(), AppError> = async {
            let mut size = 0u64;
            let mut file = tokio::fs::File::create(&temp).await?;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|error| AppError::bad(format!("读取 {name} 失败：{error}")))?
            {
                size = size
                    .checked_add(chunk.len() as u64)
                    .ok_or_else(|| AppError::bad("上传文件大小溢出"))?;
                if size > MAX_JAR_BYTES {
                    return Err(AppError::bad(format!("{name} 不能超过 1 GiB")));
                }
                file.write_all(&chunk).await?;
            }
            if size == 0 {
                return Err(AppError::bad(format!("{name} 是空文件")));
            }
            file.flush().await?;
            file.sync_all().await?;
            tokio::fs::rename(&temp, &target).await?;
            Ok(())
        }
        .await;
        if let Err(error) = write_result {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(error);
        }
        received.push(name);
    }
    if received.is_empty() {
        return Err(AppError::bad("请至少选择一个规定名称的 JAR 文件"));
    }
    // An upload request defines one upgrade batch. Remove supported JARs left
    // over from an earlier batch so they cannot be upgraded accidentally.
    for name in REQUIRED_JARS {
        if !received.contains(&name) {
            match tokio::fs::remove_file(incoming.join(name)).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    load_status().await.map(Json)
}

async fn run(
    State(state): State<AppState>,
) -> Result<Json<crate::progress::JobProgress>, AppError> {
    let current = load_status().await?;
    if !current.ready {
        return Err(AppError::bad(
            "至少需要一个待升级 JAR，并且升级脚本必须存在",
        ));
    }
    if UPGRADING.swap(true, Ordering::SeqCst) {
        return Err(AppError::conflict("已有 JAR 升级任务正在执行"));
    }
    let job = state.jobs.create();
    let job_id = job.id.clone();
    let jobs = state.jobs.clone();
    let script = deploy_dir().join("upgrade-jars.sh");
    tokio::spawn(async move {
        jobs.set(
            &job_id,
            "upgrade",
            "正在同步 JAR 并滚动升级 19、18 节点…",
            0,
            0,
        );
        let result = tokio::process::Command::new(&script).output().await;
        UPGRADING.store(false, Ordering::SeqCst);
        match result {
            Ok(output) if output.status.success() => {
                let message = output_tail(&output.stdout, "JAR 升级完成");
                jobs.finish_ok(&job_id, &message);
            }
            Ok(output) => {
                let fallback = format!("升级脚本退出码：{}", output.status);
                let message = output_tail(&output.stderr, &output_tail(&output.stdout, &fallback));
                jobs.finish_err(&job_id, &message);
            }
            Err(error) => {
                jobs.finish_err(&job_id, &format!("无法执行 {}：{error}", script.display()))
            }
        }
    });
    Ok(Json(job))
}

fn output_tail(bytes: &[u8], fallback: &str) -> String {
    const MAX: usize = 4000;
    let start = bytes.len().saturating_sub(MAX);
    let value = String::from_utf8_lossy(&bytes[start..]).trim().to_string();
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_expected_jar_names_are_allowed() {
        for name in REQUIRED_JARS {
            assert_eq!(checked_name(name).unwrap(), name);
        }
        assert!(checked_name("other.jar").is_err());
        assert!(checked_name("../cis-map-1.0.0.jar").is_err());
    }

    #[test]
    fn output_is_bounded() {
        let bytes = vec![b'x'; 5000];
        assert_eq!(output_tail(&bytes, "empty").len(), 4000);
        assert_eq!(output_tail(b"", "empty"), "empty");
    }
}
