//! k3s/containerd 离线镜像包管理与集群分发。

use crate::cluster::{Role, TOKEN_HEADER};
use crate::error::AppError;
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderValue};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::{Deserialize, Serialize};
use std::path::{Path as FsPath, PathBuf};
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
    images: Vec<String>,
    error: Option<String>,
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

#[derive(Debug, Serialize)]
struct ImportStarted {
    job_id: String,
}

pub fn console_routes() -> Router<AppState> {
    Router::new()
        .route("/api/images", get(overview))
        .route("/api/images/upload", post(upload))
        .route("/api/images/import", post(start_import))
}

pub fn cluster_routes() -> Router<AppState> {
    Router::new()
        .route("/api/cluster/images", get(worker_images))
        .route("/api/cluster/images/import", post(worker_import))
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
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

async fn installed_images() -> Result<Vec<String>, AppError> {
    let output = tokio::process::Command::new("k3s")
        .args(["ctr", "images", "list", "-q"])
        .output()
        .await
        .map_err(|e| AppError::internal(format!("执行 k3s 失败: {e}")))?;
    if !output.status.success() {
        return Err(AppError::internal(format!(
            "读取 k3s 镜像失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut images: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| visible_image(s))
        .map(str::to_string)
        .collect();
    images.sort();
    images.dedup();
    Ok(images)
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
                    images: serde_json::from_value(json).unwrap_or_default(),
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
    result: Result<Vec<String>, AppError>,
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
        let mut file = tokio::fs::File::create(&temp).await?;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| AppError::bad(e.to_string()))?
        {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);
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

fn target_nodes(
    state: &AppState,
    selected: &[String],
) -> Result<Vec<(String, String, String)>, AppError> {
    if state.cluster.role == Role::Standalone {
        return Ok(vec![("local".into(), "本机".into(), "localhost".into())]);
    }
    if state.cluster.role != Role::Master {
        return Err(AppError::bad("请在主节点执行集群镜像导入"));
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

async fn worker_images() -> Result<Json<Vec<String>>, AppError> {
    Ok(Json(installed_images().await?))
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
}
