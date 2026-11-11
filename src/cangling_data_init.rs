//! Offline baseline map data import from the adjacent cangling-repo.

use crate::error::AppError;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use flate2::read::GzDecoder;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use tar::Archive;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

const PACKAGE: &str = "apps/initial-data/ai-china";
const BASE_JAR: &str = "/opt/cangling/jars/np4-base-server-1.0.0.jar";
const BASE_SCRIPT: &str = include_str!("assets/base_init.py");
const REGION_SCRIPT: &str = include_str!("assets/region_init.py");
const REGION_PACKAGE: &str = "apps/initial-data/sys_region_full.zip";

#[derive(Default)]
pub struct DataInitState {
    job_id: Option<String>,
    output: Vec<String>,
    base_job_id: Option<String>,
    base_output: Vec<String>,
    region_job_id: Option<String>,
    region_output: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DataManifest {
    format: String,
    total_bytes: u64,
    files: Vec<DataFile>,
}

#[derive(Deserialize)]
struct DataFile {
    name: String,
    bytes: u64,
    sha256: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/cangling/apps/data-init", get(status).post(start))
        .route(
            "/api/cangling/apps/base-init",
            get(base_status).post(base_start),
        )
        .route(
            "/api/cangling/apps/region-init",
            get(region_status).post(region_start),
        )
}

async fn inspect_base_jar() -> Value {
    let output = Command::new("python3")
        .args(["-c", BASE_SCRIPT, "check", BASE_JAR])
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| json!({"available":false,"reason":"无法解析 JAR 检查结果"})),
        Ok(output) => {
            json!({"available":false,"reason":String::from_utf8_lossy(&output.stderr).trim()})
        }
        Err(error) => json!({"available":false,"reason":format!("无法检查 JAR：{error}")}),
    }
}

async fn base_status(State(state): State<AppState>) -> Json<Value> {
    let availability = inspect_base_jar().await;
    let snapshot = state.data_init.lock().expect("data init state");
    let job = snapshot
        .base_job_id
        .as_deref()
        .and_then(|id| state.jobs.get(id));
    Json(json!({
        "available": availability["available"],
        "reason": availability["reason"],
        "jarPath": BASE_JAR,
        "job": job,
        "output": snapshot.base_output,
    }))
}

async fn base_start(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let availability = inspect_base_jar().await;
    if availability["available"] != true {
        return Err(AppError::bad(
            availability["reason"]
                .as_str()
                .unwrap_or("JAR 中缺少初始化脚本"),
        ));
    }
    let mut current = state.data_init.lock().expect("data init state");
    if let Some(job) = current
        .base_job_id
        .as_deref()
        .and_then(|id| state.jobs.get(id))
    {
        if !job.done {
            return Err(AppError::conflict("base 数据库正在初始化，请查看当前进度"));
        }
    }
    let job = state.jobs.create();
    current.base_job_id = Some(job.id.clone());
    current.base_output.clear();
    drop(current);
    let job_id = job.id.clone();
    tokio::spawn(async move {
        if let Err(error) = run_base_init(&state, &job_id).await {
            let message = error.to_string();
            add_base_output(&state, format!("初始化失败：{message}"));
            state.jobs.finish_err(&job_id, &message);
        }
    });
    Ok(Json(json!({"jobId":job.id})))
}

fn add_base_output(state: &AppState, line: String) {
    let mut current = state.data_init.lock().expect("data init state");
    current.base_output.push(line);
    if current.base_output.len() > 100 {
        current.base_output.remove(0);
    }
}

