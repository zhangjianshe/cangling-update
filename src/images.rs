//! k3s/containerd 离线镜像包管理与集群分发。

use crate::cluster::{Role, TOKEN_HEADER};
use crate::error::AppError;
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderValue};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path as FsPath, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

#[derive(Debug, Deserialize, Default)]
struct DirectoryQuery {
    directory: Option<String>,
}

#[derive(Debug, Serialize)]
struct ImagePackage {
    name: String,
    size: u64,
    modified: Option<String>,
}

#[derive(Debug, Serialize)]
struct NodeImages {
    node_id: String,
    node_name: String,
    address: String,
    status: String,
    images: Vec<InstalledImage>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledImage {
    name: String,
    size: u64,
}

#[derive(Debug, Serialize)]
struct ImageOverview {
    directory: String,
    packages: Vec<ImagePackage>,
    nodes: Vec<NodeImages>,
    role: String,
}

#[derive(Debug, Deserialize)]
struct ImportRequest {
    directory: Option<String>,
    filename: String,
    #[serde(default)]
    node_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct WorkerImportRequest {
    filename: String,
    source_url: String,
}

#[derive(Debug, Deserialize)]
struct DeleteRequest {
    filenames: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct NodeImageDeleteRequest {
    images: Vec<String>,
    #[serde(default)]
    node_ids: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct WorkerImageDeleteRequest {
    images: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ImportStarted {
    job_id: String,
}

pub fn console_routes() -> Router<AppState> {
    Router::new()
        .route("/api/images", get(overview))
        .route("/api/images/upload", post(upload))
        .route("/api/images/packages", delete(delete_packages))
        .route("/api/images/node-images", delete(start_node_image_delete))
        .route("/api/images/import", post(start_import))
}

pub fn cluster_routes() -> Router<AppState> {
    Router::new()
        .route("/api/cluster/images", get(worker_images))
        .route("/api/cluster/images/import", post(worker_import))
        .route("/api/cluster/images/delete", post(worker_image_delete))
        .route(
            "/api/cluster/images/archive/{filename}",
            get(download_archive),
        )
}

fn selected_dir(state: &AppState, raw: Option<&str>) -> Result<PathBuf, AppError> {
    let path = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state.images_dir.clone());
    if !path.is_absolute() {
        return Err(AppError::bad("镜像目录必须是绝对路径"));
    }
    Ok(path)
}

fn archive_name(raw: &str) -> Result<String, AppError> {
    let name = raw.trim();
    let valid = !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && (name.ends_with(".tar") || name.ends_with(".tar.gz") || name.ends_with(".tgz"));
    valid
        .then(|| name.to_string())
        .ok_or_else(|| AppError::bad("只允许 tar、tar.gz 或 tgz 镜像包"))
}

async fn scan_packages(dir: &FsPath) -> Result<Vec<ImagePackage>, AppError> {
    tokio::fs::create_dir_all(dir).await?;
    let mut rd = tokio::fs::read_dir(dir).await?;
    let mut out = Vec::new();
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if archive_name(&name).is_err() {
            continue;
        }
        let meta = entry.metadata().await?;
        if !meta.is_file() {
            continue;
        }
        let modified = meta
            .modified()
            .ok()
            .map(chrono::DateTime::<chrono::Utc>::from)
            .map(|v| v.to_rfc3339());
        out.push(ImagePackage {
            name,
            size: meta.len(),
            modified,
        });
    }
    out.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

async fn installed_images() -> Result<Vec<InstalledImage>, AppError> {
    let output = tokio::process::Command::new("k3s")
        .args(["ctr", "images", "list"])
        .output()
        .await
        .map_err(|e| AppError::internal(format!("执行 k3s 失败: {e}")))?;
    if !output.status.success() {
        return Err(AppError::internal(format!(
            "读取 k3s 镜像失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(parse_image_list(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_image_list(output: &str) -> Vec<InstalledImage> {
    let mut images = std::collections::BTreeMap::new();
    for line in output.lines().map(str::trim) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 5 || fields[0] == "REF" || !visible_image(fields[0]) {
            continue;
        }
        let size = parse_image_size(fields[3], fields[4]).unwrap_or(0);
        images.insert(fields[0].to_string(), size);
    }
    images
        .into_iter()
        .map(|(name, size)| InstalledImage { name, size })
        .collect()
}

fn parse_image_size(value: &str, unit: &str) -> Option<u64> {
    let value = value.parse::<f64>().ok()?;
    let factor = match unit.to_ascii_lowercase().as_str() {
        "b" => 1f64,
        "kib" => 1024f64,
        "mib" => 1024f64.powi(2),
        "gib" => 1024f64.powi(3),
        "kb" => 1000f64,
        "mb" => 1000f64.powi(2),
        "gb" => 1000f64.powi(3),
        _ => return None,
    };
    Some((value * factor).round() as u64)
}

fn visible_image(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with("sha256:")
        && !name.starts_with("docker.io/rancher/")
        && !name.starts_with("docker.io.rancher/")
}

async fn overview(
    State(state): State<AppState>,
    Query(q): Query<DirectoryQuery>,
) -> Result<Json<ImageOverview>, AppError> {
    let dir = selected_dir(&state, q.directory.as_deref())?;
    let packages = scan_packages(&dir).await?;
    let nodes = collect_nodes(&state).await?;
    Ok(Json(ImageOverview {
        directory: dir.display().to_string(),
        packages,
        nodes,
        role: state.cluster.role.as_str().into(),
    }))
}

fn is_online(last_seen: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(last_seen)
        .ok()
        .map(|t| {
            chrono::Utc::now()
                .signed_duration_since(t.with_timezone(&chrono::Utc))
                .num_seconds()
                < crate::cluster::OFFLINE_AFTER_SECS as i64
        })
        .unwrap_or(false)
}

async fn collect_nodes(state: &AppState) -> Result<Vec<NodeImages>, AppError> {
    let rows = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let mut stmt =
            conn.prepare("SELECT id,name,addr,role,last_seen FROM cluster_nodes ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    if rows.is_empty() || state.cluster.role == Role::Standalone {
        return Ok(vec![local_node(
            "local",
            "本机",
            "localhost",
            installed_images().await,
        )]);
    }
    let self_id = crate::cluster::load_or_create_node_id(&state.paths);
    let token = state.cluster.token.clone().unwrap_or_default();
    let mut out = Vec::new();
    for (id, name, addr, _role, last_seen) in rows {
        if !is_online(&last_seen) {
            out.push(NodeImages {
                node_id: id,
                node_name: name,
                address: addr,
                status: "offline".into(),
                images: vec![],
                error: None,
            });
        } else if id == self_id {
            out.push(local_node(&id, &name, &addr, installed_images().await));
        } else {
            let url = format!("http://{addr}/api/cluster/images");
            match crate::cluster::http::get_json(&url, &token).await {
                Ok((status, json)) if status.is_success() => out.push(NodeImages {
                    node_id: id,
                    node_name: name,
                    address: addr,
                    status: "online".into(),
                    images: parse_remote_images(json),
                    error: None,
                }),
                Ok((status, json)) => out.push(NodeImages {
                    node_id: id,
                    node_name: name,
                    address: addr,
                    status: "error".into(),
                    images: vec![],
                    error: Some(format!("HTTP {status}: {json}")),
                }),
                Err(e) => out.push(NodeImages {
                    node_id: id,
                    node_name: name,
                    address: addr,
                    status: "error".into(),
                    images: vec![],
                    error: Some(e.to_string()),
                }),
            }
        }
    }
    Ok(out)
}

fn local_node(
    id: &str,
    name: &str,
    addr: &str,
    result: Result<Vec<InstalledImage>, AppError>,
) -> NodeImages {
    match result {
        Ok(images) => NodeImages {
            node_id: id.into(),
            node_name: name.into(),
            address: addr.into(),
            status: "online".into(),
            images,
            error: None,
        },
        Err(e) => NodeImages {
            node_id: id.into(),
            node_name: name.into(),
            address: addr.into(),
            status: "error".into(),
            images: vec![],
            error: Some(e.to_string()),
        },
    }
}

fn parse_remote_images(value: serde_json::Value) -> Vec<InstalledImage> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            if let Some(name) = item.as_str() {
                return Some(InstalledImage {
                    name: name.to_string(),
                    size: 0,
                });
            }
            serde_json::from_value(item.clone()).ok()
        })
        .collect()
}

async fn upload(
    State(state): State<AppState>,
    Query(q): Query<DirectoryQuery>,
    mut multipart: Multipart,
) -> Result<Json<ImagePackage>, AppError> {
    let dir = selected_dir(&state, q.directory.as_deref())?;
    tokio::fs::create_dir_all(&dir).await?;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::bad(e.to_string()))?
    {
        let Some(raw) = field.file_name() else {
            continue;
        };
        let name = archive_name(raw)?;
        let temp = dir.join(format!(".{name}.{}.part", Uuid::new_v4()));
        let target = dir.join(&name);
        let write_result: Result<(), AppError> = async {
            let mut file = tokio::fs::File::create(&temp).await?;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|e| AppError::bad(e.to_string()))?
            {
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            Ok(())
        }
        .await;
        if let Err(err) = write_result {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(err);
        }
        tokio::fs::rename(temp, &target).await?;
        let size = tokio::fs::metadata(target).await?.len();
        return Ok(Json(ImagePackage {
            name,
            size,
            modified: None,
        }));
    }
    Err(AppError::bad("没有收到镜像包文件"))
}

async fn delete_packages(
    State(state): State<AppState>,
    Query(q): Query<DirectoryQuery>,
    Json(body): Json<DeleteRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    if body.filenames.is_empty() {
        return Err(AppError::bad("请选择要删除的镜像包"));
    }
    let dir = selected_dir(&state, q.directory.as_deref())?;
    let names = body
        .filenames
        .iter()
        .map(|name| archive_name(name))
        .collect::<Result<Vec<_>, _>>()?;
    let mut deleted = 0usize;
    for name in names {
        match tokio::fs::remove_file(dir.join(name)).await {
            Ok(()) => deleted += 1,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(AppError::internal(format!("删除镜像包失败: {err}"))),
        }
    }
    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

async fn start_import(
    State(state): State<AppState>,
    Json(body): Json<ImportRequest>,
) -> Result<Json<ImportStarted>, AppError> {
    let dir = selected_dir(&state, body.directory.as_deref())?;
    let filename = archive_name(&body.filename)?;
    if !dir.join(&filename).is_file() {
        return Err(AppError::not_found("镜像包不存在"));
    }
    let job = state.jobs.create();
    let job_id = job.id.clone();
    let run_state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = import_cluster(&run_state, &dir, &filename, &body.node_ids, &job_id).await {
            run_state.jobs.finish_err(&job_id, &e.to_string());
        }
    });
    Ok(Json(ImportStarted { job_id: job.id }))
}

async fn start_node_image_delete(
    State(state): State<AppState>,
    Json(body): Json<NodeImageDeleteRequest>,
) -> Result<Json<ImportStarted>, AppError> {
    let images = validate_image_refs(&body.images)?;
    if body.node_ids.is_empty() {
        return Err(AppError::bad("请选择目标节点"));
    }
    let job = state.jobs.create();
    let job_id = job.id.clone();
    let run_state = state.clone();
    tokio::spawn(async move {
        if let Err(err) = delete_cluster_images(&run_state, &images, &body.node_ids, &job_id).await
        {
            run_state.jobs.finish_err(&job_id, &err.to_string());
        }
    });
    Ok(Json(ImportStarted { job_id: job.id }))
}

fn validate_image_refs(images: &[String]) -> Result<Vec<String>, AppError> {
    if images.is_empty() {
        return Err(AppError::bad("请选择要删除的节点镜像"));
    }
    images
        .iter()
        .map(|raw| {
            let image = raw.trim();
            if image.is_empty()
                || image.starts_with('-')
                || image.chars().any(char::is_whitespace)
                || image.chars().any(char::is_control)
            {
                return Err(AppError::bad(format!("无效的镜像名称: {raw}")));
            }
            Ok(image.to_string())
        })
        .collect()
}

async fn delete_cluster_images(
    state: &AppState,
    images: &[String],
    selected: &[String],
    job_id: &str,
) -> Result<(), AppError> {
    let targets = target_nodes(state, selected)?;
    if targets.is_empty() {
        return Err(AppError::bad("没有可操作的在线节点"));
    }
    let total = targets.len() as u64;
    let self_id = crate::cluster::load_or_create_node_id(&state.paths);
    let token = state.cluster.token.as_deref().unwrap_or("");
    for (index, (id, name, addr)) in targets.into_iter().enumerate() {
        state.jobs.set(
            job_id,
            "delete-images",
            &format!("正在删除 {name} 上的所选镜像"),
            index as u64,
            total,
        );
        if id == self_id || state.cluster.role == Role::Standalone {
            remove_images(images).await?;
        } else {
            let url = format!("http://{addr}/api/cluster/images/delete");
            let request = WorkerImageDeleteRequest {
                images: images.to_vec(),
            };
            let (status, json) = crate::cluster::http::post_json(
                &url,
                token,
                &serde_json::to_value(request).map_err(|e| AppError::internal(e.to_string()))?,
            )
            .await
            .map_err(AppError::from)?;
            if !status.is_success() {
                return Err(AppError::internal(format!(
                    "节点 {name} 删除镜像失败: HTTP {status} {json}"
                )));
            }
        }
        state.jobs.set(
            job_id,
            "delete-images",
            &format!("{name} 删除完成"),
            index as u64 + 1,
            total,
        );
    }
    state.jobs.finish_ok(job_id, "已从所选节点删除镜像");
    Ok(())
}

fn target_nodes(
    state: &AppState,
    selected: &[String],
) -> Result<Vec<(String, String, String)>, AppError> {
    if state.cluster.role == Role::Standalone {
        return Ok(vec![("local".into(), "本机".into(), "localhost".into())]);
    }
    if state.cluster.role != Role::Master {
        return Err(AppError::bad("请在主节点执行集群镜像操作"));
    }
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let mut stmt =
        conn.prepare("SELECT id,name,addr,last_seen FROM cluster_nodes ORDER BY name")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, name, addr, last) = row?;
        if is_online(&last) && (selected.is_empty() || selected.contains(&id)) {
            out.push((id, name, addr));
        }
    }
    Ok(out)
}

