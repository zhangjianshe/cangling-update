use crate::error::AppError;
use crate::hosts::{self, SshSyncTarget};
use crate::progress::JobProgress;
use crate::state::AppState;
use axum::extract::{Multipart, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use std::time::Duration;
use uuid::Uuid;

const MAX_JAR_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DISTRIBUTION_DIR: &str = "/opt/cangling/jars";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JarFile {
    name: String,
    size: u64,
    modified_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JarList {
    directory: String,
    distribution_directory: &'static str,
    files: Vec<JarFile>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JarUploadResult {
    files: Vec<String>,
    sync_job: JobProgress,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JarPreflightRequest {
    files: Vec<JarPreflightFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JarPreflightFile {
    name: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JarPreflightResult {
    files: Vec<JarPreflightStatus>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JarPreflightStatus {
    name: String,
    exists: bool,
    same: bool,
    server_size: u64,
    server_sha256: String,
}

#[derive(Debug, Default, Deserialize)]
struct JarSyncRequest {
    #[serde(default)]
    files: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct JarDeleteRequest {
    files: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JarDeleteResult {
    deleted: Vec<String>,
    missing: Vec<String>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/cangling/apps/jars", get(list))
        .route("/api/cangling/apps/jars/preflight", post(preflight))
        .route("/api/cangling/apps/jars/upload", post(upload))
        .route("/api/cangling/apps/jars/delete", post(delete_files))
        .route("/api/cangling/apps/jars/sync", post(sync_all))
        .route(
            "/api/cangling/apps/jars/online",
            get(online_status).post(online_start),
        )
}

const ONLINE_JARS: [(&str, &str); 3] = [
    ("cis-server-1.0.0.jar", "cis-server-1.0.0.jar"),
    ("cis-map-1.0.0.jar", "cis-map-1.0.0.jar"),
    ("cl-base-server-1.0.0.jar", "np4-base-server-1.0.0.jar"),
];
static ONLINE_RUNNING: AtomicBool = AtomicBool::new(false);
static ONLINE_JOB: Mutex<Option<String>> = Mutex::new(None);
struct OnlineGuard;
impl Drop for OnlineGuard {
    fn drop(&mut self) {
        ONLINE_RUNNING.store(false, Ordering::Release);
    }
}

async fn online_status(State(state): State<AppState>) -> Json<Option<JobProgress>> {
    let id = ONLINE_JOB.lock().expect("online jar job").clone();
    Json(id.and_then(|id| state.jobs.get(&id)))
}

async fn online_start(State(state): State<AppState>) -> Result<Json<JobProgress>, AppError> {
    if ONLINE_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(AppError::conflict("正在在线获取 JAR，请等待当前任务完成"));
    }
    let guard = OnlineGuard;
    // Resolve all configured hosts before starting any network or file changes.
    let targets = hosts::ssh_sync_targets(&state)?;
    let job = state.jobs.create();
    *ONLINE_JOB.lock().expect("online jar job") = Some(job.id.clone());
    let id = job.id.clone();
    tokio::spawn(async move {
        let _guard = guard;
        let staging =
            crate::jar_upgrade::incoming_dir().join(format!(".online-{}", Uuid::new_v4()));
        let result = fetch_and_sync(&state, &id, &staging, targets).await;
        let _ = tokio::fs::remove_dir_all(&staging).await;
        match result {
            Ok(message) => state.jobs.finish_ok(&id, &message),
            Err(error) => state.jobs.finish_err(&id, &error.to_string()),
        }
    });
    Ok(Json(job))
}

fn content_length(headers: &str) -> Option<u64> {
    // Reset at every response (redirects/proxy CONNECT may precede the final one).
    let mut length = None;
    for line in headers.lines() {
        if line.starts_with("HTTP/") {
            length = None;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().ok();
            }
        }
    }
    length
}

async fn download_jar(
    state: &AppState,
    id: &str,
    remote_name: &str,
    path: &Path,
    index: usize,
) -> Result<(), AppError> {
    let headers = path.with_extension("headers");
    let url = format!("https://soft.cangling.cn:22002/software/np4-jars/latest/{remote_name}");
    let mut child = tokio::process::Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "20",
            "--max-time",
            "1800",
            "--max-filesize",
            &MAX_JAR_BYTES.to_string(),
            "--dump-header",
        ])
        .arg(&headers)
        .arg("--output")
        .arg(path)
        .arg(&url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| AppError::bad(format!("无法启动 curl：{e}")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::internal("缺少下载错误输出"))?;
    let errors = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut message = String::new();
        tokio::io::BufReader::new(stderr)
            .read_to_string(&mut message)
            .await
            .map(|_| message)
    });
    let started = std::time::Instant::now();
    loop {
        let received = tokio::fs::metadata(path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let total = tokio::fs::read_to_string(&headers)
            .await
            .ok()
            .and_then(|h| content_length(&h));
        let part = total
            .filter(|t| *t > 0)
            .map(|t| received.saturating_mul(250) / t)
            .unwrap_or(0)
            .min(249);
        let size = match total {
            Some(total) => format!(
                "{:.1} / {:.1} MiB",
                received as f64 / 1048576.0,
                total as f64 / 1048576.0
            ),
            None => format!("已下载 {:.1} MiB", received as f64 / 1048576.0),
        };
        state.jobs.set(
            id,
            "jar-download",
            &format!("下载 {}/3：{remote_name} · {size}", index + 1),
            index as u64 * 250 + part,
            1000,
        );
        if received > MAX_JAR_BYTES {
            let _ = child.kill().await;
            return Err(AppError::bad("下载 JAR 超过 2 GiB 限制"));
        }
        if let Some(exit) = child.try_wait().map_err(|e| AppError::bad(e.to_string()))? {
            let message = errors
                .await
                .map_err(|e| AppError::internal(e.to_string()))??;
            if !exit.success() {
                return Err(AppError::bad(format!(
                    "下载 {remote_name} 失败：{}",
                    message.trim()
                )));
            }
            let received = tokio::fs::metadata(path).await?.len();
            let total = tokio::fs::read_to_string(&headers)
                .await
                .ok()
                .and_then(|h| content_length(&h));
            if received == 0 || received > MAX_JAR_BYTES || total.is_some_and(|t| t != received) {
                return Err(AppError::bad(format!("{remote_name} 下载大小校验失败")));
            }
            break;
        }
        if started.elapsed() > Duration::from_secs(1830) {
            let _ = child.kill().await;
            return Err(AppError::bad(format!("下载 {remote_name} 超时")));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    state.jobs.set(
        id,
        "jar-verify",
        &format!("校验 {remote_name} 的 JAR 内容…"),
        (index as u64 + 1) * 250,
        1000,
    );
    validate_downloaded_jar(path).await
}

async fn validate_downloaded_jar(path: &Path) -> Result<(), AppError> {
    let output = tokio::time::timeout(Duration::from_secs(180), tokio::process::Command::new("python3")
        .args(["-c", "import sys,zipfile; z=zipfile.ZipFile(sys.argv[1]); assert 'META-INF/MANIFEST.MF' in z.namelist(), 'missing JAR manifest'; assert any(n.endswith('.class') for n in z.namelist()), 'missing Java classes'; assert sum(i.file_size for i in z.infolist()) <= 8589934592, 'unpacked JAR too large'; assert z.testzip() is None, 'JAR CRC mismatch'"])
        .arg(path).kill_on_drop(true).output()).await
        .map_err(|_| AppError::bad("JAR 内容校验超时"))?
        .map_err(|e| AppError::bad(format!("无法校验 JAR：{e}")))?;
    if !output.status.success() {
        return Err(AppError::bad(format!(
            "下载内容不是完整的 Java JAR：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

async fn fetch_and_sync(
    state: &AppState,
    id: &str,
    staging: &Path,
    targets: Vec<SshSyncTarget>,
) -> Result<String, AppError> {
    tokio::fs::create_dir_all(staging).await?;
    for (index, (remote, local)) in ONLINE_JARS.iter().enumerate() {
        download_jar(state, id, remote, &staging.join(local), index).await?;
    }
    let files: Vec<String> = ONLINE_JARS
        .iter()
        .map(|(_, local)| local.to_string())
        .collect();
    let staging = staging.to_path_buf();
    let jobs = state.jobs.clone();
    let id = id.to_string();
    tokio::task::spawn_blocking(move || {
        for name in &files {
            atomic_copy(
                &staging.join(name),
                &crate::jar_upgrade::incoming_dir().join(name),
            )?;
        }
        let total = (targets.len() * files.len()).max(1) as u64;
        let summary = sync_files(&targets, &files, |message, current| {
            jobs.set(&id, "jar-sync", message, 750 + current * 250 / total, 1000);
        })?;
        Ok(format!(
            "在线获取 3 个 JAR 完成；{summary}。重启工作负载后加载新 JAR。"
        ))
    })
    .await
    .map_err(|e| AppError::internal(e.to_string()))?
}

async fn delete_files(
    Json(body): Json<JarDeleteRequest>,
) -> Result<Json<JarDeleteResult>, AppError> {
    if body.files.is_empty() || body.files.len() > 100 {
        return Err(AppError::bad("一次应删除 1 到 100 个 JAR 包"));
    }
    let incoming = crate::jar_upgrade::incoming_dir();
    tokio::fs::create_dir_all(&incoming).await?;
    let mut seen = std::collections::HashSet::new();
    let mut deleted = Vec::new();
    let mut missing = Vec::new();
    for raw_name in body.files {
        let name = checked_jar_name(&raw_name)?;
        if !seen.insert(name.clone()) {
            continue;
        }
        match tokio::fs::remove_file(incoming.join(&name)).await {
            Ok(()) => deleted.push(name),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing.push(name),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(Json(JarDeleteResult { deleted, missing }))
}

async fn preflight(
    Json(body): Json<JarPreflightRequest>,
) -> Result<Json<JarPreflightResult>, AppError> {
    if body.files.is_empty() || body.files.len() > 100 {
        return Err(AppError::bad("一次应检查 1 到 100 个 JAR 包"));
    }
    let incoming = crate::jar_upgrade::incoming_dir();
    let result = tokio::task::spawn_blocking(move || {
        let mut statuses = Vec::with_capacity(body.files.len());
        for file in body.files {
            let name = checked_jar_name(&file.name)?;
            let client_sha = file.sha256.trim().to_ascii_lowercase();
            if client_sha.len() != 64 || !client_sha.bytes().all(|value| value.is_ascii_hexdigit())
            {
                return Err(AppError::bad(format!("{name} 的 SHA-256 无效")));
            }
            let path = incoming.join(&name);
            let metadata = std::fs::metadata(&path).ok().filter(|item| item.is_file());
            let exists = metadata.is_some();
            let server_size = metadata.as_ref().map_or(0, std::fs::Metadata::len);
            let server_sha256 = if exists && server_size == file.size {
                sha256_file(&path)?
            } else {
                String::new()
            };
            statuses.push(JarPreflightStatus {
                name,
                exists,
                same: exists && server_size == file.size && server_sha256 == client_sha,
                server_size,
                server_sha256,
            });
        }
        Ok::<_, AppError>(JarPreflightResult { files: statuses })
    })
    .await
    .map_err(|error| AppError::internal(format!("JAR 预检任务失败：{error}")))??;
    Ok(Json(result))
}

async fn list() -> Result<Json<JarList>, AppError> {
    let jar_dir = crate::jar_upgrade::incoming_dir();
    tokio::fs::create_dir_all(&jar_dir).await?;
    let mut entries = tokio::fs::read_dir(&jar_dir).await?;
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let file_type = entry.file_type().await?;
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_jar_name(&name) {
            continue;
        }
        let metadata = entry.metadata().await?;
        let modified_at = metadata
            .modified()
            .ok()
            .map(DateTime::<Utc>::from)
            .map(|value| value.to_rfc3339())
            .unwrap_or_default();
        files.push(JarFile {
            name,
            size: metadata.len(),
            modified_at,
        });
    }
    files.sort_by(|left, right| {
        right
            .modified_at
            .cmp(&left.modified_at)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(Json(JarList {
        directory: jar_dir.display().to_string(),
        distribution_directory: DISTRIBUTION_DIR,
        files,
    }))
}

async fn upload(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<JarUploadResult>, AppError> {
    let jar_dir = crate::jar_upgrade::incoming_dir();
    tokio::fs::create_dir_all(&jar_dir).await?;
    let mut received = Vec::new();
    let mut sync_names = Vec::new();
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| AppError::bad(format!("无效的上传数据：{error}")))?
    {
        if field.name() == Some("sync_names") {
            let text = field
                .text()
                .await
                .map_err(|error| AppError::bad(format!("读取同步文件列表失败：{error}")))?;
            sync_names = serde_json::from_str::<Vec<String>>(&text)
                .map_err(|error| AppError::bad(format!("同步文件列表无效：{error}")))?;
            continue;
        }
        if field.name() != Some("files") {
            continue;
        }
        let name = checked_jar_name(
            field
                .file_name()
                .ok_or_else(|| AppError::bad("上传文件缺少文件名"))?,
        )?;
        let temp = jar_dir.join(format!(".{name}.{}.part", Uuid::new_v4()));
        let target = jar_dir.join(&name);
        let write_result: Result<(), AppError> = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .await?;
            let mut size = 0u64;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|error| AppError::bad(format!("读取 {name} 失败：{error}")))?
            {
                size = size
                    .checked_add(chunk.len() as u64)
                    .ok_or_else(|| AppError::bad("上传文件大小溢出"))?;
                if size > MAX_JAR_BYTES {
                    return Err(AppError::bad(format!("{name} 不能超过 2 GiB")));
                }
                tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
            }
            if size == 0 {
                return Err(AppError::bad(format!("{name} 是空文件")));
            }
            tokio::io::AsyncWriteExt::flush(&mut file).await?;
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
        return Err(AppError::bad("请至少上传一个 .jar 文件"));
    }
    if sync_names.is_empty() {
        sync_names = received.clone();
    }
    let mut unique = std::collections::HashSet::new();
    sync_names.retain(|name| unique.insert(name.clone()));
    for name in &mut sync_names {
        *name = checked_jar_name(name)?;
        if !jar_dir.join(&*name).is_file() {
            return Err(AppError::bad(format!("待同步 JAR 不存在：{name}")));
        }
    }
    let job = start_sync(state, sync_names)?;
    Ok(Json(JarUploadResult {
        files: received,
        sync_job: job,
    }))
}

async fn sync_all(
    State(state): State<AppState>,
    body: Option<Json<JarSyncRequest>>,
) -> Result<Json<JobProgress>, AppError> {
    let files = if let Some(Json(request)) = body {
        let mut files = Vec::new();
        for name in request.files {
            let name = checked_jar_name(&name)?;
            if !crate::jar_upgrade::incoming_dir().join(&name).is_file() {
                return Err(AppError::bad(format!("待同步 JAR 不存在：{name}")));
            }
            if !files.contains(&name) {
                files.push(name);
            }
        }
        files
    } else {
        jar_paths()?
            .into_iter()
            .filter_map(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .collect::<Vec<_>>()
    };
    if files.is_empty() {
        return Err(AppError::bad(format!(
            "{} 中没有 JAR 包",
            crate::jar_upgrade::incoming_dir().display()
        )));
    }
    start_sync(state, files).map(Json)
}

fn start_sync(state: AppState, files: Vec<String>) -> Result<JobProgress, AppError> {
    let targets = hosts::ssh_sync_targets(&state)?;
    let job = state.jobs.create();
    let job_id = job.id.clone();
    let jobs = state.jobs.clone();
    tokio::task::spawn_blocking(move || {
        let total = (targets.len() * files.len()).max(1) as u64;
        let result = sync_files(&targets, &files, |message, current| {
            jobs.set(&job_id, "jar-sync", message, current, total);
        });
        match result {
            Ok(summary) => jobs.finish_ok(&job_id, &summary),
            Err(error) => jobs.finish_err(&job_id, &error.to_string()),
        }
    });
    Ok(job)
}

fn sync_files<F>(
    targets: &[SshSyncTarget],
    files: &[String],
    mut progress: F,
) -> Result<String, AppError>
where
    F: FnMut(&str, u64),
{
    if targets.is_empty() {
        return Ok("JAR 已保存到本机；当前没有需要同步的其他主机".into());
    }
    let mut completed = 0u64;
    let mut copied = 0usize;
    let mut skipped = 0usize;
    let incoming_dir = crate::jar_upgrade::incoming_dir();
    for target in targets {
        if target.local {
            std::fs::create_dir_all(DISTRIBUTION_DIR)?;
            for name in files {
                let name = checked_jar_name(name)?;
                let source = incoming_dir.join(&name);
                if !source.is_file() {
                    return Err(AppError::bad(format!(
                        "上传目录中的 JAR 不存在：{}",
                        source.display()
                    )));
                }
                let destination = Path::new(DISTRIBUTION_DIR).join(&name);
                progress(&format!("正在原子分发 {name} → master"), completed);
                if files_equal(&source, &destination)? {
                    skipped += 1;
                } else {
                    atomic_copy(&source, &destination)?;
                    copied += 1;
                }
                completed += 1;
                progress(&format!("master 已原子写入并校验 {name}"), completed);
            }
            continue;
        }
        run_ssh(
            target,
            &format!("install -d -m 0755 {}", shell_quote(DISTRIBUTION_DIR)),
        )?;
        for name in files {
            let name = checked_jar_name(name)?;
            let local = incoming_dir.join(&name);
            if !local.is_file() {
                return Err(AppError::bad(format!(
                    "本地 JAR 不存在：{}",
                    local.display()
                )));
            }
            progress(&format!("正在同步 {name} → {}", target.name), completed);
            let digest = sha256_file(&local)?;
            let remote = format!("{DISTRIBUTION_DIR}/{name}");
            let remote_digest = run_ssh(
                target,
                &format!(
                    "test -f {path} && sha256sum {path} | awk '{{print $1}}' || true",
                    path = shell_quote(&remote)
                ),
            )?;
            if remote_digest.trim() == digest {
                skipped += 1;
            } else {
                let temp = format!("{DISTRIBUTION_DIR}/.{name}.{}.part", Uuid::new_v4());
                run_scp(target, &local, &temp)?;
                run_ssh(
                    target,
                    &format!(
                        "chmod 0644 {temp} && mv -f -- {temp} {target}",
                        temp = shell_quote(&temp),
                        target = shell_quote(&remote)
                    ),
                )?;
                copied += 1;
            }
            completed += 1;
            progress(&format!("已处理 {name} → {}", target.name), completed);
        }
    }
    Ok(format!(
        "同步完成：{} 台主机（含 master），复制 {copied} 个，校验或跳过 {skipped} 个。运行中的容器继续使用原只读挂载；重启或重新部署后加载新 JAR。",
        targets.len()
    ))
}

fn jar_paths() -> Result<Vec<PathBuf>, AppError> {
    let jar_dir = crate::jar_upgrade::incoming_dir();
    std::fs::create_dir_all(&jar_dir)?;
    let mut files = std::fs::read_dir(&jar_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_jar_name)
        })
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn is_jar_name(name: &str) -> bool {
    name.len() <= 255
        && name.to_ascii_lowercase().ends_with(".jar")
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
        && !name.starts_with('.')
        && !name.contains("..")
}

fn checked_jar_name(name: &str) -> Result<String, AppError> {
    let name = name.trim();
    if !is_jar_name(name) {
        return Err(AppError::bad(format!("无效的 JAR 文件名：{name}")));
    }
    Ok(name.to_string())
}

fn sha256_file(path: &Path) -> Result<String, AppError> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn files_equal(source: &Path, destination: &Path) -> Result<bool, AppError> {
    if !destination.is_file() {
        return Ok(false);
    }
    let source_meta = std::fs::metadata(source)?;
    let destination_meta = std::fs::metadata(destination)?;
    if source_meta.len() != destination_meta.len() {
        return Ok(false);
    }
    Ok(sha256_file(source)? == sha256_file(destination)?)
}

fn atomic_copy(source: &Path, destination: &Path) -> Result<(), AppError> {
    let parent = destination
        .parent()
        .ok_or_else(|| AppError::internal("JAR 分发目录无效"))?;
    std::fs::create_dir_all(parent)?;
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| AppError::bad("JAR 文件名无效"))?;
    let temp = parent.join(format!(".{name}.{}.part", Uuid::new_v4()));
    let result: Result<(), AppError> = (|| {
        let mut input = std::fs::File::open(source)?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        std::io::copy(&mut input, &mut output)?;
        output.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        output.sync_all()?;
        std::fs::rename(&temp, destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn run_ssh(target: &SshSyncTarget, script: &str) -> Result<String, AppError> {
    let output = Command::new("ssh")
        .args([
            "-p",
            &target.port.to_string(),
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "StrictHostKeyChecking=accept-new",
            &format!("{}@{}", target.username, target.host),
            "sh",
            "-c",
            &shell_quote(script),
        ])
        .output()
        .map_err(|error| AppError::internal(format!("无法连接主机 {}：{error}", target.name)))?;
    if !output.status.success() {
        return Err(AppError::internal(format!(
            "主机 {} 同步失败：{}",
            target.name,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_scp(target: &SshSyncTarget, local: &Path, remote: &str) -> Result<(), AppError> {
    let destination = format!("{}@{}:{}", target.username, target.host, remote);
    let output = Command::new("scp")
        .args([
            "-P",
            &target.port.to_string(),
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "StrictHostKeyChecking=accept-new",
        ])
        .arg(local)
        .arg(destination)
        .output()
        .map_err(|error| AppError::internal(format!("无法向 {} 传输文件：{error}", target.name)))?;
    if !output.status.success() {
        return Err(AppError::internal(format!(
            "向 {} 传输 {} 失败：{}",
            target.name,
            local.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jar_names_are_restricted_to_safe_leaf_names() {
        assert!(is_jar_name("cis-server-1.0.0.jar"));
        assert!(is_jar_name("应用.jar") == false);
        assert!(!is_jar_name("../app.jar"));
        assert!(!is_jar_name("app.jar.part"));
        assert!(!is_jar_name(".app.jar"));
    }

    #[test]
    fn remote_replace_is_shell_quoted() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}

#[cfg(test)]
mod online_tests {
    use super::*;

    #[test]
    fn download_size_uses_final_response_only() {
        assert_eq!(content_length("HTTP/1.1 302 Found\r\nContent-Length: 25\r\n\r\nHTTP/2 200\r\ncontent-length: 4096\r\n"), Some(4096));
        assert_eq!(
            content_length(
                "HTTP/1.1 302 Found\nContent-Length: 25\nHTTP/2 200\nTransfer-Encoding: chunked\n"
            ),
            None
        );
        assert_eq!(
            content_length("HTTP/2 200\nContent-Length: invalid\n"),
            None
        );
    }

    #[tokio::test]
    async fn jar_validation_rejects_error_pages_and_corrupted_archives() {
        struct TempDir(PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = TempDir(std::env::temp_dir().join(format!("online-jar-test-{}", Uuid::new_v4())));
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = dir.0.join("download.jar");
        let result = Command::new("python3").args(["-c", "import sys,zipfile; z=zipfile.ZipFile(sys.argv[1],'w'); z.writestr('META-INF/MANIFEST.MF','Manifest-Version: 1.0'); z.writestr('Test.class',b'\\xca\\xfe\\xba\\xbe'); z.close()"])
            .arg(&path).status().unwrap();
        assert!(result.success());
        validate_downloaded_jar(&path).await.unwrap();
        let valid = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"<html>download error</html>").unwrap();
        assert!(validate_downloaded_jar(&path).await.is_err());
        std::fs::write(&path, &valid[..valid.len() / 2]).unwrap();
        assert!(validate_downloaded_jar(&path).await.is_err());
        let mut damaged = valid;
        let offset = damaged
            .windows(4)
            .position(|b| b == [0xca, 0xfe, 0xba, 0xbe])
            .unwrap();
        damaged[offset] ^= 0xff;
        std::fs::write(&path, damaged).unwrap();
        assert!(validate_downloaded_jar(&path).await.is_err());
    }
}