async fn run_base_init(state: &AppState, job_id: &str) -> Result<(), AppError> {
    state
        .jobs
        .set(job_id, "checking", "检查 JAR 和 cis-postgis", 0, 100);
    let mut child = Command::new("python3")
        .args(["-c", BASE_SCRIPT, "run", BASE_JAR])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| AppError::internal(format!("启动 base 初始化脚本失败：{error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::internal("缺少初始化脚本输出"))?;
    let mut lines = BufReader::new(stdout).lines();
    let mut summary = String::new();
    while let Some(line) = lines.next_line().await? {
        if let Some(rest) = line.strip_prefix("PROGRESS\t") {
            if let Some((percent, message)) = rest.split_once('\t') {
                if let Ok(percent) = percent.parse::<u64>() {
                    state.jobs.set(job_id, "importing", message, percent, 100);
                    add_base_output(state, message.to_string());
                }
            }
        } else if let Some(message) = line.strip_prefix("RESULT\t") {
            summary = message.to_string();
            add_base_output(state, summary.clone());
        } else {
            add_base_output(state, line);
        }
    }
    let mut stderr = String::new();
    if let Some(stream) = child.stderr.take() {
        BufReader::new(stream).read_to_string(&mut stderr).await?;
    }
    let exit = child.wait().await?;
    if !exit.success() {
        return Err(AppError::bad(stderr.trim().to_string()));
    }
    if summary.is_empty() {
        return Err(AppError::internal("初始化脚本没有返回结果说明"));
    }
    state.jobs.finish_ok(job_id, &summary);
    Ok(())
}

async fn inspect_region_package(state: &AppState) -> Value {
    let archive = crate::repo::repo_root(&state.paths).join(REGION_PACKAGE);
    let output = Command::new("python3")
        .args(["-c", REGION_SCRIPT, "check"])
        .arg(archive)
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| json!({"available":false,"reason":"无法解析 行政区划检查结果"})),
        Ok(output) => {
            json!({"available":false,"reason":String::from_utf8_lossy(&output.stderr).trim()})
        }
        Err(error) => json!({"available":false,"reason":format!("无法检查行政区划数据包：{error}")}),
    }
}

async fn region_status(State(state): State<AppState>) -> Json<Value> {
    let availability = inspect_region_package(&state).await;
    let snapshot = state.data_init.lock().expect("data init state");
    let job = snapshot
        .region_job_id
        .as_deref()
        .and_then(|id| state.jobs.get(id));
    Json(json!({
        "available": availability["available"],
        "reason": availability["reason"],
        "packagePath": crate::repo::repo_root(&state.paths).join(REGION_PACKAGE),
        "initialized": availability["initialized"],
        "pending": availability["pending"],
        "skipped": availability["skipped"],
        "job": job,
        "output": snapshot.region_output,
    }))
}

async fn region_start(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let availability = inspect_region_package(&state).await;
    if availability["available"] != true {
        return Err(AppError::bad(
            availability["reason"]
                .as_str()
                .unwrap_or("行政区划数据包不可用"),
        ));
    }
    let mut current = state.data_init.lock().expect("data init state");
    if let Some(job) = current
        .region_job_id
        .as_deref()
        .and_then(|id| state.jobs.get(id))
    {
        if !job.done {
            return Err(AppError::conflict("行政区划数据库正在初始化，请查看当前进度"));
        }
    }
    let job = state.jobs.create();
    current.region_job_id = Some(job.id.clone());
    current.region_output.clear();
    drop(current);
    let job_id = job.id.clone();
    tokio::spawn(async move {
        if let Err(error) = run_region_init(&state, &job_id).await {
            let message = error.to_string();
            add_region_output(&state, format!("初始化失败：{message}"));
            state.jobs.finish_err(&job_id, &message);
        }
    });
    Ok(Json(json!({"jobId":job.id})))
}

fn add_region_output(state: &AppState, line: String) {
    let mut current = state.data_init.lock().expect("data init state");
    current.region_output.push(line);
    if current.region_output.len() > 100 {
        current.region_output.remove(0);
    }
}