fn master_addr(state: &AppState) -> Result<String, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    conn.query_row(
        "SELECT addr FROM cluster_nodes WHERE role='master' ORDER BY last_seen DESC LIMIT 1",
        [],
        |r| r.get(0),
    )
    .map_err(|_| AppError::internal("无法确定主节点地址"))
}

fn query_escape(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

async fn import_cluster(
    state: &AppState,
    dir: &FsPath,
    filename: &str,
    selected: &[String],
    job_id: &str,
) -> Result<(), AppError> {
    let targets = target_nodes(state, selected)?;
    if targets.is_empty() {
        return Err(AppError::bad("没有可导入的在线节点"));
    }
    let total = targets.len() as u64;
    let self_id = crate::cluster::load_or_create_node_id(&state.paths);
    let token = state.cluster.token.as_deref().unwrap_or("");
    let master = master_addr(state).unwrap_or_else(|_| format!("127.0.0.1:{}", state.port));
    for (idx, (id, name, addr)) in targets.into_iter().enumerate() {
        state.jobs.set(
            job_id,
            "import",
            &format!("正在向 {name} 导入 {filename}"),
            idx as u64,
            total,
        );
        if id == self_id || state.cluster.role == Role::Standalone {
            import_file(&dir.join(filename)).await?;
        } else {
            let source_url = format!(
                "http://{master}/api/cluster/images/archive/{}?directory={}",
                query_escape(filename),
                query_escape(&dir.display().to_string())
            );
            let body = serde_json::json!({"filename":filename,"source_url":source_url});
            let url = format!("http://{addr}/api/cluster/images/import");
            let (status, json) = crate::cluster::http::post_json(&url, token, &body)
                .await
                .map_err(AppError::from)?;
            if !status.is_success() {
                return Err(AppError::internal(format!(
                    "节点 {name} 导入失败: HTTP {status} {json}"
                )));
            }
        }
        state.jobs.set(
            job_id,
            "import",
            &format!("{name} 导入完成"),
            idx as u64 + 1,
            total,
        );
    }
    state
        .jobs
        .finish_ok(job_id, &format!("{filename} 已导入全部所选节点"));
    Ok(())
}

async fn worker_images() -> Result<Json<Vec<InstalledImage>>, AppError> {
    Ok(Json(installed_images().await?))
}

async fn worker_image_delete(
    Json(body): Json<WorkerImageDeleteRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let images = validate_image_refs(&body.images)?;
    remove_images(&images).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn remove_images(images: &[String]) -> Result<(), AppError> {
    let installed: std::collections::HashSet<_> = installed_images()
        .await?
        .into_iter()
        .map(|image| image.name)
        .collect();
    for image in images {
        if !installed.contains(image) {
            continue;
        }
        let output = tokio::process::Command::new("k3s")
            .args(["ctr", "images", "remove"])
            .arg(image)
            .output()
            .await
            .map_err(|e| AppError::internal(format!("执行 k3s 失败: {e}")))?;
        if !output.status.success() {
            return Err(AppError::internal(format!(
                "删除镜像 {image} 失败: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }
    Ok(())
}

async fn worker_import(
    State(state): State<AppState>,
    Json(body): Json<WorkerImportRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let filename = archive_name(&body.filename)?;
    let dir = state.images_dir.clone();
    tokio::fs::create_dir_all(&dir).await?;
    let target = dir.join(&filename);
    let temp = dir.join(format!(".{filename}.{}.part", Uuid::new_v4()));
    download_to(
        &body.source_url,
        state.cluster.token.as_deref().unwrap_or(""),
        &temp,
    )
    .await?;
    tokio::fs::rename(temp, &target).await?;
    import_file(&target).await?;
    Ok(Json(serde_json::json!({"ok":true})))
}

async fn import_file(path: &FsPath) -> Result<(), AppError> {
    if is_gzip_archive(path) {
        return import_gzip_file(path).await;
    }
    let output = tokio::process::Command::new("k3s")
        .args(["ctr", "images", "import"])
        .arg(path)
        .output()
        .await
        .map_err(|e| AppError::internal(format!("执行 k3s 失败: {e}")))?;
    if !output.status.success() {
        return Err(AppError::internal(format!(
            "导入镜像失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn is_gzip_archive(path: &FsPath) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name.ends_with(".tar.gz") || name.ends_with(".tgz")
}

async fn import_gzip_file(path: &FsPath) -> Result<(), AppError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path).map_err(|e| format!("打开 gzip 镜像包失败: {e}"))?;
        let mut decoder = flate2::read::GzDecoder::new(file);
        let mut child = std::process::Command::new("k3s")
            .args(["ctr", "images", "import", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("执行 k3s 失败: {e}"))?;
        let copy_result = {
            let mut stdin = child.stdin.take().ok_or("无法打开 k3s 标准输入")?;
            io::copy(&mut decoder, &mut stdin)
        };
        let output = child
            .wait_with_output()
            .map_err(|e| format!("等待 k3s 导入完成失败: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "导入镜像失败: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        copy_result.map_err(|e| format!("解压 gzip 镜像包失败: {e}"))?;
        Ok(())
    })
    .await
    .map_err(|e| AppError::internal(format!("镜像导入任务失败: {e}")))?
    .map_err(AppError::internal)
}

async fn download_archive(
    State(state): State<AppState>,
    Path(raw): Path<String>,
    Query(q): Query<DirectoryQuery>,
) -> Result<Response, AppError> {
    let name = archive_name(&raw)?;
    let dir = selected_dir(&state, q.directory.as_deref())?;
    let path = dir.join(name);
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| AppError::not_found("镜像包不存在"))?;
    let len = file.metadata().await?.len();
    let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&len.to_string()).map_err(|e| AppError::internal(e.to_string()))?,
    );
    Ok(response)
}

async fn download_to(url: &str, token: &str, path: &FsPath) -> Result<(), AppError> {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(std::time::Duration::from_secs(10)));
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build(connector);
    let req = hyper::Request::builder()
        .uri(url)
        .header(TOKEN_HEADER, token)
        .body(Full::new(Bytes::new()))
        .map_err(|e| AppError::internal(e.to_string()))?;
    let mut response = client
        .request(req)
        .await
        .map_err(|e| AppError::internal(format!("下载镜像包失败: {e}")))?;
    if !response.status().is_success() {
        return Err(AppError::internal(format!(
            "下载镜像包失败: HTTP {}",
            response.status()
        )));
    }
    let mut file = tokio::fs::File::create(path).await?;
    while let Some(frame) = response.frame().await {
        let frame = frame.map_err(|e| AppError::internal(e.to_string()))?;
        if let Some(data) = frame.data_ref() {
            file.write_all(data).await?;
        }
    }
    file.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_archive_names() {
        assert!(archive_name("images.tar.gz").is_ok());
        assert!(archive_name("../images.tar").is_err());
        assert!(archive_name("image.zip").is_err());
    }
    #[test]
    fn escapes_query_values() {
        assert_eq!(query_escape("/opt/a b"), "%2Fopt%2Fa%20b");
    }
    #[test]
    fn hides_k3s_system_and_digest_only_images() {
        assert!(!visible_image("docker.io/rancher/mirrored-pause:3.6"));
        assert!(!visible_image("sha256:2d61ae04c2b8"));
        assert!(!visible_image("docker.io.rancher/legacy:test"));
        assert!(visible_image("registry.example.com/np4/service:latest"));
    }
    #[test]
    fn detects_gzip_image_archives() {
        assert!(is_gzip_archive(FsPath::new("image.tar.gz")));
        assert!(is_gzip_archive(FsPath::new("IMAGE.TGZ")));
        assert!(!is_gzip_archive(FsPath::new("image.tar")));
    }
    #[test]
    fn validates_node_image_references() {
        assert!(validate_image_refs(&["registry/app:latest".into()]).is_ok());
        assert!(validate_image_refs(&["--all".into()]).is_err());
        assert!(validate_image_refs(&["bad image".into()]).is_err());
    }
    #[test]
    fn parses_ctr_image_names_and_sizes() {
        let output = "REF TYPE DIGEST SIZE PLATFORMS LABELS\nregistry.local/team/broker:1.2 application/vnd.oci.image.manifest.v1+json sha256:abc 35.6 MiB linux/amd64 -\ndocker.io/rancher/pause:3.6 application/vnd.oci.image.manifest.v1+json sha256:def 300.0 KiB linux/amd64 -\n";
        let images = parse_image_list(output);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].name, "registry.local/team/broker:1.2");
        assert_eq!(images[0].size, 37_329_306);
    }
}