async fn run_region_init(state: &AppState, job_id: &str) -> Result<(), AppError> {
    state
        .jobs
        .set(job_id, "checking", "检查行政区划数据包和 cis-postgis", 0, 100);
    let mut child = Command::new("python3")
        .args(["-c", REGION_SCRIPT, "run"])
        .arg(crate::repo::repo_root(&state.paths).join(REGION_PACKAGE))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| AppError::internal(format!("启动行政区划初始化脚本失败：{error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::internal("缺少初始化脚本输出"))?;
    let stderr = child.stderr.take().ok_or_else(|| AppError::internal("缺少初始化错误输出"))?;
    let stderr_task = tokio::spawn(async move {
        let mut text = String::new();
        BufReader::new(stderr).read_to_string(&mut text).await.map(|_| text)
    });
    let mut lines = BufReader::new(stdout).lines();
    let mut summary = String::new();
    while let Some(line) = lines.next_line().await? {
        if let Some(rest) = line.strip_prefix("PROGRESS\t") {
            if let Some((percent, message)) = rest.split_once('\t') {
                if let Ok(percent) = percent.parse::<u64>() {
                    state.jobs.set(job_id, "importing", message, percent, 100);
                    add_region_output(state, message.to_string());
                }
            }
        } else if let Some(message) = line.strip_prefix("RESULT\t") {
            summary = message.to_string();
            add_region_output(state, summary.clone());
        } else {
            add_region_output(state, line);
        }
    }
    let stderr = stderr_task.await.map_err(|error| AppError::internal(error.to_string()))??;
    let exit = child.wait().await?;
    if !exit.success() {
        return Err(AppError::bad(stderr.trim().to_string()));
    }
    if summary.is_empty() {
        return Err(AppError::internal("初始化脚本没有返回结果说明"));
    }
    state.jobs.finish_ok(job_id, &summary);
    Ok(())
}

fn package_dir(state: &AppState) -> PathBuf {
    crate::repo::repo_root(&state.paths).join(PACKAGE)
}

async fn status(State(state): State<AppState>) -> Json<Value> {
    let package = package_dir(&state);
    let snapshot = state.data_init.lock().expect("data init state");
    let job = snapshot.job_id.as_deref().and_then(|id| state.jobs.get(id));
    Json(json!({
        "available": package.join("shape-data.tar.gz").is_file()
            && package.join("data-manifest.json").is_file()
            && package.join("bundle.json").is_file()
            && package.join("import.py").is_file(),
        "packagePath": package.display().to_string(),
        "job": job,
        "output": snapshot.output,
    }))
}

async fn start(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let package = package_dir(&state);
    for filename in [
        "shape-data.tar.gz",
        "data-manifest.json",
        "bundle.json",
        "import.py",
    ] {
        if !package.join(filename).is_file() {
            return Err(AppError::bad(format!(
                "基础数据包缺少 {filename}：{}",
                package.display()
            )));
        }
    }
    let mut current = state.data_init.lock().expect("data init state");
    if let Some(job) = current.job_id.as_deref().and_then(|id| state.jobs.get(id)) {
        if !job.done {
            return Err(AppError::conflict("基础数据正在导入，请查看当前进度"));
        }
    }
    let job = state.jobs.create();
    current.job_id = Some(job.id.clone());
    current.output.clear();
    drop(current);
    let job_id = job.id.clone();
    tokio::spawn(async move {
        if let Err(error) = run_import(&state, &job_id, &package).await {
            let message = error.to_string();
            add_output(&state, format!("导入失败：{message}"));
            state.jobs.finish_err(&job_id, &message);
        }
    });
    Ok(Json(json!({ "jobId": job.id })))
}

fn add_output(state: &AppState, line: String) {
    let mut current = state.data_init.lock().expect("data init state");
    current.output.push(line);
    if current.output.len() > 100 {
        current.output.remove(0);
    }
}

async fn run_import(state: &AppState, job_id: &str, package: &Path) -> Result<(), AppError> {
    let config = crate::cangling_apps::load_saved_config(state)?;
    let namespace = &config.namespace;
    state
        .jobs
        .set(job_id, "checking", "检查 cangling-map 与基础数据包", 0, 100);
    let deployments = crate::cangling_apps::kubectl_resource_json(namespace, "deployments").await?;
    let deployment = deployments["items"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item["metadata"]["name"] == "cangling-map")
        })
        .ok_or_else(|| AppError::bad("集群中未找到 cangling-map Deployment"))?;
    let host_path = deployment["spec"]["template"]["spec"]["volumes"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["name"] == "geodata"))
        .and_then(|item| item["hostPath"]["path"].as_str())
        .ok_or_else(|| AppError::bad("cangling-map 未配置 geodata HostPath"))?;
    let host_path = PathBuf::from(host_path);
    if !host_path.is_absolute() || !host_path.ends_with("cangling-map/geodata") {
        return Err(AppError::bad(
            "cangling-map 的 geodata 宿主机目录不符合预期",
        ));
    }
    let pods = crate::cangling_apps::kubectl_resource_json(namespace, "pods").await?;
    let node = pods["items"]
        .as_array()
        .and_then(|items| {
            items.iter().find(|item| {
                item["metadata"]["labels"]["app"] == "cangling-map"
                    && item["status"]["phase"] == "Running"
            })
        })
        .and_then(|item| item["spec"]["nodeName"].as_str())
        .ok_or_else(|| AppError::bad("cangling-map 没有运行中的 Pod"))?;
    let local_node = fs::read_to_string("/etc/hostname")?.trim().to_string();
    if node != local_node {
        return Err(AppError::bad(format!(
            "cangling-map 运行在 {node}；请在该节点执行数据初始化"
        )));
    }
    let services = crate::cangling_apps::kubectl_resource_json(namespace, "services").await?;
    let cluster_ip = services["items"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item["metadata"]["name"] == "cangling-map")
        })
        .and_then(|item| item["spec"]["clusterIP"].as_str())
        .filter(|ip| *ip != "None")
        .ok_or_else(|| AppError::bad("cangling-map Service 没有 ClusterIP"))?;
    let target = host_path.join("china");
    let manifest: DataManifest =
        serde_json::from_slice(&fs::read(package.join("data-manifest.json"))?)
            .map_err(|error| AppError::bad(format!("基础数据清单无效：{error}")))?;
    if manifest.format != "cangling-map-ai-china-v1" || manifest.files.is_empty() {
        return Err(AppError::bad("基础数据清单格式不受支持"));
    }
    add_output(
        state,
        format!(
            "数据包：{}；{} 个文件",
            package.display(),
            manifest.files.len()
        ),
    );
    let jobs = state.jobs.clone();
    let id = job_id.to_string();
    let archive = package.join("shape-data.tar.gz");
    let extracted =
        tokio::task::spawn_blocking(move || extract_data(&archive, &target, &manifest, &jobs, &id))
            .await
            .map_err(|error| AppError::internal(format!("解包任务异常：{error}")))??;
    add_output(
        state,
        format!("数据文件校验完成：{extracted} 个；容器路径 /geodata/china"),
    );
    state
        .jobs
        .set(job_id, "importing", "正在导入目录、样式与图层", 75, 100);

    let bundle: Value = serde_json::from_slice(&fs::read(package.join("bundle.json"))?)
        .map_err(|error| AppError::bad(format!("图层配置无效：{error}")))?;
    let catalog_count = bundle["catalog"].as_array().map_or(0, Vec::len);
    let style_count = bundle["styles"].as_array().map_or(0, Vec::len);
    let layer_count = bundle["layers"].as_array().map_or(0, Vec::len);
    let total_steps = catalog_count + style_count + layer_count + 1;
    let mut command = Command::new("python3");
    command
        .arg(package.join("import.py"))
        .arg(format!("http://{cluster_ip}:3000"))
        .arg("/geodata/china")
        .arg("--skip-copy")
        .arg("--host-data-dir")
        .arg(host_path.join("china"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| AppError::internal(format!("启动导入脚本失败：{error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::internal("导入脚本没有标准输出"))?;
    let mut lines = BufReader::new(stdout).lines();
    let mut completed = 0usize;
    while let Some(line) = lines.next_line().await? {
        if line.starts_with("目录项:")
            || line.starts_with("样式:")
            || line.starts_with("发布图层:")
            || line.starts_with("导入完成:")
        {
            completed += 1;
            let percent = 75 + (completed * 24 / total_steps.max(1));
            state
                .jobs
                .set(job_id, "importing", &line, percent as u64, 100);
        }
        add_output(state, line);
    }
    let mut stderr = String::new();
    if let Some(stream) = child.stderr.take() {
        BufReader::new(stream).read_to_string(&mut stderr).await?;
    }
    let exit = child.wait().await?;
    if !exit.success() {
        return Err(AppError::bad(stderr.trim().to_string()));
    }
    let summary = format!(
        "基础数据导入完成：{catalog_count} 个目录项、{style_count} 个样式、{layer_count} 个发布图层、1 个 ai:china 图层组。重复导入会更新同名配置。"
    );
    add_output(state, summary.clone());
    state.jobs.finish_ok(job_id, &summary);
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, AppError> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn extract_data(
    archive_path: &Path,
    target: &Path,
    manifest: &DataManifest,
    jobs: &crate::progress::JobHub,
    job_id: &str,
) -> Result<usize, AppError> {
    fs::create_dir_all(target)?;
    let expected: HashMap<_, _> = manifest
        .files
        .iter()
        .map(|file| (file.name.as_str(), file))
        .collect();
    let mut seen = HashSet::new();
    let mut processed = 0u64;
    let decoder = GzDecoder::new(File::open(archive_path)?);
    let mut archive = Archive::new(decoder);
    for result in archive.entries()? {
        let mut entry = result?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(AppError::bad("基础数据包含非普通文件"));
        }
        let path = entry.path()?;
        let parts = path
            .components()
            .filter(|part| !matches!(part, Component::CurDir))
            .collect::<Vec<_>>();
        let name = match parts.as_slice() {
            [Component::Normal(name)] => name
                .to_str()
                .ok_or_else(|| AppError::bad("数据文件名不是 UTF-8"))?
                .to_string(),
            _ => return Err(AppError::bad("基础数据包含非法路径")),
        };
        let file = expected
            .get(name.as_str())
            .ok_or_else(|| AppError::bad(format!("清单外的数据文件：{name}")))?;
        if !seen.insert(name.clone()) || entry.size() != file.bytes {
            return Err(AppError::bad(format!("数据文件重复或大小不符：{name}")));
        }
        let destination = target.join(&name);
        let unchanged = destination.is_file() && sha256_file(&destination)? == file.sha256;
        let temp = target.join(format!(".{name}.init-{}", uuid::Uuid::new_v4()));
        let mut output = if unchanged {
            None
        } else {
            Some(File::create(&temp)?)
        };
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 1024 * 1024];
        loop {
            let n = entry.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            if let Some(out) = output.as_mut() {
                out.write_all(&buffer[..n])?;
            }
            processed += n as u64;
            let percent = (processed.saturating_mul(75) / manifest.total_bytes.max(1)).min(75);
            jobs.set(
                job_id,
                "extracting",
                &format!("正在校验数据文件 {name}"),
                percent,
                100,
            );
        }
        if format!("{:x}", hash.finalize()) != file.sha256 {
            if output.is_some() {
                let _ = fs::remove_file(&temp);
            }
            return Err(AppError::bad(format!("数据文件 SHA-256 校验失败：{name}")));
        }
        if let Some(out) = output {
            out.sync_all()?;
            drop(out);
            fs::set_permissions(&temp, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
            fs::rename(temp, destination)?;
        }
    }
    if seen.len() != expected.len() || processed != manifest.total_bytes {
        return Err(AppError::bad("基础数据包缺少文件或大小与清单不符"));
    }
    Ok(seen.len())
}
