use crate::auth;
use crate::cluster::Role;
use crate::backup::{
    dir_size, project_dir_size, remove_dir_if_exists, restore_directory,
    restore_directory_with_progress, snapshot_directory, snapshot_directory_with_progress,
};
use crate::db;
use crate::docker::{parse_compose_ps, to_latest_tag};
use crate::error::AppError;
use crate::hostinfo;
use crate::models::*;
use crate::paths::{
    commit_compose_draft, compose_draft_path, compose_etag, compose_live_path, env_live_path,
    find_compose_file, find_env_file, is_image_archive_name, is_uploadable_name,
    parse_compose_images, parse_compose_jar_mounts, require_absolute_dir, resolve_host_path,
    safe_filename, validate_compose_text, validate_env_text, write_text_atomic, JarMount,
};
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{header, HeaderValue};
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use std::io::{BufReader, Read};
use std::path::{Path as FsPath, PathBuf};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;
use walkdir::WalkDir;

const NP4_PROJECT_DIR: &str = "/opt/cangling-np4";
const HARBOR_PROJECT_DIR: &str = "/opt/cangling/cangling-zot";
const HARBOR_TEMPLATE_REL: &str = "images/base-images/latest/linux/all/cangling-zot.tar.gz";

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(crate::portal::page))
        .route("/console", get(index))
        .route("/hostinfo", get(hostinfo_term))
        .route("/hostinfo.md", get(hostinfo_markdown))
        .route(
            "/api/hostinfo/note",
            get(hostinfo_note).put(save_hostinfo_note),
        )
        .merge(crate::portal::routes())
        .route("/api/auth/status", get(auth::status))
        .route("/api/auth/setup", post(auth::setup))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/change-password", post(auth::change_password))
        .route("/api/jobs", post(create_job))
        .route("/api/jobs/{id}", get(get_job))
        .route("/api/meta", get(meta))
        .route("/api/repo", get(crate::repo::list))
        .route(
            "/api/repo/{tab}/{package}/download",
            get(crate::repo::download),
        )
        .route("/api/repo/install", post(crate::repo::install))
        .route("/api/gitrepo/status", get(crate::gitrepo::status))
        .route("/api/gitrepo/tree", get(crate::gitrepo::tree))
        .route("/api/gitrepo/list", get(crate::gitrepo::list))
        .route("/api/gitrepo/file", get(crate::gitrepo::file))
        .route("/api/validate-directory", post(validate_directory))
        .route("/api/browse-directory", post(browse_directory))
        .route("/api/create-directory", post(create_directory))
        .route("/api/orphans", get(list_orphans))
        .route("/api/orphans/{*id}", axum::routing::delete(delete_orphan))
        .route("/api/np4/deploy/status", get(np4_deploy_status))
        .route("/api/np4/deploy", post(deploy_np4))
        .route("/api/harbor/deploy/status", get(harbor_deploy_status))
        .route("/api/harbor/deploy", post(deploy_harbor))
        .route("/api/projects", get(list_projects).post(create_project))
        .route(
            "/api/projects/{id}",
            get(get_project).put(update_project).delete(delete_project),
        )
        .route("/api/projects/{id}/versions", get(list_versions))
        .route("/api/projects/{id}/files", get(project_files_list))
        .route(
            "/api/projects/{id}/files/content",
            get(project_file_get).put(project_file_put),
        )
        .route("/api/projects/{id}/updates", post(create_update))
        .route(
            "/api/projects/{id}/temp-upload",
            get(temp_upload_status).put(temp_upload_chunk),
        )
        .route(
            "/api/projects/{id}/np4-repo-update",
            post(update_np4_from_repo),
        )
        .route(
            "/api/projects/{id}/zot/environment",
            post(check_zot_environment),
        )
        .route("/api/projects/{id}/replace", post(create_replace))
        .route("/api/projects/{id}/rollback", post(rollback))
        .route("/api/projects/{id}/compose", get(compose_status))
        .route(
            "/api/projects/{id}/compose/file",
            get(compose_file_get).put(compose_file_put),
        )
        .route(
            "/api/projects/{id}/compose/revisions",
            get(compose_revisions_list),
        )
        .route(
            "/api/projects/{id}/compose/revisions/{rev_id}",
            get(compose_revision_get),
        )
        .route(
            "/api/projects/{id}/compose/revisions/{rev_id}/restore",
            post(compose_revision_restore),
        )
        .route(
            "/api/projects/{id}/env/file",
            get(env_file_get).put(env_file_put),
        )
        .route("/api/projects/{id}/env/revisions", get(env_revisions_list))
        .route(
            "/api/projects/{id}/env/revisions/{rev_id}",
            get(env_revision_get),
        )
        .route(
            "/api/projects/{id}/env/revisions/{rev_id}/restore",
            post(env_revision_restore),
        )
        .route("/api/projects/{id}/compose/up", post(compose_up))
        .route("/api/projects/{id}/compose/down", post(compose_down))
        .route("/api/projects/{id}/compose/restart", post(compose_restart))
        .route(
            "/api/projects/{id}/compose/restart/{service}",
            post(compose_restart_service),
        )
        .route("/api/projects/{id}/compose/logs", get(compose_logs))
        .route(
            "/api/projects/{id}/compose/exec/{service}",
            get(compose_exec),
        )
        .route("/api/projects/{id}/db/meta", get(db_meta))
        .route("/api/projects/{id}/db/databases", get(db_databases))
        .route("/api/projects/{id}/db/schemas", get(db_schemas))
        .route("/api/projects/{id}/db/objects", get(db_objects))
        .route("/api/projects/{id}/db/rows", get(db_rows))
        .route("/api/projects/{id}/db/query", post(db_query))
        .route(
            "/api/projects/{id}/db/backups",
            get(db_backups).post(db_backup_create),
        )
        .route(
            "/api/projects/{id}/db/backups/{backup_id}/restore",
            post(db_backup_restore),
        )
        .route(
            "/api/projects/{id}/db/backup-schedule",
            get(db_backup_schedule_get).put(db_backup_schedule_save),
        )
        .route(
            "/api/projects/{id}/db/backup-schedule/run",
            post(db_backup_schedule_run),
        )
        .route(
            "/api/projects/{id}/db/row",
            post(db_update_row).delete(db_delete_row),
        )
        .route("/vendor/xterm.css", get(vendor_xterm_css))
        .route("/vendor/xterm.js", get(vendor_xterm_js))
        .route("/vendor/xterm-addon-fit.js", get(vendor_xterm_fit))
        .route("/vendor/compose-canvas.js", get(vendor_compose_canvas))
        .route("/vendor/ace/{*name}", get(vendor_ace))
        .route(
            "/vendor/iosevka-term-regular.woff2",
            get(vendor_iosevka_regular),
        )
        .route("/vendor/iosevka-term-bold.woff2", get(vendor_iosevka_bold))
        .route(
            "/api/cluster/nodes",
            get(crate::cluster::server::list_nodes),
        )
        .route(
            "/api/cluster/status",
            get(crate::cluster::server::cluster_status),
        )
        .route("/api/cluster/init", post(crate::cluster::init::start_init))
        .route(
            "/api/cluster/check",
            post(crate::cluster::init::start_check),
        )
        .route(
            "/api/cluster/init/status",
            get(crate::cluster::init::status),
        )
        .merge(crate::images::console_routes())
        .route(
            "/api/storages",
            get(crate::storage::list_storages).post(crate::storage::create_storage),
        )
        .route(
            "/api/storages/{id}",
            get(crate::storage::get_storage)
                .put(crate::storage::update_storage)
                .delete(crate::storage::delete_storage),
        )
        .route("/api/storages/{id}/deploy", post(crate::storage::deploy_storage))
        .route("/api/storages/{id}/unmount", post(crate::storage::unmount_storage))
        .route(
            "/api/storages/{id}/start-share",
            post(crate::storage::start_share),
        )
        .merge(crate::cluster::server::m2m_routes(state.clone()))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .with_state(state)
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024 * 1024))
}

#[derive(Debug, Deserialize, Default)]
struct HostinfoQuery {
    color: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct HostinfoNote {
    note: String,
}

#[derive(Debug, Deserialize)]
struct SaveHostinfoNote {
    note: String,
}

async fn hostinfo_note(State(state): State<AppState>) -> Json<HostinfoNote> {
    Json(HostinfoNote {
        note: hostinfo::load_note(&state.paths),
    })
}

async fn save_hostinfo_note(
    State(state): State<AppState>,
    Json(body): Json<SaveHostinfoNote>,
) -> Result<Json<HostinfoNote>, AppError> {
    hostinfo::save_note(&state.paths, &body.note).map_err(AppError::from)?;
    Ok(Json(HostinfoNote {
        note: hostinfo::load_note(&state.paths),
    }))
}

async fn hostinfo_term(
    State(state): State<AppState>,
    Query(q): Query<HostinfoQuery>,
) -> Result<impl IntoResponse, AppError> {
    let color = hostinfo::want_color(q.color.as_deref());
    let snap = load_host_snapshot(&state).await?;
    Ok(plain_text(hostinfo::render_ansi(&snap, color)))
}

async fn hostinfo_markdown(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    let snap = load_host_snapshot(&state).await?;
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/markdown; charset=utf-8"),
        )],
        hostinfo::render_markdown(&snap),
    ))
}

async fn load_host_snapshot(state: &AppState) -> Result<hostinfo::HostSnapshot, AppError> {
    let projects = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::list_projects(&conn)?
    };
    let paths = state.paths.clone();
    let port = state.port;
    let mut snap =
        tokio::task::spawn_blocking(move || hostinfo::collect_with_projects(&paths, projects))
            .await
            .map_err(|e| AppError::internal(e.to_string()))?;
    snap.listen = Some(("0.0.0.0".into(), port));
    Ok(snap)
}

fn plain_text(body: String) -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
}

async fn create_job(State(state): State<AppState>) -> Json<crate::progress::JobProgress> {
    Json(state.jobs.create())
}

async fn get_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<crate::progress::JobProgress>, AppError> {
    state
        .jobs
        .get(&id)
        .map(Json)
        .ok_or_else(|| AppError::not_found("进度任务不存在"))
}

fn job_set(
    state: &AppState,
    job_id: Option<&str>,
    phase: &str,
    message: &str,
    current: u64,
    total: u64,
) {
    if let Some(id) = job_id {
        state.jobs.set(id, phase, message, current, total);
    }
}

fn job_ok(state: &AppState, job_id: Option<&str>, message: &str) {
    if let Some(id) = job_id {
        state.jobs.finish_ok(id, message);
    }
}

fn job_err(state: &AppState, job_id: Option<&str>, error: &str) {
    if let Some(id) = job_id {
        state.jobs.finish_err(id, error);
    }
}

async fn compose_down_for_backup(
    state: &AppState,
    live: &std::path::Path,
    job_id: Option<&str>,
    stop: bool,
) -> Result<bool, AppError> {
    if !stop {
        return Ok(false);
    }
    job_set(
        state,
        job_id,
        "compose",
        "正在停止 Compose，以便全量备份…",
        0,
        0,
    );
    state
        .docker
        .compose_down(live)
        .await
        .map_err(|e| AppError::bad(format!("停止 Compose 失败，已取消备份：{e}")))?;
    Ok(true)
}

async fn compose_up_best_effort(
    state: &AppState,
    live: &std::path::Path,
    job_id: Option<&str>,
    message: &str,
) {
    job_set(state, job_id, "compose", message, 0, 0);
    if let Err(err) = state.docker.compose_up(live).await {
        tracing::warn!("compose up after backup: {err:#}");
    }
}

fn snapshot_blocking(
    src: PathBuf,
    dst: PathBuf,
    jobs: crate::progress::JobHub,
    job_id: Option<String>,
) -> anyhow::Result<u64> {
    match job_id {
        Some(id) => snapshot_directory_with_progress(&src, &dst, |done, total, name| {
            jobs.set(&id, "snapshot", &format!("备份 {name}"), done, total);
        }),
        None => snapshot_directory(&src, &dst),
    }
}

fn restore_blocking(
    snapshot: PathBuf,
    live: PathBuf,
    jobs: crate::progress::JobHub,
    job_id: Option<String>,
) -> anyhow::Result<()> {
    match job_id {
        Some(id) => restore_directory_with_progress(&snapshot, &live, |done, total, name| {
            jobs.set(&id, "restore", &format!("恢复 {name}"), done, total);
        }),
        None => restore_directory(&snapshot, &live),
    }
}

async fn index() -> impl IntoResponse {
    let html =
        include_str!("assets/index.html").replace("__APP_VERSION__", env!("CARGO_PKG_VERSION"));
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html)
}

async fn meta(State(state): State<AppState>) -> Json<Meta> {
    Json(Meta {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        exe_dir: state.paths.exe_dir.display().to_string(),
        config_dir: state.paths.config_dir.display().to_string(),
        db_path: state.paths.db_path.display().to_string(),
        port: state.port,
        docker: state.docker.meta().await,
    })
}

async fn validate_directory(
    Json(body): Json<ValidateDirBody>,
) -> Result<Json<ValidateDirResult>, AppError> {
    Ok(Json(inspect_directory(&body.directory)?))
}

fn inspect_directory(raw: &str) -> Result<ValidateDirResult, AppError> {
    let dir = require_absolute_dir(raw).map_err(|e| AppError::bad(e.to_string()))?;
    let compose = find_compose_file(&dir);
    let (images, jar_mounts, warning) = match &compose {
        Some(path) => {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            (
                parse_compose_images(&text),
                parse_compose_jar_mounts(&text),
                None,
            )
        }
        None => (
            Vec::new(),
            Vec::new(),
            Some("该目录中未找到 docker-compose.yml / compose.yaml".into()),
        ),
    };
    Ok(ValidateDirResult {
        ok: compose.is_some(),
        directory: dir.display().to_string(),
        compose_file: compose.map(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }),
        images,
        jar_mounts,
        warning,
    })
}

async fn browse_directory(
    Json(body): Json<BrowseDirBody>,
) -> Result<Json<BrowseDirResult>, AppError> {
    let dir = if body.path.trim().is_empty() {
        PathBuf::from("/")
    } else {
        require_absolute_dir(&body.path).map_err(|e| AppError::bad(e.to_string()))?
    };
    let dir = dir.canonicalize().unwrap_or(dir);

    let mut entries = Vec::new();
    let rd = std::fs::read_dir(&dir)
        .map_err(|e| AppError::internal(format!("无法读取目录 {}：{e}", dir.display())))?;
    for ent in rd.flatten() {
        let path = ent.path();
        if !path.is_dir() {
            continue;
        }
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        entries.push(BrowseDirEntry {
            has_compose: find_compose_file(&path).is_some(),
            name,
            path: path.display().to_string(),
        });
    }
    entries.sort_by(|a, b| {
        b.has_compose
            .cmp(&a.has_compose)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let parent = dir.parent().map(|p| p.display().to_string());
    Ok(Json(BrowseDirResult {
        path: dir.display().to_string(),
        parent,
        entries,
    }))
}

async fn create_directory(
    Json(body): Json<CreateDirBody>,
) -> Result<Json<CreateDirResult>, AppError> {
    let path = create_subdirectory(&body.parent, &body.name)?;
    Ok(Json(CreateDirResult {
        path: path.display().to_string(),
    }))
}

fn create_subdirectory(parent: &str, name: &str) -> Result<PathBuf, AppError> {
    let parent = require_absolute_dir(parent).map_err(|e| AppError::bad(e.to_string()))?;
    let name = name.trim();
    let mut components = FsPath::new(name).components();
    let valid = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
        && !name.starts_with('.');
    if !valid {
        return Err(AppError::bad("目录名不能为空、隐藏名称或包含路径分隔符"));
    }
    let path = parent.join(name);
    std::fs::create_dir(&path).map_err(|error| match error.kind() {
        std::io::ErrorKind::AlreadyExists => AppError::conflict(format!("目录已存在：{}", path.display())),
        _ => AppError::internal(format!("无法创建目录 {}：{error}", path.display())),
    })?;
    Ok(path.canonicalize().unwrap_or(path))
}

async fn list_projects(State(state): State<AppState>) -> Result<Json<Vec<Project>>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    Ok(Json(db::list_projects(&conn)?))
}

fn np4_arch() -> Result<(&'static str, &'static [&'static str]), AppError> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(("x86", &["x86", "amd64", "linux-x86", "linux-amd64"])),
        "aarch64" => Ok(("arm", &["arm", "arm64", "aarch64", "linux-arm64", "kylin-arm"])),
        arch => Err(AppError::bad(format!("NP4 部署暂不支持本机架构 {arch}"))),
    }
}

fn np4_template_dir(exe_dir: &FsPath) -> Option<PathBuf> {
    let repo = exe_dir.join("repo");
    [
        repo.join("cangling-np4"),
        repo.join("np4").join("cangling-np4"),
    ]
        .into_iter()
        .find(|dir| {
            dir.join("docker-compose-x86.yaml").is_file()
                && dir.join("docker-compose-arm.yaml").is_file()
        })
}

/// NP4 images are published as their own software package, separate from the
/// deployment-template package (`cangling-np4`).
fn np4_image_package_dir(exe_dir: &FsPath) -> Option<PathBuf> {
    let repo = exe_dir.join("repo");
    [repo.join("np4"), repo.join("np4").join("base-images")]
        .into_iter()
        .find(|dir| {
            let base = if dir.file_name().and_then(|name| name.to_str()) == Some("base-images") {
                dir.to_path_buf()
            } else {
                dir.join("base-images")
            };
            base.is_dir()
        })
}

fn np4_image_archives(image_package: &FsPath, aliases: &[&str]) -> Vec<PathBuf> {
    let base = if image_package.file_name().and_then(|name| name.to_str()) == Some("base-images") {
        image_package.to_path_buf()
    } else {
        image_package.join("base-images")
    };
    if !base.is_dir() {
        return Vec::new();
    }
    let selected = aliases
        .iter()
        .map(|alias| base.join(alias))
        .find(|dir| dir.is_dir());
    let search_root = selected.as_deref().unwrap_or(&base);
    let filter_flat = selected.is_none();
    let mut archives: Vec<_> = WalkDir::new(search_root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy();
            if !is_image_archive_name(&name) {
                return false;
            }
            !filter_flat
                || aliases.iter().any(|alias| {
                    entry
                        .path()
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains(alias)
                })
        })
        .map(|entry| entry.into_path())
        .collect();
    archives.sort();
    archives
}

fn copy_np4_template(source: &FsPath, dest: &FsPath, arch: &str) -> anyhow::Result<()> {
    if dest.exists() {
        anyhow::bail!("目标目录 {} 已存在", dest.display());
    }
    let compose = source.join(format!("docker-compose-{arch}.yaml"));
    if !compose.is_file() {
        anyhow::bail!("未找到 {}", compose.display());
    }

    std::fs::create_dir_all(dest)?;
    for entry in WalkDir::new(source).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        let rel = path.strip_prefix(source)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let first = rel.components().next().and_then(|c| c.as_os_str().to_str());
        if matches!(first, Some(".git") | Some("base-images") | Some("soft"))
            || matches!(
                rel.file_name().and_then(|name| name.to_str()),
                Some("docker-compose-x86.yaml") | Some("docker-compose-arm.yaml")
            )
        {
            continue;
        }
        let target = dest.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(path, target)?;
        }
    }
    std::fs::copy(compose, dest.join("docker-compose.yaml"))?;
    Ok(())
}

fn np4_jar_files(image_package: &FsPath) -> anyhow::Result<Vec<PathBuf>> {
    let source = image_package
        .join("np4-jars")
        .join("latest")
        .join("all")
        .join("all");
    if !source.is_dir() {
        anyhow::bail!("未找到 NP4 JAR 包目录 {}", source.display());
    }
    let mut jars: Vec<_> = std::fs::read_dir(&source)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "jar"))
        .collect();
    jars.sort();
    if jars.is_empty() {
        anyhow::bail!("NP4 JAR 包目录 {} 中没有 .jar 文件", source.display());
    }
    Ok(jars)
}

fn initialize_np4_project(dest: &FsPath, jars: &[PathBuf], master_ip: &str) -> anyhow::Result<()> {
    let example = dest.join(".env.example");
    let env = dest.join(".env");
    if !example.is_file() {
        anyhow::bail!("项目模板未包含 {}", example.display());
    }
    std::fs::copy(&example, &env)?;
    let content = std::fs::read_to_string(&env)?;
    let mut found_host = false;
    let mut lines: Vec<String> = content
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("HOST=") {
                found_host = true;
                format!("HOST={master_ip}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !found_host {
        lines.push(format!("HOST={master_ip}"));
    }
    std::fs::write(&env, format!("{}\n", lines.join("\n")))?;

    let target = dest.join("jars");
    std::fs::create_dir_all(&target)?;
    for jar in jars {
        let name = jar
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("无效 JAR 文件名: {}", jar.display()))?;
        std::fs::copy(jar, target.join(name))?;
    }
    Ok(())
}

fn create_np4_data_dirs(dest: &FsPath) -> anyhow::Result<Vec<PathBuf>> {
    let env = dest.join(".env");
    let content = std::fs::read_to_string(&env)?;
    let data_path = content
        .lines()
        .filter_map(|line| line.trim().strip_prefix("DATA_PATH="))
        .map(|value| value.trim().trim_matches(['\'', '"']))
        .find(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{} 中未配置 DATA_PATH", env.display()))?;
    let data_path = PathBuf::from(data_path);
    if !data_path.is_absolute() {
        anyhow::bail!("DATA_PATH 必须是绝对路径：{}", data_path.display());
    }

    let dirs: Vec<_> = ["np4", "np4-resource-images", "algo_tools"]
        .into_iter()
        .map(|name| data_path.join(name))
        .collect();
    for dir in &dirs {
        std::fs::create_dir_all(dir)?;
    }
    Ok(dirs)
}

async fn np4_deploy_status(
    State(state): State<AppState>,
) -> Result<Json<Np4DeployStatus>, AppError> {
    let project_dir = PathBuf::from(NP4_PROJECT_DIR);
    let is_master = state.cluster.role == Role::Master;
    let registered = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::list_projects(&conn)?
            .iter()
            .any(|project| project.directory == NP4_PROJECT_DIR)
    };
    let (arch, aliases) = match np4_arch() {
        Ok(value) => value,
        Err(err) => {
            return Ok(Json(Np4DeployStatus {
                is_master,
                project_dir: NP4_PROJECT_DIR.into(),
                exists: project_dir.exists(),
                registered,
                arch: None,
                template_dir: None,
                image_count: 0,
                message: Some(err.to_string()),
            }));
        }
    };
    let template = np4_template_dir(&state.paths.exe_dir);
    let image_package = np4_image_package_dir(&state.paths.exe_dir);
    let archives = image_package
        .as_deref()
        .map(|dir| np4_image_archives(dir, aliases))
        .unwrap_or_default();
    let message = if !is_master {
        Some("仅主节点提供 NP4 项目部署。".into())
    } else if project_dir.exists() {
        None
    } else if template.is_none() {
        Some("本地软件仓库未找到 cangling-np4 模板；请先通过维护中心同步。".into())
    } else if archives.is_empty() {
        Some(format!("NP4 基础镜像包中未找到 {arch} 架构的 base-images 镜像包。"))
    } else {
        None
    };
    Ok(Json(Np4DeployStatus {
        is_master,
        project_dir: NP4_PROJECT_DIR.into(),
        exists: project_dir.exists(),
        registered,
        arch: Some(arch.into()),
        template_dir: template.map(|dir| dir.display().to_string()),
        image_count: archives.len(),
        message,
    }))
}

async fn deploy_np4(
    State(state): State<AppState>,
    Json(body): Json<DeployNp4Body>,
) -> Result<Json<Project>, AppError> {
    if state.cluster.role != Role::Master {
        return Err(AppError::bad("仅主节点可以部署 NP4 项目"));
    }
    let (arch, aliases) = np4_arch()?;
    let dest = PathBuf::from(NP4_PROJECT_DIR);
    if dest.exists() {
        return Err(AppError::Conflict(format!("{} 已存在，已取消部署", dest.display())));
    }
    let template = np4_template_dir(&state.paths.exe_dir).ok_or_else(|| {
        AppError::bad("本地软件仓库未找到 cangling-np4 模板；请先通过维护中心同步")
    })?;
    let image_package = np4_image_package_dir(&state.paths.exe_dir).ok_or_else(|| {
        AppError::bad("本地软件仓库未找到 NP4 基础镜像包；请先通过维护中心同步 np4")
    })?;
    let archives = np4_image_archives(&image_package, aliases);
    if archives.is_empty() {
        return Err(AppError::bad(format!("NP4 基础镜像包中未找到 {arch} 架构的 base-images 镜像包")));
    }
    let jars = np4_jar_files(&image_package).map_err(AppError::from)?;
    let master_ip = hostinfo::primary_ip();
    if master_ip.is_empty() {
        return Err(AppError::bad("无法识别 Master 主机 IP，无法写入 NP4 .env"));
    }

    job_set(
        &state,
        body.job_id.as_deref(),
        "np4-template",
        "正在复制 NP4 项目模板…",
        0,
        archives.len() as u64 + 4,
    );
    let source = template.clone();
    let target = dest.clone();
    let arch = arch.to_string();
    if let Err(err) = tokio::task::spawn_blocking(move || copy_np4_template(&source, &target, &arch))
        .await
        .map_err(|e| AppError::internal(e.to_string()))
        .and_then(|result| result.map_err(AppError::from))
    {
        job_err(&state, body.job_id.as_deref(), &err.to_string());
        return Err(err);
    }

    job_set(
        &state,
        body.job_id.as_deref(),
        "np4-config",
        "正在生成 .env 并复制 NP4 JAR 包…",
        1,
        archives.len() as u64 + 4,
    );
    let target = dest.clone();
    if let Err(err) = tokio::task::spawn_blocking(move || initialize_np4_project(&target, &jars, &master_ip))
        .await
        .map_err(|e| AppError::internal(e.to_string()))
        .and_then(|result| result.map_err(AppError::from))
    {
        job_err(&state, body.job_id.as_deref(), &err.to_string());
        return Err(err);
    }

    for (index, archive) in archives.iter().enumerate() {
        job_set(
            &state,
            body.job_id.as_deref(),
            "np4-images",
            &format!("正在导入基础镜像：{}", archive.file_name().unwrap_or_default().to_string_lossy()),
            index as u64 + 2,
            archives.len() as u64 + 4,
        );
        if let Err(err) = state.docker.load_archive(archive).await {
            let err = AppError::bad(format!("导入 {} 失败：{err}", archive.display()));
            job_err(&state, body.job_id.as_deref(), &err.to_string());
            return Err(err);
        }
    }

    job_set(
        &state,
        body.job_id.as_deref(),
        "np4-directories",
        "正在创建 NP4 数据目录…",
        archives.len() as u64 + 2,
        archives.len() as u64 + 4,
    );
    let target = dest.clone();
    if let Err(err) = tokio::task::spawn_blocking(move || create_np4_data_dirs(&target))
        .await
        .map_err(|e| AppError::internal(e.to_string()))
        .and_then(|result| result.map_err(AppError::from))
    {
        job_err(&state, body.job_id.as_deref(), &err.to_string());
        return Err(err);
    }

    job_set(
        &state,
        body.job_id.as_deref(),
        "np4-project",
        "正在登记 NP4 项目并建立基线快照…",
        archives.len() as u64 + 3,
        archives.len() as u64 + 4,
    );
    let project = create_project(
        State(state.clone()),
        Json(CreateProject {
            name: "农业普查（NP4）".into(),
            description: Some("由本地软件仓库 cangling-np4 部署".into()),
            directory: NP4_PROJECT_DIR.into(),
            job_id: body.job_id.clone(),
            stop_compose: false,
        }),
    )
    .await?;

    job_set(
        &state,
        body.job_id.as_deref(),
        "np4-start",
        "数据目录已就绪，正在启动 NP4 应用…",
        archives.len() as u64 + 4,
        archives.len() as u64 + 4,
    );
    if let Err(err) = state.docker.compose_up(&dest).await {
        let err = AppError::bad(format!("NP4 项目已部署，但启动失败：{err}"));
        job_err(&state, body.job_id.as_deref(), &err.to_string());
        return Err(err);
    }
    job_ok(&state, body.job_id.as_deref(), "NP4 项目已部署并启动");
    Ok(project)
}

fn harbor_template_archive(exe_dir: &FsPath) -> Option<PathBuf> {
    let archive = exe_dir.join("repo").join(HARBOR_TEMPLATE_REL);
    archive.is_file().then_some(archive)
}

fn is_harbor_project(project: &Project) -> bool {
    project.directory == HARBOR_PROJECT_DIR
        || project.name.eq_ignore_ascii_case("harbor")
        || project.name.eq_ignore_ascii_case("cangling-zot")
}

fn harbor_arch() -> Result<(&'static str, &'static str), AppError> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(("amd64", "zot-image-amd64.tar.gz")),
        "aarch64" => Ok(("arm64", "zot-image-arm64.tar.gz")),
        arch => Err(AppError::bad(format!(
            "Harbor 部署暂不支持本机架构 {arch}"
        ))),
    }
}

fn harbor_image_archive(project_dir: &FsPath, filename: &str) -> Option<PathBuf> {
    let archive = project_dir.join(filename);
    archive.is_file().then_some(archive)
}

fn initialize_harbor_project(project_dir: &FsPath) -> anyhow::Result<()> {
    let init = project_dir.join("init-auth.sh");
    if !init.is_file() {
        anyhow::bail!("项目模板未包含 {}", init.display());
    }
    std::fs::create_dir_all(project_dir.join("data"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["init-auth.sh", "pack.sh", "bundle.sh"] {
            let path = project_dir.join(name);
            if path.is_file() {
                let mut permissions = std::fs::metadata(&path)?.permissions();
                permissions.set_mode(permissions.mode() | 0o111);
                std::fs::set_permissions(path, permissions)?;
            }
        }
    }

    let output = std::process::Command::new("bash")
        .arg("init-auth.sh")
        .current_dir(project_dir)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        anyhow::bail!("运行 init-auth.sh 失败：{detail}");
    }
    let htpasswd = project_dir.join("config/htpasswd");
    if !htpasswd.is_file() {
        anyhow::bail!("init-auth.sh 未生成 {}", htpasswd.display());
    }
    Ok(())
}

fn unsafe_archive_path(path: &FsPath) -> bool {
    path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
}

fn unpack_harbor_template(archive: &FsPath, dest: &FsPath) -> anyhow::Result<()> {
    if dest.exists() {
        anyhow::bail!("目标目录 {} 已存在", dest.display());
    }
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("部署目录没有父目录：{}", dest.display()))?;
    std::fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".cangling-zot.deploy-{}", Uuid::new_v4()));
    std::fs::create_dir(&staging)?;

    let result = (|| -> anyhow::Result<()> {
        let file = std::fs::File::open(archive)?;
        let decoder = flate2::read::GzDecoder::new(BufReader::new(file));
        let mut tar = tar::Archive::new(decoder);
        tar.set_overwrite(false);
        tar.set_preserve_permissions(true);
        for entry in tar.entries()? {
            let mut entry = entry?;
            let relative = entry.path()?.into_owned();
            if relative.as_os_str().is_empty() || unsafe_archive_path(&relative) {
                anyhow::bail!("模板包含不安全路径：{}", relative.display());
            }
            if !entry.unpack_in(&staging)? {
                anyhow::bail!("模板文件无法安全解压：{}", relative.display());
            }
        }

        let source = if find_compose_file(&staging).is_some() {
            staging.clone()
        } else {
            let candidates: Vec<_> = WalkDir::new(&staging)
                .min_depth(1)
                .max_depth(2)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_dir())
                .map(|entry| entry.into_path())
                .filter(|dir| find_compose_file(dir).is_some())
                .collect();
            match candidates.as_slice() {
                [only] => only.clone(),
                [] => anyhow::bail!("cangling-zot 模板中未找到 Compose 文件"),
                _ => anyhow::bail!("cangling-zot 模板中包含多个 Compose 项目，无法确定项目根目录"),
            }
        };

        std::fs::rename(&source, dest)?;
        if source != staging {
            std::fs::remove_dir_all(&staging)?;
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_dir_all(dest);
    }
    result
}

async fn harbor_deploy_status(
    State(state): State<AppState>,
) -> Result<Json<HarborDeployStatus>, AppError> {
    let can_deploy = state.cluster.role != Role::Worker;
    let project_dir = PathBuf::from(HARBOR_PROJECT_DIR);
    let registered = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::list_projects(&conn)?.iter().any(is_harbor_project)
    };
    let template = harbor_template_archive(&state.paths.exe_dir);
    let message = if !can_deploy {
        Some("工作节点不提供 Harbor 项目部署，请在主节点操作。".into())
    } else if registered {
        None
    } else if project_dir.exists() {
        Some(format!(
            "{} 已存在；为避免覆盖，未自动部署。可通过“新建项目”登记该目录。",
            project_dir.display()
        ))
    } else if template.is_none() {
        Some(format!(
            "本地软件仓库未找到 {HARBOR_TEMPLATE_REL}；请先通过维护中心同步 images 软件集。"
        ))
    } else {
        None
    };
    Ok(Json(HarborDeployStatus {
        can_deploy,
        project_dir: HARBOR_PROJECT_DIR.into(),
        exists: project_dir.exists(),
        registered,
        template_archive: template.map(|path| path.display().to_string()),
        message,
    }))
}

async fn deploy_harbor(
    State(state): State<AppState>,
    Json(body): Json<DeployHarborBody>,
) -> Result<Json<Project>, AppError> {
    if state.cluster.role == Role::Worker {
        return Err(AppError::bad("工作节点不能部署 Harbor 项目，请在主节点操作"));
    }
    let (arch, image_filename) = harbor_arch()?;
    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        if db::list_projects(&conn)?.iter().any(is_harbor_project) {
            return Err(AppError::Conflict("Harbor 项目已经存在".into()));
        }
    }
    let dest = PathBuf::from(HARBOR_PROJECT_DIR);
    if dest.exists() {
        return Err(AppError::Conflict(format!(
            "{} 已存在，已取消部署",
            dest.display()
        )));
    }
    let archive = harbor_template_archive(&state.paths.exe_dir).ok_or_else(|| {
        AppError::bad(format!(
            "本地软件仓库未找到 {HARBOR_TEMPLATE_REL}；请先通过维护中心同步 images 软件集"
        ))
    })?;

    job_set(
        &state,
        body.job_id.as_deref(),
        "harbor-template",
        "正在解压 cangling-zot 项目模板…",
        0,
        5,
    );
    let source = archive.clone();
    let target = dest.clone();
    if let Err(err) = tokio::task::spawn_blocking(move || unpack_harbor_template(&source, &target))
        .await
        .map_err(|error| AppError::internal(error.to_string()))
        .and_then(|result| result.map_err(AppError::from))
    {
        job_err(&state, body.job_id.as_deref(), &err.to_string());
        let _ = std::fs::remove_dir_all(&dest);
        return Err(err);
    }

    job_set(
        &state,
        body.job_id.as_deref(),
        "harbor-init",
        "正在初始化 Zot 证书、帐号和数据目录…",
        1,
        5,
    );
    let target = dest.clone();
    if let Err(err) = tokio::task::spawn_blocking(move || initialize_harbor_project(&target))
        .await
        .map_err(|error| AppError::internal(error.to_string()))
        .and_then(|result| result.map_err(AppError::from))
    {
        job_err(&state, body.job_id.as_deref(), &err.to_string());
        let _ = std::fs::remove_dir_all(&dest);
        return Err(err);
    }

    let Some(image) = harbor_image_archive(&dest, image_filename) else {
        let err = AppError::bad(format!(
            "cangling-zot 模板中未找到 {arch} 架构镜像 {image_filename}"
        ));
        job_err(&state, body.job_id.as_deref(), &err.to_string());
        let _ = std::fs::remove_dir_all(&dest);
        return Err(err);
    };
    job_set(
        &state,
        body.job_id.as_deref(),
        "harbor-images",
        &format!("正在导入 {arch} 镜像：{image_filename}"),
        2,
        5,
    );
    if let Err(error) = state.docker.load_archive(&image).await {
        let err = AppError::bad(format!("导入 {} 失败：{error}", image.display()));
        job_err(&state, body.job_id.as_deref(), &err.to_string());
        let _ = std::fs::remove_dir_all(&dest);
        return Err(err);
    }

    job_set(
        &state,
        body.job_id.as_deref(),
        "harbor-project",
        "正在登记 Harbor 项目并建立基线快照…",
        3,
        5,
    );
    let project = create_project(
        State(state.clone()),
        Json(CreateProject {
            name: "cangling-zot".into(),
            description: Some("苍灵统一镜像仓库（Zot），由本地软件仓库模板部署".into()),
            directory: HARBOR_PROJECT_DIR.into(),
            job_id: body.job_id.clone(),
            stop_compose: false,
        }),
    )
    .await?;

    job_set(
        &state,
        body.job_id.as_deref(),
        "harbor-start",
        "基线已建立，正在启动 Harbor…",
        4,
        5,
    );
    if let Err(err) = state.docker.compose_up(&dest).await {
        let err = AppError::bad(format!("Harbor 项目已部署，但启动失败：{err}"));
        job_err(&state, body.job_id.as_deref(), &err.to_string());
        return Err(err);
    }
    job_ok(&state, body.job_id.as_deref(), "Harbor 项目已部署并启动");
    Ok(project)
}

fn ensure_yaml_mapping<'a>(
    mapping: &'a mut serde_yaml::Mapping,
    key: &str,
) -> &'a mut serde_yaml::Mapping {
    let value = mapping
        .entry(serde_yaml::Value::String(key.to_string()))
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    if !value.is_mapping() {
        *value = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    value.as_mapping_mut().expect("value was set to mapping")
}

fn update_zot_hosts(content: &str, master_ip: &str) -> String {
    let mut lines: Vec<&str> = content
        .lines()
        .filter(|line| {
            let active = line.split('#').next().unwrap_or_default();
            !active.split_whitespace().skip(1).any(|host| host == "hub.cangling.cn")
        })
        .collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let mut result = lines.join("\n");
    if !result.is_empty() {
        result.push('\n');
    }
    result.push_str(&format!("{master_ip}\thub.cangling.cn\n"));
    result
}

fn update_zot_registries(content: &str, ca_path: &FsPath) -> anyhow::Result<String> {
    let mut root = if content.trim().is_empty() {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
    } else {
        serde_yaml::from_str::<serde_yaml::Value>(content)?
    };
    if !root.is_mapping() {
        anyhow::bail!("registries.yaml 根节点必须是 YAML mapping");
    }
    let root = root.as_mapping_mut().expect("root is mapping");
    let hub = serde_yaml::Value::String("hub.cangling.cn".into());

    let mirrors = ensure_yaml_mapping(root, "mirrors");
    let mirror = mirrors
        .entry(hub.clone())
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    if !mirror.is_mapping() {
        *mirror = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    mirror.as_mapping_mut().expect("mirror is mapping").insert(
        serde_yaml::Value::String("endpoint".into()),
        serde_yaml::Value::Sequence(vec![serde_yaml::Value::String(
            "https://hub.cangling.cn".into(),
        )]),
    );

    let configs = ensure_yaml_mapping(root, "configs");
    let config = configs
        .entry(hub)
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    if !config.is_mapping() {
        *config = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let tls = ensure_yaml_mapping(config.as_mapping_mut().expect("config is mapping"), "tls");
    tls.insert(
        serde_yaml::Value::String("ca_file".into()),
        serde_yaml::Value::String(ca_path.display().to_string()),
    );
    Ok(serde_yaml::to_string(&root)?)
}

fn write_system_text(path: &FsPath, content: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} 没有父目录", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".cangling-update-{}.tmp", Uuid::new_v4()));
    std::fs::write(&temp, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o644))?;
    }
    std::fs::rename(temp, path)?;
    Ok(())
}

fn configure_zot_environment_files(
    hosts_path: &FsPath,
    registries_path: &FsPath,
    ca_path: &FsPath,
    master_ip: &str,
    ca_pem: &str,
) -> anyhow::Result<()> {
    master_ip
        .parse::<std::net::IpAddr>()
        .map_err(|_| anyhow::anyhow!("Master IP 无效：{master_ip}"))?;
    if ca_pem.trim().is_empty() {
        anyhow::bail!("Zot CA 证书为空");
    }
    let hosts = std::fs::read_to_string(hosts_path).unwrap_or_default();
    let registries = std::fs::read_to_string(registries_path).unwrap_or_default();
    write_system_text(hosts_path, &update_zot_hosts(&hosts, master_ip))?;
    write_system_text(ca_path, ca_pem)?;
    write_system_text(
        registries_path,
        &update_zot_registries(&registries, ca_path)?,
    )?;
    Ok(())
}

fn restart_k3s_for_role(role: Role) -> anyhow::Result<String> {
    let candidates: &[&str] = if role == Role::Worker {
        &["k3s-agent"]
    } else {
        &["k3s", "k3s-agent"]
    };
    for service in candidates {
        let known = std::path::Path::new(&format!("/etc/systemd/system/{service}.service")).exists()
            || std::process::Command::new("systemctl")
                .args(["is-active", "--quiet", service])
                .status()
                .is_ok_and(|status| status.success());
        if !known {
            continue;
        }
        let output = std::process::Command::new("systemctl")
            .args(["restart", service])
            .output()?;
        if !output.status.success() {
            anyhow::bail!(
                "重启 {service} 失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        return Ok(format!("已更新 hosts/registry 配置并重启 {service}"));
    }
    anyhow::bail!("未找到 k3s 或 k3s-agent 服务")
}

fn apply_zot_environment_local(
    role: Role,
    master_ip: &str,
    ca_pem: &str,
) -> anyhow::Result<String> {
    configure_zot_environment_files(
        FsPath::new("/etc/hosts"),
        FsPath::new("/etc/rancher/k3s/registries.yaml"),
        FsPath::new("/etc/rancher/k3s/cangling-ca.crt"),
        master_ip,
        ca_pem,
    )?;
    restart_k3s_for_role(role)
}

pub(crate) async fn apply_zot_environment_on_node(
    State(state): State<AppState>,
    Json(body): Json<ZotEnvironmentRequest>,
) -> Result<Json<ZotNodeEnvironmentResult>, AppError> {
    let node = hostinfo::hostname();
    let address = hostinfo::primary_ip();
    let role = state.cluster.role;
    let master_ip = body.master_ip;
    let ca_pem = body.ca_pem;
    let result = tokio::task::spawn_blocking(move || {
        apply_zot_environment_local(role, &master_ip, &ca_pem)
    })
    .await
    .map_err(|error| AppError::internal(error.to_string()))?;
    match result {
        Ok(message) => Ok(Json(ZotNodeEnvironmentResult {
            node,
            address,
            ok: true,
            message,
        })),
        Err(error) => Err(AppError::internal(error.to_string())),
    }
}

async fn check_zot_environment(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CheckZotEnvironmentBody>,
) -> Result<Json<ZotEnvironmentResult>, AppError> {
    if state.cluster.role == Role::Worker {
        return Err(AppError::bad("请在主节点执行 Zot 集群环境检查"));
    }
    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };
    if !is_harbor_project(&project) {
        return Err(AppError::bad("环境检查只适用于 cangling-zot 项目"));
    }
    let master_ip = hostinfo::primary_ip();
    master_ip
        .parse::<std::net::IpAddr>()
        .map_err(|_| AppError::bad("无法识别 Master IP"))?;
    let ca_path = PathBuf::from(&project.directory).join("ca/cangling-ca.crt");
    let ca_pem = std::fs::read_to_string(&ca_path)
        .map_err(|error| AppError::bad(format!("无法读取 {}：{error}", ca_path.display())))?;
    let workers = if state.cluster.role == Role::Master {
        crate::cluster::server::workers(&state)?
    } else {
        Vec::new()
    };
    let node_total = workers.len() as u64 + 1;
    let total = node_total + 1;
    let mut nodes = Vec::new();

    job_set(
        &state,
        body.job_id.as_deref(),
        "zot-environment",
        "正在配置主节点 Zot 镜像仓库…",
        0,
        total,
    );
    let local_role = state.cluster.role;
    let local_ip = master_ip.clone();
    let local_ca = ca_pem.clone();
    let local = tokio::task::spawn_blocking(move || {
        apply_zot_environment_local(local_role, &local_ip, &local_ca)
    })
    .await
    .map_err(|error| AppError::internal(error.to_string()))?;
    nodes.push(ZotNodeEnvironmentResult {
        node: hostinfo::hostname(),
        address: master_ip.clone(),
        ok: local.is_ok(),
        message: local.unwrap_or_else(|error| error.to_string()),
    });

    let token = state.cluster.token.clone().unwrap_or_default();
    for (index, (name, address, online)) in workers.into_iter().enumerate() {
        job_set(
            &state,
            body.job_id.as_deref(),
            "zot-environment",
            &format!("正在配置节点 {name}…"),
            index as u64 + 1,
            total,
        );
        if !online {
            nodes.push(ZotNodeEnvironmentResult {
                node: name,
                address,
                ok: false,
                message: "节点离线，未能配置".into(),
            });
            continue;
        }
        let url = format!("http://{address}/api/cluster/zot/environment");
        let request = serde_json::to_value(ZotEnvironmentRequest {
            master_ip: master_ip.clone(),
            ca_pem: ca_pem.clone(),
        })
        .map_err(|error| AppError::internal(error.to_string()))?;
        let result = crate::cluster::http::post_json(&url, &token, &request).await;
        match result {
            Ok((status, value)) if status.is_success() => {
                match serde_json::from_value::<ZotNodeEnvironmentResult>(value) {
                    Ok(result) => nodes.push(result),
                    Err(error) => nodes.push(ZotNodeEnvironmentResult {
                        node: name,
                        address,
                        ok: false,
                        message: format!("解析节点响应失败：{error}"),
                    }),
                }
            }
            Ok((status, value)) => nodes.push(ZotNodeEnvironmentResult {
                node: name,
                address,
                ok: false,
                message: format!("节点返回 {status}: {value}"),
            }),
            Err(error) => nodes.push(ZotNodeEnvironmentResult {
                node: name,
                address,
                ok: false,
                message: error.to_string(),
            }),
        }
    }
    let environment_ok = nodes.iter().all(|node| node.ok);
    let (image_test_ok, image_test_message) = if environment_ok {
        job_set(
            &state,
            body.job_id.as_deref(),
            "zot-test",
            "正在推送 hello-world 并验证所有 k3s 节点拉取…",
            node_total,
            total,
        );
        let script = PathBuf::from(&project.directory).join("test-k3s.sh");
        let script_display = script.display().to_string();
        let test = tokio::task::spawn_blocking(move || {
            std::process::Command::new("bash")
                .arg(&script)
                .current_dir(script.parent().unwrap_or_else(|| FsPath::new("/")))
                .output()
        })
        .await
        .map_err(|error| AppError::internal(error.to_string()))?;
        match test {
            Ok(output) if output.status.success() => (
                true,
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .last()
                    .unwrap_or("hello-world 集群测试通过")
                    .to_string(),
            ),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let detail = if stderr.is_empty() { stdout } else { stderr };
                (false, format!("hello-world 集群测试失败：{detail}"))
            }
            Err(error) => (false, format!("无法执行 {script_display}：{error}")),
        }
    } else {
        (false, "部分节点环境配置失败，已跳过 hello-world 集群测试".into())
    };
    let ok = environment_ok && image_test_ok;
    let message = if ok {
        "Zot 环境配置及 hello-world 全节点拉取测试完成"
    } else if environment_ok {
        "Zot 环境配置完成，但 hello-world 全节点拉取测试失败"
    } else {
        "部分节点 Zot 镜像仓库环境配置失败"
    };
    if ok {
        job_ok(&state, body.job_id.as_deref(), message);
    } else {
        job_err(&state, body.job_id.as_deref(), message);
    }
    Ok(Json(ZotEnvironmentResult {
        ok,
        master_ip,
        nodes,
        image_test_ok,
        image_test_message,
    }))
}

async fn get_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Project>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    db::get_project(&conn, &id)?
        .map(Json)
        .ok_or_else(|| AppError::not_found("项目不存在"))
}

const MAX_PROJECT_TEXT_SIZE: u64 = 2 * 1024 * 1024;

fn project_file_is_editable(path: &FsPath) -> bool {
    let name = path.file_name().and_then(|v| v.to_str()).unwrap_or_default().to_ascii_lowercase();
    if name == ".env" || name.starts_with(".env.") {
        return true;
    }
    matches!(
        FsPath::new(&name).extension().and_then(|v| v.to_str()),
        Some("txt" | "yaml" | "yml" | "ini" | "conf" | "cfg" | "json" | "xml" | "properties" | "toml" | "md" | "sh" | "service" | "log")
    )
}

fn clean_project_relative(path: &str) -> Result<PathBuf, AppError> {
    let path = FsPath::new(path.trim());
    if path.is_absolute()
        || path.components().any(|part| matches!(part, std::path::Component::ParentDir | std::path::Component::Prefix(_)))
    {
        return Err(AppError::bad("项目文件路径无效"));
    }
    Ok(path.to_path_buf())
}

fn project_root(state: &AppState, id: &str) -> Result<PathBuf, AppError> {
    let directory = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, id)?
            .ok_or_else(|| AppError::not_found("项目不存在"))?
            .directory
    };
    PathBuf::from(&directory)
        .canonicalize()
        .map_err(|e| AppError::internal(format!("无法访问项目目录 {directory}：{e}")))
}

fn project_file_path(root: &FsPath, relative: &str) -> Result<(PathBuf, PathBuf), AppError> {
    let relative = clean_project_relative(relative)?;
    let joined = root.join(&relative);
    let resolved = joined
        .canonicalize()
        .map_err(|e| AppError::not_found(format!("无法访问 {}：{e}", joined.display())))?;
    if !resolved.starts_with(root) {
        return Err(AppError::bad("目标路径超出项目目录"));
    }
    Ok((relative, resolved))
}

async fn project_files_list(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ProjectFileQuery>,
) -> Result<Json<Vec<ProjectFileEntry>>, AppError> {
    let root = project_root(&state, &id)?;
    let (relative, directory) = project_file_path(&root, &query.path)?;
    if !directory.is_dir() {
        return Err(AppError::bad("目标不是目录"));
    }
    let rd = std::fs::read_dir(&directory)
        .map_err(|e| AppError::internal(format!("无法读取目录 {}：{e}", directory.display())))?;
    let mut entries = Vec::new();
    for entry in rd.flatten() {
        let Ok(resolved) = entry.path().canonicalize() else { continue };
        if !resolved.starts_with(&root) { continue; }
        let Ok(metadata) = resolved.metadata() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = relative.join(&name);
        entries.push(ProjectFileEntry {
            name,
            path: rel.to_string_lossy().replace('\\', "/"),
            is_dir: metadata.is_dir(),
            size: if metadata.is_file() { metadata.len() } else { 0 },
            editable: metadata.is_file() && project_file_is_editable(&rel),
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(Json(entries))
}

async fn project_file_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ProjectFileQuery>,
) -> Result<Json<ProjectFileView>, AppError> {
    let root = project_root(&state, &id)?;
    let (relative, file) = project_file_path(&root, &query.path)?;
    let metadata = file.metadata().map_err(|e| AppError::internal(format!("无法读取文件信息：{e}")))?;
    if !metadata.is_file() { return Err(AppError::bad("目标不是文件")); }
    if metadata.len() > MAX_PROJECT_TEXT_SIZE {
        return Err(AppError::bad(format!("文件超过 {} MB，无法预览", MAX_PROJECT_TEXT_SIZE / 1024 / 1024)));
    }
    let content = std::fs::read_to_string(&file)
        .map_err(|e| AppError::bad(format!("文件不是可预览的 UTF-8 文本：{e}")))?;
    Ok(Json(ProjectFileView {
        path: relative.to_string_lossy().replace('\\', "/"),
        size: metadata.len(),
        editable: project_file_is_editable(&relative),
        content,
    }))
}

async fn project_file_put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SaveProjectFileBody>,
) -> Result<Json<ProjectFileView>, AppError> {
    if body.content.len() as u64 > MAX_PROJECT_TEXT_SIZE {
        return Err(AppError::bad(format!("文件超过 {} MB，无法保存", MAX_PROJECT_TEXT_SIZE / 1024 / 1024)));
    }
    let root = project_root(&state, &id)?;
    let (relative, file) = project_file_path(&root, &body.path)?;
    if !project_file_is_editable(&relative) { return Err(AppError::bad("该文件类型不允许编辑")); }
    if !file.is_file() { return Err(AppError::bad("目标不是文件")); }
    write_text_atomic(&file, &body.content).map_err(AppError::from)?;
    let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(body.content.len() as u64);
    Ok(Json(ProjectFileView {
        path: relative.to_string_lossy().replace('\\', "/"),
        size,
        content: body.content,
        editable: true,
    }))
}

async fn create_project(
    State(state): State<AppState>,
    Json(body): Json<CreateProject>,
) -> Result<Json<Project>, AppError> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::bad("项目名称不能为空"));
    }
    let inspected = inspect_directory(&body.directory)?;
    if !inspected.ok {
        return Err(AppError::bad(
            inspected
                .warning
                .unwrap_or_else(|| "该目录不是 Docker Compose 应用".into()),
        ));
    }

    let id = Uuid::new_v4().to_string();
    let now = db::now_rfc3339();
    let project = Project {
        id: id.clone(),
        name,
        description: body.description.unwrap_or_default(),
        directory: inspected.directory.clone(),
        created_at: now.clone(),
        updated_at: now.clone(),
        current_version_no: Some(1),
        current_version_id: None,
        version_count: 1,
    };

    let version_id = Uuid::new_v4().to_string();
    let tree = state.paths.version_tree(&id, &version_id);
    let live = PathBuf::from(&inspected.directory);
    let stopped =
        compose_down_for_backup(&state, &live, body.job_id.as_deref(), body.stop_compose).await?;
    let tree_clone = tree.clone();
    let jobs = state.jobs.clone();
    let job_id = body.job_id.clone();
    let live_for_snap = live.clone();
    job_set(
        &state,
        job_id.as_deref(),
        "snapshot",
        "正在建立基线快照…",
        0,
        0,
    );
    if let Err(err) = tokio::task::spawn_blocking(move || {
        snapshot_blocking(live_for_snap, tree_clone, jobs, job_id)
    })
    .await
    .map_err(|e| AppError::internal(e.to_string()))
    .and_then(|r| r.map_err(AppError::from))
    {
        job_err(&state, body.job_id.as_deref(), &err.to_string());
        let root = state.paths.project_backup_root(&id);
        let _ = remove_dir_if_exists(&root);
        if stopped {
            compose_up_best_effort(
                &state,
                &live,
                body.job_id.as_deref(),
                "备份失败，正在重新启动 Compose…",
            )
            .await;
        }
        return Err(err);
    }

    let db_err = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        if let Err(err) = db::insert_project(&conn, &project) {
            Some(err)
        } else {
            let version = Version {
                id: version_id.clone(),
                project_id: id.clone(),
                version_no: 1,
                label: "v1".into(),
                note: "基线快照".into(),
                backup_path: tree.display().to_string(),
                images: Vec::new(),
                jars: Vec::new(),
                is_current: true,
                kind: "baseline".into(),
                created_at: now,
                app_bytes: 0,
                backup_bytes: 0,
                repo_bytes: 0,
            };
            if let Err(err) = db::insert_version(&conn, &version) {
                let _ = db::delete_project(&conn, &id);
                Some(err)
            } else {
                None
            }
        }
    };
    if let Some(err) = db_err {
        let root = state.paths.project_backup_root(&id);
        let _ = remove_dir_if_exists(&root);
        if stopped {
            compose_up_best_effort(
                &state,
                &live,
                body.job_id.as_deref(),
                "写入失败，正在重新启动 Compose…",
            )
            .await;
        }
        let msg = err.to_string();
        if msg.contains("UNIQUE") {
            job_err(&state, body.job_id.as_deref(), "已存在同名项目");
            return Err(AppError::Conflict("已存在同名项目".into()));
        }
        job_err(&state, body.job_id.as_deref(), &msg);
        return Err(err.into());
    }
    if stopped {
        compose_up_best_effort(
            &state,
            &live,
            body.job_id.as_deref(),
            "基线已建立，正在重新启动 Compose…",
        )
        .await;
    }
    job_ok(&state, body.job_id.as_deref(), "基线快照已建立");

    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    db::get_project(&conn, &id)?
        .map(Json)
        .ok_or_else(|| AppError::internal("project missing after insert"))
}

async fn update_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateProject>,
) -> Result<Json<Project>, AppError> {
    let existing = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };

    let name = body
        .name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or(existing.name);
    let description = body.description.unwrap_or(existing.description);
    let directory = if let Some(dir) = body.directory {
        let inspected = inspect_directory(&dir)?;
        if !inspected.ok {
            return Err(AppError::bad(
                inspected
                    .warning
                    .unwrap_or_else(|| "该目录不是 Docker Compose 应用".into()),
            ));
        }
        inspected.directory
    } else {
        existing.directory
    };

    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    db::update_project(
        &conn,
        &id,
        &name,
        &description,
        &directory,
        &db::now_rfc3339(),
    )?;
    db::get_project(&conn, &id)?
        .map(Json)
        .ok_or_else(|| AppError::not_found("项目不存在"))
}

fn orphan_stat(id: String, path: &std::path::Path) -> OrphanBackup {
    let modified = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());
    OrphanBackup {
        id,
        path: path.display().to_string(),
        bytes: dir_size(path),
        modified,
    }
}

fn valid_backup_id(id: &str) -> bool {
    !id.is_empty()
        && !id.contains("..")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn collect_orphans(
    backups_dir: &std::path::Path,
    known_projects: &std::collections::HashSet<String>,
    versions_by_project: &std::collections::HashMap<String, std::collections::HashSet<String>>,
) -> Vec<OrphanBackup> {
    let mut orphans = Vec::new();
    if backups_dir.is_dir() {
        if let Ok(rd) = std::fs::read_dir(backups_dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let id = entry.file_name().to_string_lossy().into_owned();
                if !known_projects.contains(&id) {
                    orphans.push(orphan_stat(id, &path));
                }
            }
        }
    }
    for pid in known_projects {
        let Some(known_versions) = versions_by_project.get(pid) else {
            continue;
        };
        let root = backups_dir.join(pid);
        if !root.is_dir() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "repo.git" || known_versions.contains(&name) {
                continue;
            }
            orphans.push(orphan_stat(format!("{pid}/{name}"), &path));
        }
    }
    orphans.sort_by(|a, b| a.id.cmp(&b.id));
    orphans
}

async fn list_orphans(State(state): State<AppState>) -> Result<Json<Vec<OrphanBackup>>, AppError> {
    let (projects, versions_by_project) = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let projects = db::list_projects(&conn)?;
        let mut versions_by_project = std::collections::HashMap::new();
        for p in &projects {
            let ids: std::collections::HashSet<String> = db::list_versions(&conn, &p.id)?
                .into_iter()
                .map(|v| v.id)
                .collect();
            versions_by_project.insert(p.id.clone(), ids);
        }
        (projects, versions_by_project)
    };
    let known: std::collections::HashSet<String> = projects.into_iter().map(|p| p.id).collect();
    Ok(Json(collect_orphans(
        &state.paths.backups_dir,
        &known,
        &versions_by_project,
    )))
}

async fn delete_orphan(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let id = id.trim_matches('/').to_string();
    if id.contains("..") {
        return Err(AppError::bad("无效的残留备份编号"));
    }
    let root = if let Some((pid, vid)) = id.split_once('/') {
        if !valid_backup_id(pid) || !valid_backup_id(vid) {
            return Err(AppError::bad("无效的残留备份编号"));
        }
        let exists = {
            let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
            db::get_version(&conn, pid, vid)?.is_some()
        };
        if exists {
            return Err(AppError::Conflict(
                "该目录属于已登记版本，请用「删除项目」".into(),
            ));
        }
        state.paths.version_dir(pid, vid)
    } else {
        if !valid_backup_id(&id) {
            return Err(AppError::bad("无效的残留备份编号"));
        }
        let known = {
            let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
            db::get_project(&conn, &id)?.is_some()
        };
        if known {
            return Err(AppError::Conflict(
                "该目录属于已登记项目，请用「删除项目」".into(),
            ));
        }
        state.paths.project_backup_root(&id)
    };
    if !root.exists() {
        return Err(AppError::not_found("没有找到该残留备份"));
    }
    tokio::task::spawn_blocking(move || remove_dir_if_exists(&root))
        .await
        .map_err(|e| AppError::internal(e.to_string()))??;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn delete_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        if !db::delete_project(&conn, &id)? {
            return Err(AppError::not_found("项目不存在"));
        }
    }
    let root = state.paths.project_backup_root(&id);
    let _ = tokio::task::spawn_blocking(move || remove_dir_if_exists(&root)).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn list_versions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<Version>>, AppError> {
    let (project, mut versions) = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let project =
            db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?;
        let versions = db::list_versions(&conn, &id)?;
        (project, versions)
    };
    let live = PathBuf::from(project.directory);
    let backups = state.paths.project_backup_root(&id);
    let version_dirs: Vec<(String, PathBuf)> = versions
        .iter()
        .map(|v| (v.id.clone(), state.paths.version_dir(&id, &v.id)))
        .collect();
    let sized = tokio::task::spawn_blocking(move || {
        let app_bytes = project_dir_size(&live);
        let repo_bytes = dir_size(&backups.join("repo.git"));
        let backup_bytes: std::collections::HashMap<String, u64> = version_dirs
            .into_iter()
            .map(|(vid, path)| (vid, dir_size(&path)))
            .collect();
        (app_bytes, repo_bytes, backup_bytes)
    })
    .await
    .map_err(|e| AppError::internal(e.to_string()))?;
    let (app_bytes, repo_bytes, backup_bytes) = sized;
    for v in &mut versions {
        v.app_bytes = app_bytes;
        v.repo_bytes = repo_bytes;
        v.backup_bytes = backup_bytes.get(&v.id).copied().unwrap_or(0);
    }
    Ok(Json(versions))
}

fn is_np4_project(project: &Project) -> bool {
    project.name.eq_ignore_ascii_case("cangling-np4")
        || FsPath::new(&project.directory)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("cangling-np4"))
}

fn np4_repo_update_files(exe_dir: &FsPath) -> Result<Option<Vec<PathBuf>>, AppError> {
    let dir = exe_dir
        .join("repo")
        .join("np4")
        .join("np4-jars")
        .join("latest")
        .join("all")
        .join("all");
    if !dir.is_dir() {
        return Ok(None);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| AppError::internal(format!("读取 {} 失败：{e}", dir.display())))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        let name = name.to_ascii_lowercase();
                        name.ends_with(".jar") || name.ends_with(".tar.gz")
                    })
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return Ok(None);
    }
    Ok(Some(files))
}

fn files_equal(left: &FsPath, right: &FsPath) -> std::io::Result<bool> {
    if !right.is_file() || std::fs::metadata(left)?.len() != std::fs::metadata(right)?.len() {
        return Ok(false);
    }
    let mut left = std::fs::File::open(left)?;
    let mut right = std::fs::File::open(right)?;
    let mut a = [0_u8; 64 * 1024];
    let mut b = [0_u8; 64 * 1024];
    loop {
        let an = left.read(&mut a)?;
        let bn = right.read(&mut b)?;
        if an != bn || a[..an] != b[..bn] {
            return Ok(false);
        }
        if an == 0 {
            return Ok(true);
        }
    }
}

fn np4_repo_files_unchanged(
    files: &[PathBuf],
    images_dir: &FsPath,
    jars_dir: &FsPath,
) -> std::io::Result<bool> {
    for source in files {
        let Some(name) = source.file_name() else {
            return Ok(false);
        };
        let target = if source.extension().is_some_and(|ext| ext == "jar") {
            jars_dir.join(name)
        } else {
            images_dir.join(name)
        };
        if !files_equal(source, &target)? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Np4RepoFileStamp {
    name: String,
    bytes: u64,
    fingerprint: u64,
}

fn np4_repo_file_stamps(files: &[PathBuf]) -> std::io::Result<Vec<Np4RepoFileStamp>> {
    let mut stamps = Vec::with_capacity(files.len());
    for path in files {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let mut file = std::fs::File::open(path)?;
        let mut bytes = 0_u64;
        let mut fingerprint = 0xcbf29ce484222325_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            bytes += count as u64;
            for byte in &buffer[..count] {
                fingerprint ^= u64::from(*byte);
                fingerprint = fingerprint.wrapping_mul(0x100000001b3);
            }
        }
        stamps.push(Np4RepoFileStamp {
            name,
            bytes,
            fingerprint,
        });
    }
    stamps.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(stamps)
}

fn np4_repo_manifest_path(state: &AppState, project_id: &str) -> Result<PathBuf, AppError> {
    let project_id = safe_filename(project_id).map_err(|e| AppError::bad(e.to_string()))?;
    Ok(state
        .paths
        .config_dir
        .join("np4-repo-updates")
        .join(format!("{project_id}.json")))
}

fn np4_repo_manifest_matches(
    path: &FsPath,
    stamps: &[Np4RepoFileStamp],
) -> Result<Option<bool>, AppError> {
    let value = match std::fs::read(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let saved: Vec<Np4RepoFileStamp> = match serde_json::from_slice(&value) {
        Ok(saved) => saved,
        Err(_) => return Ok(Some(false)),
    };
    Ok(Some(saved == stamps))
}

fn save_np4_repo_manifest(
    path: &FsPath,
    stamps: &[Np4RepoFileStamp],
) -> Result<(), AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::internal("NP4 更新指纹目录无效"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".manifest-{}.tmp", Uuid::new_v4()));
    let value = serde_json::to_vec(stamps).map_err(|e| AppError::internal(e.to_string()))?;
    if let Err(error) = std::fs::write(&temporary, value) {
        return Err(error.into());
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

async fn update_np4_from_repo(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Np4RepoUpdateBody>,
) -> Result<Json<Np4RepoUpdateResult>, AppError> {
    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };
    if !is_np4_project(&project) {
        return Err(AppError::bad("该操作只适用于 cangling-np4 项目"));
    }

    let gate = state.lock_project(&id);
    let _guard = gate
        .try_lock()
        .map_err(|_| AppError::conflict("正在恢复或升级中，请勿重复操作"))?;
    job_set(
        &state,
        body.job_id.as_deref(),
        "check",
        "正在检查 NP4 软件仓库…",
        0,
        0,
    );

    let exe_dir = state.paths.exe_dir.clone();
    let files = tokio::task::spawn_blocking(move || np4_repo_update_files(&exe_dir))
        .await
        .map_err(|e| AppError::internal(e.to_string()))??;
    let Some(files) = files else {
        let message = "软件仓库 np4/np4-jars/latest/all/all 中没有找到 JAR 或 tar.gz 更新包，无需更新";
        job_ok(&state, body.job_id.as_deref(), message);
        return Ok(Json(Np4RepoUpdateResult {
            updated: false,
            message: message.into(),
            version: None,
            files: Vec::new(),
        }));
    };
    let manifest_path = np4_repo_manifest_path(&state, &id)?;
    let files_for_stamps = files.clone();
    let stamps = tokio::task::spawn_blocking(move || np4_repo_file_stamps(&files_for_stamps))
        .await
        .map_err(|e| AppError::internal(e.to_string()))??;
    let saved_manifest = {
        let manifest_path = manifest_path.clone();
        let stamps_for_compare = stamps.clone();
        tokio::task::spawn_blocking(move || {
            np4_repo_manifest_matches(&manifest_path, &stamps_for_compare)
        })
        .await
        .map_err(|e| AppError::internal(e.to_string()))??
    };
    let unchanged = if let Some(matches) = saved_manifest {
        matches
    } else if let Some(version_id) = project.current_version_id.as_deref() {
        let images = state.paths.version_images(&id, version_id);
        let jars = state.paths.version_jars(&id, version_id);
        let compare = files.clone();
        tokio::task::spawn_blocking(move || np4_repo_files_unchanged(&compare, &images, &jars))
            .await
            .map_err(|e| AppError::internal(e.to_string()))??
    } else {
        false
    };
    if unchanged {
        if saved_manifest.is_none() {
            let path = manifest_path.clone();
            tokio::task::spawn_blocking(move || save_np4_repo_manifest(&path, &stamps))
                .await
                .map_err(|e| AppError::internal(e.to_string()))??;
        }
        job_ok(&state, body.job_id.as_deref(), "软件仓库中的 NP4 更新包没有变化");
        return Ok(Json(Np4RepoUpdateResult {
            updated: false,
            message: "软件仓库中的 JAR 和镜像包没有变化，无需更新".into(),
            version: None,
            files: Vec::new(),
        }));
    }

    let available_files: Vec<Np4RepoUpdateFile> = stamps
        .iter()
        .map(|stamp| Np4RepoUpdateFile {
            name: stamp.name.clone(),
            bytes: stamp.bytes,
        })
        .collect();
    let action = body.action.trim();
    if action.is_empty() || action == "check" {
        let message = format!(
            "发现 {} 个更新文件，请选择“导入并发布”或“替换并重启”",
            available_files.len()
        );
        job_ok(&state, body.job_id.as_deref(), &message);
        return Ok(Json(Np4RepoUpdateResult {
            updated: false,
            message,
            version: None,
            files: available_files,
        }));
    }
    if !matches!(action, "publish" | "replace") {
        return Err(AppError::bad("无效的 NP4 更新操作"));
    }

    let tmp = state
        .paths
        .uploads_dir
        .join(format!("np4-repo-update-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&tmp).await?;
    let mut staged = Vec::with_capacity(files.len());
    for (index, source) in files.iter().enumerate() {
        let name = source
            .file_name()
            .ok_or_else(|| AppError::bad("NP4 更新包文件名无效"))?;
        job_set(
            &state,
            body.job_id.as_deref(),
            "stage",
            &format!("正在准备 {}", name.to_string_lossy()),
            index as u64,
            files.len() as u64,
        );
        let target = tmp.join(name);
        if let Err(error) = tokio::fs::copy(source, &target).await {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            return Err(error.into());
        }
        staged.push(target);
    }

    let job_id = body.job_id.clone();
    if action == "publish" {
        let Json(result) = apply_update(
            state,
            project,
            IncomingUpload {
                note: "从 NP4 软件仓库导入并发布".into(),
                restart: body.restart,
                stop_compose: body.stop_compose,
                files: staged,
                tmp,
                job_id,
            },
            "update",
        )
        .await?;
        let path = manifest_path;
        tokio::task::spawn_blocking(move || save_np4_repo_manifest(&path, &stamps))
            .await
            .map_err(|e| AppError::internal(e.to_string()))??;
        return Ok(Json(Np4RepoUpdateResult {
            updated: true,
            message: format!("已将 {} 发布为最新版", result.version.label),
            version: Some(result.version),
            files: Vec::new(),
        }));
    }

    let Json(result) = apply_replace(
        state,
        project,
        IncomingUpload {
            note: String::new(),
            restart: true,
            stop_compose: false,
            files: staged,
            tmp,
            job_id,
        },
    )
    .await?;
    let path = manifest_path;
    tokio::task::spawn_blocking(move || save_np4_repo_manifest(&path, &stamps))
        .await
        .map_err(|e| AppError::internal(e.to_string()))??;
    let file_count = result.loaded.len() + result.jars.len();
    Ok(Json(Np4RepoUpdateResult {
        updated: true,
        message: format!("已替换 {file_count} 个 NP4 更新文件并重启 Compose"),
        version: None,
        files: Vec::new(),
    }))
}

async fn create_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    multipart: Multipart,
) -> Result<Json<UpdateResult>, AppError> {
    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };

    let gate = state.lock_project(&id);
    let _guard = gate.lock().await;

    let upload = receive_upload(&state, multipart).await?;
    apply_update(state, project, upload, "update").await
}

const TEMP_UPLOAD_CHUNK_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct TempUploadQuery {
    upload_id: String,
    name: String,
    total: u64,
    offset: Option<u64>,
}

#[derive(Debug, serde::Serialize)]
struct TempUploadResult {
    name: String,
    path: String,
    uploaded: u64,
    total: u64,
    complete: bool,
}

fn checked_temp_upload_id(value: &str) -> Result<String, AppError> {
    Uuid::parse_str(value)
        .map(|id| id.to_string())
        .map_err(|_| AppError::bad("无效的上传任务 ID"))
}

async fn temp_upload_paths(
    project: &Project,
    query: &TempUploadQuery,
) -> Result<(String, PathBuf, PathBuf, PathBuf), AppError> {
    let upload_id = checked_temp_upload_id(&query.upload_id)?;
    let filename = safe_filename(&query.name).map_err(|e| AppError::bad(e.to_string()))?;
    let project_dir = PathBuf::from(&project.directory);
    let metadata = tokio::fs::metadata(&project_dir)
        .await
        .map_err(|e| AppError::bad(format!("项目目录不可用：{e}")))?;
    if !metadata.is_dir() {
        return Err(AppError::bad("项目目录不是目录"));
    }

    let temp_dir = project_dir.join("temp");
    tokio::fs::create_dir_all(&temp_dir).await?;
    let temp_metadata = tokio::fs::symlink_metadata(&temp_dir).await?;
    if temp_metadata.file_type().is_symlink() || !temp_metadata.is_dir() {
        return Err(AppError::bad("项目 temp 路径必须是普通目录"));
    }

    let staging_dir = temp_dir.join(".uploads");
    tokio::fs::create_dir_all(&staging_dir).await?;
    let staging_metadata = tokio::fs::symlink_metadata(&staging_dir).await?;
    if staging_metadata.file_type().is_symlink() || !staging_metadata.is_dir() {
        return Err(AppError::bad("项目 temp/.uploads 路径必须是普通目录"));
    }

    Ok((
        filename.clone(),
        temp_dir.join(filename),
        staging_dir.join(format!("{upload_id}.part")),
        staging_dir.join(format!("{upload_id}.done")),
    ))
}

fn temp_upload_done_value(filename: &str, total: u64) -> String {
    format!("{filename}\n{total}")
}

async fn replace_temp_upload(part: &FsPath, target: &FsPath) -> Result<(), AppError> {
    match tokio::fs::symlink_metadata(target).await {
        Ok(metadata) if metadata.is_dir() => {
            return Err(AppError::bad("同名目标是目录，无法用上传文件替换"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // part 与 target 位于同一文件系统；Linux rename 会原子替换已有的同名文件。
    tokio::fs::rename(part, target).await?;
    Ok(())
}

async fn temp_upload_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<TempUploadQuery>,
) -> Result<Json<TempUploadResult>, AppError> {
    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };
    let gate = state.lock_project(&id);
    let _guard = gate.lock().await;
    let (filename, target, part, done) = temp_upload_paths(&project, &query).await?;
    let done_value = temp_upload_done_value(&filename, query.total);
    let complete = match tokio::fs::read_to_string(&done).await {
        Ok(value) if value == done_value => tokio::fs::metadata(&target)
            .await
            .map(|meta| meta.is_file() && meta.len() == query.total)
            .unwrap_or(false),
        _ => false,
    };
    let uploaded = if complete {
        query.total
    } else {
        match tokio::fs::symlink_metadata(&part).await {
            Ok(meta) if meta.file_type().is_file() => meta.len(),
            Ok(_) => return Err(AppError::bad("上传暂存路径不是普通文件")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        }
    };
    if uploaded > query.total {
        return Err(AppError::bad(
            "服务器暂存文件大于待上传文件，请重新选择文件",
        ));
    }
    Ok(Json(TempUploadResult {
        name: filename,
        path: target.display().to_string(),
        uploaded,
        total: query.total,
        complete,
    }))
}

async fn temp_upload_chunk(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<TempUploadQuery>,
    body: Bytes,
) -> Result<Json<TempUploadResult>, AppError> {
    if body.len() > TEMP_UPLOAD_CHUNK_MAX_BYTES {
        return Err(AppError::bad("上传分片不能超过 16 MiB"));
    }
    let offset = query
        .offset
        .ok_or_else(|| AppError::bad("缺少上传偏移量"))?;
    let chunk_len = body.len() as u64;
    if offset > query.total || chunk_len > query.total.saturating_sub(offset) {
        return Err(AppError::bad("上传分片超出文件大小"));
    }

    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };
    let gate = state.lock_project(&id);
    let _guard = gate.lock().await;
    let (filename, target, part, done) = temp_upload_paths(&project, &query).await?;
    let current = match tokio::fs::symlink_metadata(&part).await {
        Ok(meta) if meta.file_type().is_file() => meta.len(),
        Ok(_) => return Err(AppError::bad("上传暂存路径不是普通文件")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error.into()),
    };
    if current != offset {
        return Err(AppError::bad(format!(
            "上传偏移量不一致，服务器已接收 {current} 字节"
        )));
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part)
        .await?;
    file.write_all(&body).await?;
    file.flush().await?;
    file.sync_data().await?;
    let uploaded = current + chunk_len;
    let complete = uploaded == query.total;
    if complete {
        tokio::fs::write(&done, temp_upload_done_value(&filename, query.total)).await?;
        replace_temp_upload(&part, &target).await?;
    }

    Ok(Json(TempUploadResult {
        name: filename,
        path: target.display().to_string(),
        uploaded,
        total: query.total,
        complete,
    }))
}

async fn create_replace(
    State(state): State<AppState>,
    Path(id): Path<String>,
    multipart: Multipart,
) -> Result<Json<ReplaceResult>, AppError> {
    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };

    let gate = state.lock_project(&id);
    let _guard = gate.lock().await;

    let upload = receive_upload(&state, multipart).await?;
    apply_replace(state, project, upload).await
}

struct IncomingUpload {
    note: String,
    restart: bool,
    stop_compose: bool,
    files: Vec<PathBuf>,
    tmp: PathBuf,
    job_id: Option<String>,
}

async fn receive_upload(
    state: &AppState,
    mut multipart: Multipart,
) -> Result<IncomingUpload, AppError> {
    let tmp = state.paths.uploads_dir.join(Uuid::new_v4().to_string());
    tokio::fs::create_dir_all(&tmp).await?;

    let mut note = String::new();
    let mut restart = false;
    let mut stop_compose = false;
    let mut files = Vec::new();
    let mut job_id = None;

    let result: Result<(), AppError> = async {
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|e| AppError::bad(format!("无效的上传数据：{e}")))?
        {
            let field_name = field.name().unwrap_or("").to_string();
            let filename = field.file_name().unwrap_or("").to_string();

            if field_name == "note" {
                note = field
                    .text()
                    .await
                    .map_err(|e| AppError::bad(e.to_string()))?;
                continue;
            }
            if field_name == "job_id" {
                let v = field
                    .text()
                    .await
                    .map_err(|e| AppError::bad(e.to_string()))?;
                if !v.trim().is_empty() {
                    job_id = Some(v.trim().to_string());
                }
                continue;
            }
            if field_name == "restart" {
                let v = field
                    .text()
                    .await
                    .map_err(|e| AppError::bad(e.to_string()))?;
                restart = matches!(v.trim(), "1" | "true" | "on" | "yes");
                continue;
            }
            if field_name == "stop_compose" {
                let v = field
                    .text()
                    .await
                    .map_err(|e| AppError::bad(e.to_string()))?;
                stop_compose = matches!(v.trim(), "1" | "true" | "on" | "yes");
                continue;
            }
            if filename.is_empty() {
                continue;
            }
            let filename = safe_filename(&filename).map_err(|e| AppError::bad(e.to_string()))?;
            if !is_uploadable_name(&filename) {
                return Err(AppError::bad(format!(
                    "{filename} 不是 .tar / .tar.gz / .tgz 镜像包或 .jar"
                )));
            }
            let dest = tmp.join(&filename);
            let mut file = tokio::fs::File::create(&dest).await?;
            let mut field = field;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|e| AppError::bad(format!("上传中断：{e}")))?
            {
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            files.push(dest);
        }
        Ok(())
    }
    .await;

    if let Err(err) = result {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return Err(err);
    }
    if let Some(id) = &job_id {
        state.jobs.set(id, "upload", "上传已接收，准备处理…", 0, 0);
    }
    Ok(IncomingUpload {
        note,
        restart,
        stop_compose,
        files,
        tmp,
        job_id,
    })
}

async fn apply_update(
    state: AppState,
    project: Project,
    upload: IncomingUpload,
    kind: &str,
) -> Result<Json<UpdateResult>, AppError> {
    let IncomingUpload {
        note,
        restart,
        stop_compose,
        files: staged_files,
        tmp,
        job_id,
    } = upload;
    let version_id = Uuid::new_v4().to_string();
    let version_no = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::next_version_no(&conn, &project.id)?
    };
    let tree = state.paths.version_tree(&project.id, &version_id);
    let images_dir = state.paths.version_images(&project.id, &version_id);
    let jars_dir = state.paths.version_jars(&project.id, &version_id);
    let live = PathBuf::from(&project.directory);
    let version_dir = state.paths.version_dir(&project.id, &version_id);

    job_set(&state, job_id.as_deref(), "database-backup", "正在执行升级前数据库备份…", 0, 0);
    if let Err(err) = crate::dbbackup::create_before_update(&state, &project).await {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        job_err(&state, job_id.as_deref(), &err.to_string());
        return Err(AppError::bad(format!("升级前数据库备份失败，已取消升级：{err}")));
    }

    let stopped = compose_down_for_backup(&state, &live, job_id.as_deref(), stop_compose).await;
    let stopped = match stopped {
        Ok(v) => v,
        Err(err) => {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            job_err(&state, job_id.as_deref(), &err.to_string());
            return Err(err);
        }
    };

    let tree_clone = tree.clone();
    let live_clone = live.clone();
    let jobs = state.jobs.clone();
    let job_for_snap = job_id.clone();
    job_set(
        &state,
        job_id.as_deref(),
        "snapshot",
        "正在备份当前目录…",
        0,
        0,
    );
    if let Err(err) = tokio::task::spawn_blocking(move || {
        snapshot_blocking(live_clone, tree_clone, jobs, job_for_snap)
    })
    .await
    .map_err(|e| AppError::internal(e.to_string()))
    .and_then(|r| r.map_err(AppError::from))
    {
        job_err(&state, job_id.as_deref(), &err.to_string());
        let _ = remove_dir_if_exists(&version_dir);
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        if stopped {
            compose_up_best_effort(
                &state,
                &live,
                job_id.as_deref(),
                "备份失败，正在重新启动 Compose…",
            )
            .await;
        }
        return Err(err);
    }

    if let Err(err) = tokio::fs::create_dir_all(&images_dir).await {
        let _ = remove_dir_if_exists(&version_dir);
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        if stopped {
            compose_up_best_effort(
                &state,
                &live,
                job_id.as_deref(),
                "发布失败，正在重新启动 Compose…",
            )
            .await;
        }
        return Err(err.into());
    }

    let (archives, jar_files): (Vec<PathBuf>, Vec<PathBuf>) =
        staged_files.into_iter().partition(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .map(is_image_archive_name)
                .unwrap_or(false)
        });

    let mut loaded = Vec::new();
    if !archives.is_empty() {
        job_set(
            &state,
            job_id.as_deref(),
            "load",
            "正在导入 Docker 镜像…",
            0,
            archives.len() as u64,
        );
    }
    if let Err(err) = load_and_retag(
        &state,
        &archives,
        Some(&images_dir),
        &mut loaded,
        job_id.as_deref(),
    )
    .await
    {
        job_err(&state, job_id.as_deref(), &err.to_string());
        let _ = remove_dir_if_exists(&version_dir);
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        if stopped {
            compose_up_best_effort(
                &state,
                &live,
                job_id.as_deref(),
                "发布失败，正在重新启动 Compose…",
            )
            .await;
        }
        return Err(err);
    }

    let mounts = read_jar_mounts(&live);
    let mut deployed_jars = Vec::new();
    if !jar_files.is_empty() {
        job_set(
            &state,
            job_id.as_deref(),
            "deploy",
            "正在写入 JAR…",
            0,
            jar_files.len() as u64,
        );
    }
    if let Err(err) = deploy_jars(
        &jar_files,
        Some(&jars_dir),
        &live,
        &mounts,
        &mut deployed_jars,
    )
    .await
    {
        job_err(&state, job_id.as_deref(), &err.to_string());
        let _ = remove_dir_if_exists(&version_dir);
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        if stopped {
            compose_up_best_effort(
                &state,
                &live,
                job_id.as_deref(),
                "发布失败，正在重新启动 Compose…",
            )
            .await;
        }
        return Err(err);
    }
    let _ = tokio::fs::remove_dir_all(&tmp).await;

    let now = db::now_rfc3339();
    let version = Version {
        id: version_id.clone(),
        project_id: project.id.clone(),
        version_no,
        label: format!("v{version_no}"),
        note: note.trim().to_string(),
        backup_path: tree.display().to_string(),
        images: loaded.clone(),
        jars: deployed_jars.clone(),
        is_current: true,
        kind: kind.to_string(),
        created_at: now,
        app_bytes: 0,
        backup_bytes: 0,
        repo_bytes: 0,
    };

    let db_err = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        match db::insert_version(&conn, &version) {
            Ok(()) => {
                db::mark_current(&conn, &project.id, &version_id)?;
                None
            }
            Err(err) => Some(err),
        }
    };
    if let Some(err) = db_err {
        let _ = remove_dir_if_exists(&version_dir);
        if stopped {
            compose_up_best_effort(
                &state,
                &live,
                job_id.as_deref(),
                "写入失败，正在重新启动 Compose…",
            )
            .await;
        }
        job_err(&state, job_id.as_deref(), &err.to_string());
        return Err(err.into());
    }

    if restart {
        job_set(
            &state,
            job_id.as_deref(),
            "compose",
            "正在重启 Compose…",
            0,
            0,
        );
        if let Err(err) = restart_after_update(&state, &live, &deployed_jars).await {
            tracing::warn!("compose up after update failed: {err:#}");
            let msg = format!(
                "文件已保存为版本 {}，但 Compose 启动失败：{err}",
                version.label
            );
            job_err(&state, job_id.as_deref(), &msg);
            return Err(AppError::internal(msg));
        }
    }
    job_ok(&state, job_id.as_deref(), "发布完成");

    let version = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_version(&conn, &project.id, &version_id)?
            .ok_or_else(|| AppError::internal("version missing after insert"))?
    };

    Ok(Json(UpdateResult {
        version,
        loaded,
        jars: deployed_jars,
    }))
}

async fn apply_replace(
    state: AppState,
    project: Project,
    upload: IncomingUpload,
) -> Result<Json<ReplaceResult>, AppError> {
    let IncomingUpload {
        restart,
        files: staged_files,
        tmp,
        job_id,
        ..
    } = upload;
    let live = PathBuf::from(&project.directory);

    let (archives, jar_files): (Vec<PathBuf>, Vec<PathBuf>) =
        staged_files.into_iter().partition(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .map(is_image_archive_name)
                .unwrap_or(false)
        });

    if archives.is_empty() && jar_files.is_empty() {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return Err(AppError::bad("请上传镜像包或 JAR"));
    }

    job_set(&state, job_id.as_deref(), "database-backup", "正在执行升级前数据库备份…", 0, 0);
    if let Err(err) = crate::dbbackup::create_before_update(&state, &project).await {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        job_err(&state, job_id.as_deref(), &err.to_string());
        return Err(AppError::bad(format!("升级前数据库备份失败，已取消替换：{err}")));
    }

    let mut loaded = Vec::new();
    if !archives.is_empty() {
        job_set(
            &state,
            job_id.as_deref(),
            "load",
            "正在导入 Docker 镜像…",
            0,
            archives.len() as u64,
        );
    }
    if let Err(err) = load_and_retag(&state, &archives, None, &mut loaded, job_id.as_deref()).await
    {
        job_err(&state, job_id.as_deref(), &err.to_string());
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return Err(err);
    }

    let mounts = read_jar_mounts(&live);
    let mut deployed_jars = Vec::new();
    if !jar_files.is_empty() {
        job_set(
            &state,
            job_id.as_deref(),
            "deploy",
            "正在写入 JAR…",
            0,
            jar_files.len() as u64,
        );
    }
    if let Err(err) = deploy_jars(&jar_files, None, &live, &mounts, &mut deployed_jars).await {
        job_err(&state, job_id.as_deref(), &err.to_string());
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return Err(err);
    }
    let _ = tokio::fs::remove_dir_all(&tmp).await;

    if restart {
        job_set(
            &state,
            job_id.as_deref(),
            "compose",
            "正在重启 Compose…",
            0,
            0,
        );
        let result = if !archives.is_empty() {
            state.docker.compose_restart(&live).await
        } else {
            restart_after_update(&state, &live, &deployed_jars).await
        };
        if let Err(err) = result {
            tracing::warn!("compose restart after replace failed: {err:#}");
            let msg = format!("文件已替换，但 Compose 重启失败：{err}");
            job_err(&state, job_id.as_deref(), &msg);
            return Err(AppError::internal(msg));
        }
    }
    job_ok(&state, job_id.as_deref(), "替换完成");

    Ok(Json(ReplaceResult {
        loaded,
        jars: deployed_jars,
    }))
}

fn read_jar_mounts(project_dir: &std::path::Path) -> Vec<JarMount> {
    match find_compose_file(project_dir) {
        Some(path) => {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            parse_compose_jar_mounts(&text)
        }
        None => Vec::new(),
    }
}

async fn deploy_jars(
    jar_files: &[PathBuf],
    archive_dir: Option<&std::path::Path>,
    live: &std::path::Path,
    mounts: &[JarMount],
    deployed: &mut Vec<DeployedJar>,
) -> Result<(), AppError> {
    if jar_files.is_empty() {
        return Ok(());
    }
    if let Some(dir) = archive_dir {
        tokio::fs::create_dir_all(dir).await?;
    }
    for src in jar_files {
        let name = src
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("app.jar")
            .to_string();
        if let Some(dir) = archive_dir {
            tokio::fs::copy(src, dir.join(&name)).await?;
        }

        let matches: Vec<&JarMount> = mounts.iter().filter(|m| m.basename == name).collect();
        let (dests, services) = if matches.is_empty() {
            let fallback = if live.join("jars").is_dir() {
                live.join("jars").join(&name)
            } else {
                live.join(&name)
            };
            (vec![fallback], Vec::new())
        } else {
            let dests = matches
                .iter()
                .map(|m| resolve_host_path(live, &m.host_path))
                .collect::<Vec<_>>();
            let services = matches
                .iter()
                .map(|m| m.service.clone())
                .collect::<Vec<_>>();
            (dests, services)
        };

        for dest in &dests {
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::copy(src, dest).await?;
        }
        deployed.push(DeployedJar {
            file: name,
            dest: dests
                .first()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            services,
        });
    }
    Ok(())
}

async fn restart_after_update(
    state: &AppState,
    live: &std::path::Path,
    jars: &[DeployedJar],
) -> anyhow::Result<String> {
    let mut services: Vec<String> = jars
        .iter()
        .flat_map(|j| j.services.iter().cloned())
        .collect();
    services.sort();
    services.dedup();
    let mut out = String::new();
    if !services.is_empty() {
        out = state.docker.compose_up_recreate(live, &services).await?;
    }
    // Backup may have run `compose down`. Recreating only JAR services
    // would leave PostGIS / broker / etc. stopped.
    let more = state.docker.compose_up(live).await?;
    if out.is_empty() {
        Ok(more)
    } else {
        Ok(format!("{out}\n{more}"))
    }
}

async fn load_and_retag(
    state: &AppState,
    staged_files: &[PathBuf],
    images_dir: Option<&std::path::Path>,
    loaded: &mut Vec<LoadedImage>,
    job_id: Option<&str>,
) -> Result<(), AppError> {
    let total = staged_files.len() as u64;
    for (idx, src) in staged_files.iter().enumerate() {
        let name = src
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("image.tar.gz")
            .to_string();
        let load_from = if let Some(dir) = images_dir {
            tokio::fs::create_dir_all(dir).await?;
            let dest = dir.join(&name);
            tokio::fs::copy(src, &dest).await?;
            dest
        } else {
            src.clone()
        };
        job_set(
            state,
            job_id,
            "load",
            &format!("正在 docker load {name}"),
            idx as u64,
            total,
        );

        let images = state
            .docker
            .load_archive(&load_from)
            .await
            .map_err(|e| AppError::internal(e.to_string()))?;

        let mut latest_tags = Vec::new();
        for image in &images {
            if let Some(latest) = to_latest_tag(image) {
                state
                    .docker
                    .tag(image, &latest)
                    .await
                    .map_err(|e| AppError::internal(e.to_string()))?;
                if !latest_tags.contains(&latest) {
                    latest_tags.push(latest);
                }
            }
        }
        loaded.push(LoadedImage {
            file: name,
            loaded: images,
            latest_tags,
        });
    }
    Ok(())
}

async fn rollback(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RollbackBody>,
) -> Result<Json<UpdateResult>, AppError> {
    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };
    let target = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_version(&conn, &id, &body.version_id)?
            .ok_or_else(|| AppError::not_found("版本不存在"))?
    };

    let gate = state.lock_project(&id);
    let _guard = match gate.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            return Err(AppError::Conflict("正在恢复或升级中，请勿重复操作".into()));
        }
    };

    // Snapshot current live tree first so rollback itself can be undone.
    let safety_id = Uuid::new_v4().to_string();
    let safety_no = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::next_version_no(&conn, &id)?
    };
    let safety_tree = state.paths.version_tree(&id, &safety_id);
    let safety_dir = state.paths.version_dir(&id, &safety_id);
    let live = PathBuf::from(&project.directory);
    let stopped =
        compose_down_for_backup(&state, &live, body.job_id.as_deref(), body.stop_compose).await?;
    let safety_tree_clone = safety_tree.clone();
    let live_clone = live.clone();
    let jobs = state.jobs.clone();
    let job_id = body.job_id.clone();
    job_set(
        &state,
        job_id.as_deref(),
        "snapshot",
        "正在备份当前目录…",
        0,
        0,
    );
    if let Err(err) = tokio::task::spawn_blocking(move || {
        snapshot_blocking(live_clone, safety_tree_clone, jobs, job_id)
    })
    .await
    .map_err(|e| AppError::internal(e.to_string()))
    .and_then(|r| r.map_err(AppError::from))
    {
        job_err(&state, body.job_id.as_deref(), &err.to_string());
        let _ = remove_dir_if_exists(&safety_dir);
        if stopped {
            compose_up_best_effort(
                &state,
                &live,
                body.job_id.as_deref(),
                "备份失败，正在重新启动 Compose…",
            )
            .await;
        }
        return Err(err);
    }

    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::insert_version(
            &conn,
            &Version {
                id: safety_id,
                project_id: id.clone(),
                version_no: safety_no,
                label: format!("v{safety_no}"),
                note: format!("回滚到 {} 前的自动快照", target.label),
                backup_path: safety_tree.display().to_string(),
                images: Vec::new(),
                jars: Vec::new(),
                is_current: false,
                kind: "pre-rollback".into(),
                created_at: db::now_rfc3339(),
                app_bytes: 0,
                backup_bytes: 0,
                repo_bytes: 0,
            },
        )?;
    }

    let snapshot = PathBuf::from(&target.backup_path);
    let live_restore = live.clone();
    job_set(
        &state,
        body.job_id.as_deref(),
        "restore",
        "正在解压备份…",
        0,
        0,
    );
    let jobs = state.jobs.clone();
    let job_for_restore = body.job_id.clone();
    if let Err(err) = tokio::task::spawn_blocking(move || {
        restore_blocking(snapshot, live_restore, jobs, job_for_restore)
    })
    .await
    .map_err(|e| AppError::internal(e.to_string()))
    .and_then(|r| r.map_err(AppError::from))
    {
        job_err(&state, body.job_id.as_deref(), &err.to_string());
        if stopped {
            compose_up_best_effort(
                &state,
                &live,
                body.job_id.as_deref(),
                "恢复失败，正在重新启动 Compose…",
            )
            .await;
        }
        return Err(err);
    }

    let images_dir = state.paths.version_images(&id, &target.id);
    let mut loaded = target.images.clone();
    if images_dir.is_dir() {
        let mut archives = Vec::new();
        let mut rd = tokio::fs::read_dir(&images_dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            if path.is_file() {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    if is_image_archive_name(name) {
                        archives.push(path);
                    }
                }
            }
        }
        archives.sort();
        if !archives.is_empty() {
            loaded.clear();
            for archive in archives {
                let name = archive
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("image.tar.gz")
                    .to_string();
                let images = state
                    .docker
                    .load_archive(&archive)
                    .await
                    .map_err(|e| AppError::internal(e.to_string()))?;
                let mut latest_tags = Vec::new();
                for image in &images {
                    if let Some(latest) = to_latest_tag(image) {
                        state
                            .docker
                            .tag(image, &latest)
                            .await
                            .map_err(|e| AppError::internal(e.to_string()))?;
                        if !latest_tags.contains(&latest) {
                            latest_tags.push(latest);
                        }
                    }
                }
                loaded.push(LoadedImage {
                    file: name,
                    loaded: images,
                    latest_tags,
                });
            }
        }
    }

    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::mark_current(&conn, &id, &target.id)?;
    }

    if body.restart {
        job_set(
            &state,
            body.job_id.as_deref(),
            "compose",
            "正在重启 Compose…",
            0,
            0,
        );
        let mounts = read_jar_mounts(&live);
        let mut services: Vec<String> = mounts.into_iter().map(|m| m.service).collect();
        services.sort();
        services.dedup();
        let result: anyhow::Result<String> = async {
            let mut out = String::new();
            if !services.is_empty() {
                out = state.docker.compose_up_recreate(&live, &services).await?;
            }
            let more = state.docker.compose_up(&live).await?;
            if out.is_empty() {
                Ok(more)
            } else {
                Ok(format!("{out}\n{more}"))
            }
        }
        .await;
        if let Err(err) = result {
            job_err(&state, body.job_id.as_deref(), &err.to_string());
            return Err(AppError::internal(err.to_string()));
        }
    }
    job_ok(&state, body.job_id.as_deref(), "恢复完成");

    let version = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_version(&conn, &id, &target.id)?
            .ok_or_else(|| AppError::internal("version missing"))?
    };

    Ok(Json(UpdateResult {
        version,
        loaded,
        jars: target.jars,
    }))
}

async fn compose_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ComposeStatus>, AppError> {
    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };
    let dir = PathBuf::from(&project.directory);
    let compose_file = find_compose_file(&dir);
    let (images, jar_mounts) = match &compose_file {
        Some(p) => {
            let text = std::fs::read_to_string(p).unwrap_or_default();
            (parse_compose_images(&text), parse_compose_jar_mounts(&text))
        }
        None => (Vec::new(), Vec::new()),
    };

    let (services, raw, error) = match state.docker.compose_ps_raw(&dir).await {
        Ok(raw) => {
            let services = parse_compose_ps(&raw);
            (services, Some(raw), None)
        }
        Err(err) => (Vec::new(), None, Some(err.to_string())),
    };
    let meta = state.docker.meta().await;

    Ok(Json(ComposeStatus {
        available: meta.available,
        compose_file: compose_file.map(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }),
        env_file: find_env_file(&dir).map(|_| crate::paths::ENV_FILENAME.to_string()),
        images,
        jar_mounts,
        services,
        raw,
        error,
    }))
}

fn read_compose_disk(path: &std::path::Path, exists: bool) -> Result<(String, String), AppError> {
    if !exists {
        return Ok((String::new(), compose_etag("")));
    }
    let content = std::fs::read_to_string(path).map_err(|e| {
        AppError::internal(format!("读取 Compose 文件失败：{}：{e}", path.display()))
    })?;
    let etag = compose_etag(&content);
    Ok((content, etag))
}

fn compose_file_view(
    conn: &rusqlite::Connection,
    project_id: &str,
    path: &std::path::Path,
    filename: &str,
    exists: bool,
    content: &str,
    etag: &str,
    recreate_log: Option<String>,
    unchanged: bool,
) -> Result<ComposeFileView, AppError> {
    let revisions = db::list_compose_revisions(conn, project_id)?;
    let latest = revisions.first();
    Ok(ComposeFileView {
        filename: filename.to_string(),
        path: path.display().to_string(),
        exists,
        bytes: content.len() as u64,
        matches_latest: latest.map(|r| r.etag.as_str()) == Some(etag),
        current_rev_id: latest.map(|r| r.id.clone()),
        current_rev_no: latest.map(|r| r.rev_no),
        revisions,
        content: content.to_string(),
        etag: etag.to_string(),
        recreate_log,
        unchanged,
    })
}

fn record_compose_revision(
    conn: &rusqlite::Connection,
    project_id: &str,
    filename: &str,
    content: &str,
    note: &str,
    kind: &str,
) -> Result<ComposeRevision, AppError> {
    let rev = ComposeRevision {
        id: Uuid::new_v4().to_string(),
        project_id: project_id.to_string(),
        rev_no: db::next_compose_rev_no(conn, project_id)?,
        filename: filename.to_string(),
        etag: compose_etag(content),
        bytes: content.len() as u64,
        content: content.to_string(),
        note: note.to_string(),
        kind: kind.to_string(),
        created_at: db::now_rfc3339(),
    };
    db::insert_compose_revision(conn, &rev)?;
    Ok(rev)
}

fn snapshot_disk_if_needed(
    conn: &rusqlite::Connection,
    project_id: &str,
    filename: &str,
    disk: &str,
    disk_etag: &str,
) -> Result<(), AppError> {
    if disk.is_empty() {
        return Ok(());
    }
    let latest = db::latest_compose_revision(conn, project_id)?;
    match latest {
        Some(r) if r.etag == disk_etag => Ok(()),
        Some(_) => {
            record_compose_revision(
                conn,
                project_id,
                filename,
                disk,
                "磁盘上的未记录内容",
                "external",
            )?;
            Ok(())
        }
        None => {
            record_compose_revision(
                conn,
                project_id,
                filename,
                disk,
                "打开编辑器时的线上文件",
                "baseline",
            )?;
            Ok(())
        }
    }
}

async fn install_compose_file(
    state: &AppState,
    dir: &std::path::Path,
    dest: &std::path::Path,
    content: &str,
) -> Result<(), AppError> {
    if matches!(
        state.docker.compose_kind().await,
        crate::docker::ComposeKind::Missing
    ) {
        write_text_atomic(dest, content)?;
        return Ok(());
    }
    let draft = compose_draft_path(dest);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&draft, content)
        .map_err(|e| AppError::internal(format!("写入草稿失败：{e}")))?;
    let draft_name = draft
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Err(err) = state.docker.compose_config_file(dir, &draft_name).await {
        let _ = std::fs::remove_file(&draft);
        return Err(AppError::bad(format!("Compose 文件校验失败：{err}")));
    }
    if let Err(err) = commit_compose_draft(dest, &draft) {
        let _ = std::fs::remove_file(&draft);
        return Err(AppError::internal(err.to_string()));
    }
    Ok(())
}

async fn save_compose_content(
    state: &AppState,
    project: &Project,
    content: String,
    note: String,
    kind: &str,
    recreate: bool,
    expected_etag: Option<&str>,
) -> Result<Json<ComposeFileView>, AppError> {
    validate_compose_text(&content).map_err(|e| AppError::bad(e.to_string()))?;
    let dir = PathBuf::from(&project.directory);
    let (dest, filename, exists) = compose_live_path(&dir);
    let (_disk, disk_etag) = read_compose_disk(&dest, exists)?;
    if let Some(expected) = expected_etag {
        if expected != disk_etag {
            return Err(AppError::conflict(
                "磁盘上的 Compose 文件已变化，请关闭后重新打开再保存",
            ));
        }
    }

    let gate = state.lock_project(&project.id);
    let _guard = gate.lock().await;

    let (disk, disk_etag, exists) = {
        let (dest2, _, exists2) = compose_live_path(&dir);
        read_compose_disk(&dest2, exists2).map(|(c, e)| (c, e, exists2))?
    };
    if let Some(expected) = expected_etag {
        if expected != disk_etag {
            return Err(AppError::conflict(
                "磁盘上的 Compose 文件已变化，请关闭后重新打开再保存",
            ));
        }
    }

    if content == disk {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        snapshot_disk_if_needed(&conn, &project.id, &filename, &disk, &disk_etag)?;
        let view = compose_file_view(
            &conn,
            &project.id,
            &dest,
            &filename,
            exists,
            &disk,
            &disk_etag,
            None,
            true,
        )?;
        return Ok(Json(view));
    }

    install_compose_file(state, &dir, &dest, &content).await?;
    let etag = compose_etag(&content);
    let default_note = if kind == "restore" {
        "恢复历史版本"
    } else {
        ""
    };
    let note = if note.trim().is_empty() {
        default_note.to_string()
    } else {
        note
    };

    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        snapshot_disk_if_needed(&conn, &project.id, &filename, &disk, &disk_etag)?;
        record_compose_revision(&conn, &project.id, &filename, &content, &note, kind)?;
    }

    let recreate_log = if recreate {
        match state.docker.compose_up(&dir).await {
            Ok(logs) => Some(logs),
            Err(err) => Some(format!("Compose 文件已保存，但启动失败：{err}")),
        }
    } else {
        None
    };

    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let view = compose_file_view(
        &conn,
        &project.id,
        &dest,
        &filename,
        true,
        &content,
        &etag,
        recreate_log,
        false,
    )?;
    Ok(Json(view))
}

async fn compose_file_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ComposeFileView>, AppError> {
    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };
    let dir = PathBuf::from(&project.directory);
    let (path, filename, exists) = compose_live_path(&dir);
    let (content, etag) = read_compose_disk(&path, exists)?;
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    Ok(Json(compose_file_view(
        &conn, &id, &path, &filename, exists, &content, &etag, None, false,
    )?))
}

async fn compose_file_put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SaveComposeBody>,
) -> Result<Json<ComposeFileView>, AppError> {
    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };
    save_compose_content(
        &state,
        &project,
        body.content,
        body.note.unwrap_or_default(),
        "save",
        body.recreate,
        body.expected_etag.as_deref(),
    )
    .await
}

async fn compose_revisions_list(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ComposeRevision>>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    if db::get_project(&conn, &id)?.is_none() {
        return Err(AppError::not_found("项目不存在"));
    }
    Ok(Json(db::list_compose_revisions(&conn, &id)?))
}

async fn compose_revision_get(
    State(state): State<AppState>,
    Path((id, rev_id)): Path<(String, String)>,
) -> Result<Json<ComposeRevision>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    db::get_compose_revision(&conn, &id, &rev_id)?
        .map(Json)
        .ok_or_else(|| AppError::not_found("该 Compose 版本不存在"))
}

async fn compose_revision_restore(
    State(state): State<AppState>,
    Path((id, rev_id)): Path<(String, String)>,
    Json(body): Json<RestoreComposeBody>,
) -> Result<Json<ComposeFileView>, AppError> {
    let (project, rev) = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let project =
            db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?;
        let rev = db::get_compose_revision(&conn, &id, &rev_id)?
            .ok_or_else(|| AppError::not_found("该 Compose 版本不存在"))?;
        (project, rev)
    };
    let note = body
        .note
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("恢复 r{}", rev.rev_no));
    save_compose_content(
        &state,
        &project,
        rev.content,
        note,
        "restore",
        body.recreate,
        None,
    )
    .await
}

fn env_file_view(
    conn: &rusqlite::Connection,
    project_id: &str,
    path: &std::path::Path,
    filename: &str,
    exists: bool,
    content: &str,
    etag: &str,
    recreate_log: Option<String>,
    unchanged: bool,
) -> Result<ComposeFileView, AppError> {
    let revisions = db::list_env_revisions(conn, project_id)?;
    let latest = revisions.first();
    Ok(ComposeFileView {
        filename: filename.to_string(),
        path: path.display().to_string(),
        exists,
        bytes: content.len() as u64,
        matches_latest: latest.map(|r| r.etag.as_str()) == Some(etag),
        current_rev_id: latest.map(|r| r.id.clone()),
        current_rev_no: latest.map(|r| r.rev_no),
        revisions,
        content: content.to_string(),
        etag: etag.to_string(),
        recreate_log,
        unchanged,
    })
}

fn record_env_revision(
    conn: &rusqlite::Connection,
    project_id: &str,
    filename: &str,
    content: &str,
    note: &str,
    kind: &str,
) -> Result<ComposeRevision, AppError> {
    let rev = ComposeRevision {
        id: Uuid::new_v4().to_string(),
        project_id: project_id.to_string(),
        rev_no: db::next_env_rev_no(conn, project_id)?,
        filename: filename.to_string(),
        etag: compose_etag(content),
        bytes: content.len() as u64,
        content: content.to_string(),
        note: note.to_string(),
        kind: kind.to_string(),
        created_at: db::now_rfc3339(),
    };
    db::insert_env_revision(conn, &rev)?;
    Ok(rev)
}

fn snapshot_env_disk_if_needed(
    conn: &rusqlite::Connection,
    project_id: &str,
    filename: &str,
    disk: &str,
    disk_etag: &str,
) -> Result<(), AppError> {
    let latest = db::latest_env_revision(conn, project_id)?;
    match latest {
        Some(r) if r.etag == disk_etag => Ok(()),
        Some(_) => {
            record_env_revision(
                conn,
                project_id,
                filename,
                disk,
                "磁盘上的未记录内容",
                "external",
            )?;
            Ok(())
        }
        None => {
            record_env_revision(
                conn,
                project_id,
                filename,
                disk,
                "打开编辑器时的线上文件",
                "baseline",
            )?;
            Ok(())
        }
    }
}

async fn save_env_content(
    state: &AppState,
    project: &Project,
    content: String,
    note: String,
    kind: &str,
    recreate: bool,
    expected_etag: Option<&str>,
) -> Result<Json<ComposeFileView>, AppError> {
    validate_env_text(&content).map_err(|e| AppError::bad(e.to_string()))?;
    let dir = PathBuf::from(&project.directory);
    let (dest, _filename, exists) = env_live_path(&dir);
    let (_disk, disk_etag) = read_compose_disk(&dest, exists)?;
    if let Some(expected) = expected_etag {
        if expected != disk_etag {
            return Err(AppError::conflict(
                "磁盘上的 .env 文件已变化，请关闭后重新打开再保存",
            ));
        }
    }

    let gate = state.lock_project(&project.id);
    let _guard = gate.lock().await;

    let (dest, filename, exists) = env_live_path(&dir);
    let (disk, disk_etag) = read_compose_disk(&dest, exists)?;
    if let Some(expected) = expected_etag {
        if expected != disk_etag {
            return Err(AppError::conflict(
                "磁盘上的 .env 文件已变化，请关闭后重新打开再保存",
            ));
        }
    }

    if exists && content == disk {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        snapshot_env_disk_if_needed(&conn, &project.id, &filename, &disk, &disk_etag)?;
        let view = env_file_view(
            &conn,
            &project.id,
            &dest,
            &filename,
            exists,
            &disk,
            &disk_etag,
            None,
            true,
        )?;
        return Ok(Json(view));
    }

    write_text_atomic(&dest, &content)?;
    let etag = compose_etag(&content);
    let default_note = if kind == "restore" {
        "恢复历史版本"
    } else {
        ""
    };
    let note = if note.trim().is_empty() {
        default_note.to_string()
    } else {
        note
    };

    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        snapshot_env_disk_if_needed(&conn, &project.id, &filename, &disk, &disk_etag)?;
        record_env_revision(&conn, &project.id, &filename, &content, &note, kind)?;
    }

    let recreate_log = if recreate {
        match state.docker.compose_up(&dir).await {
            Ok(logs) => Some(logs),
            Err(err) => Some(format!(".env 已保存，但启动失败：{err}")),
        }
    } else {
        None
    };

    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let view = env_file_view(
        &conn,
        &project.id,
        &dest,
        &filename,
        true,
        &content,
        &etag,
        recreate_log,
        false,
    )?;
    Ok(Json(view))
}

async fn env_file_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ComposeFileView>, AppError> {
    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };
    let dir = PathBuf::from(&project.directory);
    let (path, filename, exists) = env_live_path(&dir);
    let (content, etag) = read_compose_disk(&path, exists)?;
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    Ok(Json(env_file_view(
        &conn, &id, &path, &filename, exists, &content, &etag, None, false,
    )?))
}

async fn env_file_put(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SaveComposeBody>,
) -> Result<Json<ComposeFileView>, AppError> {
    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };
    save_env_content(
        &state,
        &project,
        body.content,
        body.note.unwrap_or_default(),
        "save",
        body.recreate,
        body.expected_etag.as_deref(),
    )
    .await
}

async fn env_revisions_list(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ComposeRevision>>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    if db::get_project(&conn, &id)?.is_none() {
        return Err(AppError::not_found("项目不存在"));
    }
    Ok(Json(db::list_env_revisions(&conn, &id)?))
}

async fn env_revision_get(
    State(state): State<AppState>,
    Path((id, rev_id)): Path<(String, String)>,
) -> Result<Json<ComposeRevision>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    db::get_env_revision(&conn, &id, &rev_id)?
        .map(Json)
        .ok_or_else(|| AppError::not_found("该环境变量版本不存在"))
}

async fn env_revision_restore(
    State(state): State<AppState>,
    Path((id, rev_id)): Path<(String, String)>,
    Json(body): Json<RestoreComposeBody>,
) -> Result<Json<ComposeFileView>, AppError> {
    let (project, rev) = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let project =
            db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?;
        let rev = db::get_env_revision(&conn, &id, &rev_id)?
            .ok_or_else(|| AppError::not_found("该环境变量版本不存在"))?;
        (project, rev)
    };
    let note = body
        .note
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("恢复 r{}", rev.rev_no));
    save_env_content(
        &state,
        &project,
        rev.content,
        note,
        "restore",
        body.recreate,
        None,
    )
    .await
}

async fn compose_up(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<LogsResult>, AppError> {
    compose_action(state, id, |d, dir| async move { d.compose_up(&dir).await }).await
}

async fn compose_down(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<LogsResult>, AppError> {
    compose_action(
        state,
        id,
        |d, dir| async move { d.compose_down(&dir).await },
    )
    .await
}

async fn compose_restart(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<LogsResult>, AppError> {
    compose_action(
        state,
        id,
        |d, dir| async move { d.compose_restart(&dir).await },
    )
    .await
}

async fn compose_restart_service(
    State(state): State<AppState>,
    Path((id, service)): Path<(String, String)>,
) -> Result<Json<LogsResult>, AppError> {
    crate::term::require_service_name(&service)?;
    compose_action(state, id, move |d, dir| async move {
        d.compose_restart_service(&dir, &service).await
    })
    .await
}

async fn compose_action<F, Fut>(
    state: AppState,
    id: String,
    f: F,
) -> Result<Json<LogsResult>, AppError>
where
    F: FnOnce(crate::docker::Docker, PathBuf) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<String>>,
{
    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };
    let gate = state.lock_project(&id);
    let _guard = gate.lock().await;
    let logs = f(state.docker.clone(), PathBuf::from(project.directory))
        .await
        .map_err(|e| AppError::internal(e.to_string()))?;
    Ok(Json(LogsResult { logs }))
}

#[derive(serde::Deserialize)]
struct LogsQuery {
    tail: Option<u32>,
    service: Option<String>,
}

async fn compose_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<LogsQuery>,
) -> Result<Json<LogsResult>, AppError> {
    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };
    let tail = q.tail.unwrap_or(200).min(2000);
    let service = match q
        .service
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(name) => {
            crate::term::require_service_name(name)?;
            Some(name.to_string())
        }
        None => None,
    };
    let logs = state
        .docker
        .compose_logs(&PathBuf::from(project.directory), tail, service.as_deref())
        .await
        .map_err(|e| AppError::internal(e.to_string()))?;
    Ok(Json(LogsResult { logs }))
}

async fn compose_exec(
    State(state): State<AppState>,
    Path((id, service)): Path<(String, String)>,
    Query(query): Query<crate::term::ExecQuery>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Result<impl IntoResponse, AppError> {
    crate::term::require_service_name(&service)?;
    let project = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_project(&conn, &id)?.ok_or_else(|| AppError::not_found("项目不存在"))?
    };
    let dir = PathBuf::from(project.directory);
    let docker = state.docker.clone();
    Ok(ws.on_upgrade(move |socket| {
        crate::term::run_exec_socket(socket, docker, dir, service, query)
    }))
}

#[derive(Deserialize)]
struct DbParams {
    service: String,
    engine: Option<String>,
    database: Option<String>,
    schema: Option<String>,
    name: Option<String>,
    offset: Option<u32>,
    limit: Option<u32>,
    filter_col: Option<String>,
    filter_op: Option<String>,
    filter_value: Option<String>,
}

fn db_project(state: &AppState, id: &str) -> Result<Project, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    db::get_project(&conn, id)?.ok_or_else(|| AppError::not_found("项目不存在"))
}

async fn db_meta(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DbParams>,
) -> Result<Json<crate::dbadmin::DbMeta>, AppError> {
    crate::dbadmin::require_engine(q.engine.as_deref())?;
    let project = db_project(&state, &id)?;
    Ok(Json(
        crate::dbadmin::meta(
            &state.docker,
            std::path::Path::new(&project.directory),
            &q.service,
        )
        .await?,
    ))
}

async fn db_databases(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DbParams>,
) -> Result<Json<crate::dbadmin::NameList>, AppError> {
    crate::dbadmin::require_engine(q.engine.as_deref())?;
    let project = db_project(&state, &id)?;
    Ok(Json(
        crate::dbadmin::databases(
            &state.docker,
            std::path::Path::new(&project.directory),
            &q.service,
        )
        .await?,
    ))
}

async fn db_schemas(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DbParams>,
) -> Result<Json<crate::dbadmin::NameList>, AppError> {
    crate::dbadmin::require_engine(q.engine.as_deref())?;
    let database = q
        .database
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad("请选择数据库"))?;
    let project = db_project(&state, &id)?;
    Ok(Json(
        crate::dbadmin::schemas(
            &state.docker,
            std::path::Path::new(&project.directory),
            &q.service,
            database,
        )
        .await?,
    ))
}

async fn db_objects(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DbParams>,
) -> Result<Json<crate::dbadmin::ObjectList>, AppError> {
    crate::dbadmin::require_engine(q.engine.as_deref())?;
    let database = q
        .database
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad("请选择数据库"))?;
    let schema = q
        .schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad("请选择 schema"))?;
    let project = db_project(&state, &id)?;
    Ok(Json(
        crate::dbadmin::objects(
            &state.docker,
            std::path::Path::new(&project.directory),
            &q.service,
            database,
            schema,
        )
        .await?,
    ))
}

async fn db_rows(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DbParams>,
) -> Result<Json<crate::dbadmin::RowPage>, AppError> {
    crate::dbadmin::require_engine(q.engine.as_deref())?;
    let database = q
        .database
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad("请选择数据库"))?;
    let schema = q
        .schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad("请选择 schema"))?;
    let name = q
        .name
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::bad("请选择表或视图"))?;
    let project = db_project(&state, &id)?;
    let filter = match (
        q.filter_col.as_deref().filter(|s| !s.is_empty()),
        q.filter_value.as_deref().filter(|s| !s.trim().is_empty()),
    ) {
        (Some(col), Some(val)) => Some(crate::dbadmin::RowFilter {
            column: col.to_string(),
            op: q.filter_op.as_deref().unwrap_or("contains").to_string(),
            value: val.to_string(),
        }),
        _ => None,
    };
    Ok(Json(
        crate::dbadmin::rows(
            &state.docker,
            std::path::Path::new(&project.directory),
            &q.service,
            database,
            schema,
            name,
            q.offset.unwrap_or(0),
            crate::dbadmin::clamp_limit(q.limit),
            filter,
        )
        .await?,
    ))
}

async fn db_update_row(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<crate::dbadmin::UpdateRowBody>,
) -> Result<Json<crate::dbadmin::UpdateRowResult>, AppError> {
    crate::dbadmin::require_engine(body.engine.as_deref())?;
    let project = db_project(&state, &id)?;
    Ok(Json(
        crate::dbadmin::update_row(
            &state.docker,
            std::path::Path::new(&project.directory),
            &body,
        )
        .await?,
    ))
}

async fn db_delete_row(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<crate::dbadmin::DeleteRowBody>,
) -> Result<Json<crate::dbadmin::DeleteRowResult>, AppError> {
    crate::dbadmin::require_engine(body.engine.as_deref())?;
    let project = db_project(&state, &id)?;
    Ok(Json(
        crate::dbadmin::delete_row(
            &state.docker,
            std::path::Path::new(&project.directory),
            &body,
        )
        .await?,
    ))
}

async fn db_query(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<crate::dbadmin::QueryBody>,
) -> Result<Json<crate::dbadmin::QueryResult>, AppError> {
    crate::dbadmin::require_engine(body.engine.as_deref())?;
    let project = db_project(&state, &id)?;
    Ok(Json(
        crate::dbadmin::query(
            &state.docker,
            std::path::Path::new(&project.directory),
            &body.service,
            &body.database,
            &body.sql,
        )
        .await?,
    ))
}

async fn db_backups(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DbParams>,
) -> Result<Json<Vec<crate::dbbackup::BackupManifest>>, AppError> {
    let _ = db_project(&state, &id)?;
    let mut backups = crate::dbbackup::list(&state.paths.db_backups_dir, &id)?;
    if !q.service.is_empty() {
        backups.retain(|backup| backup.service == q.service);
    }
    if let Some(database) = q.database.as_deref().filter(|value| !value.is_empty()) {
        backups.retain(|backup| backup.database == database);
    }
    Ok(Json(backups))
}

async fn db_backup_create(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<crate::dbbackup::BackupBody>,
) -> Result<Json<crate::progress::JobProgress>, AppError> {
    crate::dbadmin::require_engine(body.engine.as_deref())?;
    let project = db_project(&state, &id)?;
    let job = if let Some(job_id) = body.job_id.as_deref() {
        state
            .jobs
            .get(job_id)
            .ok_or_else(|| AppError::not_found("进度任务不存在"))?
    } else {
        state.jobs.create()
    };
    let job_id = job.id.clone();
    let run_state = state.clone();
    let lock = state.lock_project(&id);
    tokio::spawn(async move {
        let _guard = lock.lock().await;
        run_state.jobs.set(
            &job_id,
            "backup",
            "正在导出 PostgreSQL 数据（数据库工具不提供准确百分比，大库需要等待）…",
            0,
            0,
        );
        let result = crate::dbbackup::create(
            &run_state.docker,
            std::path::Path::new(&project.directory),
            &run_state.paths.db_backups_dir,
            &id,
            &body.service,
            &body.database,
            "manual",
        )
        .await;
        match result {
            Ok(backup) => run_state.jobs.finish_ok(
                &job_id,
                &format!("数据库备份完成：{}（{} 字节）", backup.database, backup.dump_bytes),
            ),
            Err(error) => run_state.jobs.finish_err(&job_id, &error.to_string()),
        }
    });
    Ok(Json(job))
}

async fn db_backup_schedule_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<crate::dbbackup::BackupSchedule>, AppError> {
    let _ = db_project(&state, &id)?;
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    Ok(Json(crate::dbbackup::get_schedule(&conn, &id)?))
}

async fn db_backup_schedule_save(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<crate::dbbackup::BackupScheduleBody>,
) -> Result<Json<crate::dbbackup::BackupSchedule>, AppError> {
    let _ = db_project(&state, &id)?;
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    Ok(Json(crate::dbbackup::save_schedule(&conn, &id, &body)?))
}

async fn db_backup_schedule_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<crate::progress::JobProgress>, AppError> {
    let project = db_project(&state, &id)?;
    let schedule = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        crate::dbbackup::get_schedule(&conn, &id)?
    };
    if schedule.service.is_empty() || schedule.database.is_empty() {
        return Err(AppError::bad("请先保存自动备份的容器和数据库"));
    }
    let job = state.jobs.create();
    let job_id = job.id.clone();
    let run_state = state.clone();
    let lock = state.lock_project(&id);
    tokio::spawn(async move {
        let _guard = lock.lock().await;
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        if let Ok(conn) = run_state.db.lock() {
            let _ = crate::dbbackup::set_run_status(
                &conn, &id, Some(&today), "running", "自动备份正在执行", false,
            );
        }
        run_state.jobs.set(
            &job_id,
            "backup",
            "正在导出 PostgreSQL 数据（数据库工具不提供准确百分比，大库需要等待）…",
            0,
            0,
        );
        let result = crate::dbbackup::run_schedule(&run_state, &project, &schedule).await;
        if let Ok(conn) = run_state.db.lock() {
            match &result {
                Ok(backup) => { let _ = crate::dbbackup::set_run_status(&conn, &id, None, "success", &format!("备份完成：{}", backup.id), true); }
                Err(error) => { let _ = crate::dbbackup::set_run_status(&conn, &id, None, "failed", &error.to_string(), true); }
            }
        }
        match result {
            Ok(backup) => run_state.jobs.finish_ok(&job_id, &format!("自动备份完成：{}", backup.id)),
            Err(error) => run_state.jobs.finish_err(&job_id, &error.to_string()),
        }
    });
    Ok(Json(job))
}

async fn db_backup_restore(
    State(state): State<AppState>,
    Path((id, backup_id)): Path<(String, String)>,
    Json(body): Json<crate::dbbackup::RestoreBody>,
) -> Result<Json<crate::progress::JobProgress>, AppError> {
    crate::dbadmin::require_engine(body.engine.as_deref())?;
    let project = db_project(&state, &id)?;
    let job = if let Some(job_id) = body.job_id.as_deref() {
        state
            .jobs
            .get(job_id)
            .ok_or_else(|| AppError::not_found("进度任务不存在"))?
    } else {
        state.jobs.create()
    };
    let job_id = job.id.clone();
    let run_state = state.clone();
    let lock = state.lock_project(&id);
    tokio::spawn(async move {
        let _guard = lock.lock().await;
        run_state.jobs.set(
            &job_id,
            "safety-backup",
            "恢复前正在自动备份当前数据库…",
            5,
            100,
        );
        let result = crate::dbbackup::restore(
            &run_state.docker,
            std::path::Path::new(&project.directory),
            &run_state.paths.db_backups_dir,
            &id,
            &backup_id,
            &body,
        )
        .await;
        match result {
            Ok(restored) => run_state.jobs.finish_ok(
                &job_id,
                &format!(
                    "数据库恢复完成；恢复前安全备份：{}",
                    restored.safety_backup_id
                ),
            ),
            Err(error) => run_state.jobs.finish_err(&job_id, &error.to_string()),
        }
    });
    Ok(Json(job))
}

async fn vendor_xterm_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("assets/vendor/xterm.css"),
    )
}

async fn vendor_xterm_js() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("assets/vendor/xterm.js"),
    )
}

async fn vendor_xterm_fit() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("assets/vendor/xterm-addon-fit.js"),
    )
}

async fn vendor_compose_canvas() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            // This module and the inline page code share an API. Never let a
            // newly loaded page execute against yesterday's cached module.
            (header::CACHE_CONTROL, "no-cache, must-revalidate"),
        ],
        include_str!("assets/compose-canvas.js"),
    )
}

fn vendor_font(bytes: &'static [u8]) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        bytes,
    )
}

async fn vendor_iosevka_regular() -> impl IntoResponse {
    vendor_font(include_bytes!("assets/vendor/iosevka-term-regular.woff2"))
}

async fn vendor_iosevka_bold() -> impl IntoResponse {
    vendor_font(include_bytes!("assets/vendor/iosevka-term-bold.woff2"))
}

async fn vendor_ace(Path(name): Path<String>) -> Result<impl IntoResponse, AppError> {
    let file = name.rsplit('/').next().unwrap_or(name.as_str());
    let bytes: &'static [u8] = match file {
        "ace.js" => include_bytes!("assets/vendor/ace/ace.js"),
        "mode-yaml.js" => include_bytes!("assets/vendor/ace/mode-yaml.js"),
        "mode-ini.js" => include_bytes!("assets/vendor/ace/mode-ini.js"),
        "mode-sql.js" => include_bytes!("assets/vendor/ace/mode-sql.js"),
        "theme-github.js" => include_bytes!("assets/vendor/ace/theme-github.js"),
        "theme-github_dark.js" => include_bytes!("assets/vendor/ace/theme-github_dark.js"),
        "ext-searchbox.js" => include_bytes!("assets/vendor/ace/ext-searchbox.js"),
        _ => return Err(AppError::not_found("资源不存在")),
    };
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        bytes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::fs;

    fn temp_root() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "cangling-orphan-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_project_template(archive: &FsPath, prefix: Option<&str>) {
        let file = fs::File::create(archive).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let name = prefix
            .map(|value| format!("{value}/docker-compose.yml"))
            .unwrap_or_else(|| "docker-compose.yml".into());
        let content = b"services:\n  zot:\n    image: ghcr.io/project-zot/zot:latest\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, &content[..]).unwrap();
        tar.finish().unwrap();
    }

    #[test]
    fn harbor_template_uses_images_software_set_layout() {
        let root = temp_root();
        let archive = root.join("repo").join(HARBOR_TEMPLATE_REL);
        fs::create_dir_all(archive.parent().unwrap()).unwrap();
        fs::write(&archive, b"template").unwrap();

        assert_eq!(harbor_template_archive(&root), Some(archive));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn harbor_template_unpacks_wrapped_project_into_fixed_directory() {
        let root = temp_root();
        let archive = root.join("cangling-zot.tar.gz");
        let dest = root.join("opt/cangling/cangling-zot");
        write_project_template(&archive, Some("cangling-zot"));

        unpack_harbor_template(&archive, &dest).unwrap();

        assert!(dest.join("docker-compose.yml").is_file());
        assert!(!dest.join("cangling-zot").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn harbor_template_rejects_unsafe_archive_paths() {
        assert!(unsafe_archive_path(FsPath::new("../outside")));
        assert!(unsafe_archive_path(FsPath::new("/absolute")));
        assert!(!unsafe_archive_path(FsPath::new("cangling-zot/compose.yaml")));
    }

    #[test]
    fn harbor_images_select_the_readme_defined_architecture_file() {
        let root = temp_root();
        let amd64 = root.join("zot-image-amd64.tar.gz");
        let arm64 = root.join("zot-image-arm64.tar.gz");
        fs::write(&amd64, b"amd64 image").unwrap();
        fs::write(&arm64, b"arm64 image").unwrap();

        assert_eq!(
            harbor_image_archive(&root, "zot-image-amd64.tar.gz"),
            Some(amd64)
        );
        assert_eq!(
            harbor_image_archive(&root, "zot-image-arm64.tar.gz"),
            Some(arm64)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn harbor_initialization_runs_readme_script_and_creates_data() {
        let root = temp_root();
        fs::create_dir_all(root.join("config")).unwrap();
        fs::write(
            root.join("init-auth.sh"),
            "#!/usr/bin/env bash\nset -e\nprintf 'user:hash\\n' > config/htpasswd\n",
        )
        .unwrap();

        initialize_harbor_project(&root).unwrap();

        assert!(root.join("data").is_dir());
        assert_eq!(
            fs::read_to_string(root.join("config/htpasswd")).unwrap(),
            "user:hash\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(
                fs::metadata(root.join("init-auth.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn zot_hosts_replaces_old_mapping_idempotently() {
        let original = "127.0.0.1 localhost\n10.0.0.1 hub.cangling.cn old-alias\n# keep me\n";
        let first = update_zot_hosts(original, "10.0.0.9");
        let second = update_zot_hosts(&first, "10.0.0.9");
        assert_eq!(first, second);
        assert!(first.contains("127.0.0.1 localhost"));
        assert!(first.contains("# keep me"));
        assert!(first.contains("10.0.0.9\thub.cangling.cn"));
        assert!(!first.contains("10.0.0.1 hub.cangling.cn"));
    }

    #[test]
    fn zot_registry_injection_preserves_other_registries() {
        let existing = "mirrors:\n  registry.local:\n    endpoint:\n      - http://registry.local\nconfigs:\n  registry.local:\n    tls:\n      insecure_skip_verify: true\n  hub.cangling.cn:\n    auth:\n      username: existing-user\n";
        let rendered = update_zot_registries(
            existing,
            FsPath::new("/etc/rancher/k3s/cangling-ca.crt"),
        )
        .unwrap();
        let yaml: serde_yaml::Value = serde_yaml::from_str(&rendered).unwrap();
        assert_eq!(
            yaml["mirrors"]["hub.cangling.cn"]["endpoint"][0],
            "https://hub.cangling.cn"
        );
        assert_eq!(
            yaml["configs"]["hub.cangling.cn"]["tls"]["ca_file"],
            "/etc/rancher/k3s/cangling-ca.crt"
        );
        assert_eq!(
            yaml["mirrors"]["registry.local"]["endpoint"][0],
            "http://registry.local"
        );
        assert_eq!(
            yaml["configs"]["registry.local"]["tls"]["insecure_skip_verify"],
            true
        );
        assert_eq!(
            yaml["configs"]["hub.cangling.cn"]["auth"]["username"],
            "existing-user"
        );
    }

    #[test]
    fn zot_environment_files_write_hosts_ca_and_registry() {
        let root = temp_root();
        let hosts = root.join("etc/hosts");
        let registries = root.join("etc/rancher/k3s/registries.yaml");
        let ca = root.join("etc/rancher/k3s/cangling-ca.crt");
        fs::create_dir_all(hosts.parent().unwrap()).unwrap();
        fs::write(&hosts, "127.0.0.1 localhost\n").unwrap();

        configure_zot_environment_files(
            &hosts,
            &registries,
            &ca,
            "192.168.3.10",
            "-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----\n",
        )
        .unwrap();

        assert!(fs::read_to_string(hosts)
            .unwrap()
            .contains("192.168.3.10\thub.cangling.cn"));
        assert!(fs::read_to_string(registries)
            .unwrap()
            .contains("hub.cangling.cn"));
        assert!(fs::read_to_string(ca).unwrap().contains("BEGIN CERTIFICATE"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn lists_failed_first_backup_dir() {
        let root = temp_root();
        fs::create_dir_all(root.join("leftover-id").join("repo.git")).unwrap();
        fs::write(
            root.join("leftover-id").join("repo.git").join("HEAD"),
            b"ref",
        )
        .unwrap();
        let items = collect_orphans(&root, &HashSet::new(), &HashMap::new());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "leftover-id");
        assert!(items[0].bytes > 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn lists_leftover_version_under_known_project() {
        let root = temp_root();
        fs::create_dir_all(root.join("proj").join("repo.git")).unwrap();
        fs::create_dir_all(root.join("proj").join("good-ver")).unwrap();
        fs::create_dir_all(root.join("proj").join("ghost-ver")).unwrap();
        fs::write(
            root.join("proj").join("ghost-ver").join("tree.gitref"),
            b"dead",
        )
        .unwrap();
        let known = HashSet::from(["proj".into()]);
        let versions = HashMap::from([("proj".into(), HashSet::from(["good-ver".into()]))]);
        let items = collect_orphans(&root, &known, &versions);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "proj/ghost-ver");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn valid_backup_id_rejects_traversal() {
        assert!(valid_backup_id("a1b2-c3"));
        assert!(!valid_backup_id(".."));
        assert!(!valid_backup_id("a/b"));
        assert!(!valid_backup_id(""));
    }

    #[test]
    fn np4_images_use_the_selected_architecture_directory() {
        let root = temp_root();
        let base = root.join("base-images");
        fs::create_dir_all(base.join("x86")).unwrap();
        fs::create_dir_all(base.join("arm")).unwrap();
        fs::write(base.join("x86").join("x86-image.tar.gz"), b"x86").unwrap();
        fs::write(base.join("arm").join("arm-image.tar.gz"), b"arm").unwrap();

        let images = np4_image_archives(&root, &["x86", "amd64"]);
        assert_eq!(images, vec![base.join("x86").join("x86-image.tar.gz")]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn np4_images_support_repository_latest_linux_arch_layout() {
        let root = temp_root();
        let image = root
            .join("base-images")
            .join("latest")
            .join("linux")
            .join("amd64")
            .join("np4-images-x86.tar.gz");
        fs::create_dir_all(image.parent().unwrap()).unwrap();
        fs::write(&image, b"x86").unwrap();

        assert_eq!(np4_image_archives(&root, &["x86", "amd64"]), vec![image]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn np4_template_copies_only_the_selected_compose_file() {
        let root = temp_root();
        let source = root.join("source");
        let target = root.join("target");
        fs::create_dir_all(source.join("config")).unwrap();
        fs::create_dir_all(source.join("base-images").join("x86")).unwrap();
        fs::write(source.join("docker-compose-x86.yaml"), b"x86 compose").unwrap();
        fs::write(source.join("docker-compose-arm.yaml"), b"arm compose").unwrap();
        fs::write(source.join("config").join("app.conf"), b"config").unwrap();
        fs::write(source.join("base-images").join("x86").join("image.tar.gz"), b"image")
            .unwrap();

        copy_np4_template(&source, &target, "x86").unwrap();
        assert_eq!(fs::read(target.join("docker-compose.yaml")).unwrap(), b"x86 compose");
        assert_eq!(fs::read(target.join("config").join("app.conf")).unwrap(), b"config");
        assert!(!target.join("docker-compose-arm.yaml").exists());
        assert!(!target.join("base-images").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn np4_initialization_sets_master_host_and_copies_jars() {
        let root = temp_root();
        let project = root.join("project");
        let jar = root.join("source.jar");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(".env.example"), "PORT=8080\nHOST=old-host\n").unwrap();
        fs::write(&jar, b"jar").unwrap();

        initialize_np4_project(&project, &[jar], "192.168.3.10").unwrap();
        assert_eq!(
            fs::read_to_string(project.join(".env")).unwrap(),
            "PORT=8080\nHOST=192.168.3.10\n"
        );
        assert_eq!(fs::read(project.join("jars").join("source.jar")).unwrap(), b"jar");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn np4_data_directories_follow_env_data_path() {
        let root = temp_root();
        let project = root.join("project");
        let data = root.join("storage");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join(".env"),
            format!("DATA_PATH='{}'\n", data.display()),
        )
        .unwrap();

        let dirs = create_np4_data_dirs(&project).unwrap();
        assert_eq!(
            dirs,
            vec![
                data.join("np4"),
                data.join("np4-resource-images"),
                data.join("algo_tools")
            ]
        );
        assert!(dirs.iter().all(|dir| dir.is_dir()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn np4_repo_update_selects_jars_and_tar_gz_and_detects_changes() {
        let root = temp_root();
        let source = root
            .join("repo/np4/np4-jars/latest/all/all");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("api.jar"), b"jar-v1").unwrap();
        fs::write(source.join("broker.tar.gz"), b"image-v1").unwrap();
        fs::write(source.join("ignore.txt"), b"ignore").unwrap();
        let files = np4_repo_update_files(&root).unwrap().unwrap();
        assert_eq!(files.len(), 2);

        let images = root.join("images");
        let jars = root.join("jars");
        fs::create_dir_all(&images).unwrap();
        fs::create_dir_all(&jars).unwrap();
        fs::copy(source.join("api.jar"), jars.join("api.jar")).unwrap();
        fs::copy(
            source.join("broker.tar.gz"),
            images.join("broker.tar.gz"),
        )
        .unwrap();
        assert!(np4_repo_files_unchanged(&files, &images, &jars).unwrap());
        fs::write(source.join("api.jar"), b"jar-v2").unwrap();
        assert!(!np4_repo_files_unchanged(&files, &images, &jars).unwrap());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn np4_repo_manifest_detects_content_changes_without_copying_packages() {
        let root = temp_root();
        let jar = root.join("api.jar");
        let image = root.join("broker.tar.gz");
        let manifest = root.join("state/project.json");
        fs::write(&jar, b"jar-v1").unwrap();
        fs::write(&image, b"image-v1").unwrap();

        let files = vec![image.clone(), jar.clone()];
        let stamps = np4_repo_file_stamps(&files).unwrap();
        assert_eq!(np4_repo_manifest_matches(&manifest, &stamps).unwrap(), None);
        save_np4_repo_manifest(&manifest, &stamps).unwrap();
        assert_eq!(
            np4_repo_manifest_matches(&manifest, &stamps).unwrap(),
            Some(true)
        );

        fs::write(&jar, b"jar-v2").unwrap();
        let changed = np4_repo_file_stamps(&files).unwrap();
        assert_eq!(
            np4_repo_manifest_matches(&manifest, &changed).unwrap(),
            Some(false)
        );
        assert_eq!(fs::read_dir(root.join("state")).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn project_file_paths_reject_parent_traversal() {
        assert_eq!(clean_project_relative("config/app.yaml").unwrap(), PathBuf::from("config/app.yaml"));
        assert!(clean_project_relative("../etc/passwd").is_err());
        assert!(clean_project_relative("config/../../etc/passwd").is_err());
        assert!(clean_project_relative("/etc/passwd").is_err());
    }

    #[test]
    fn project_file_editability_covers_configuration_files() {
        for path in ["README.txt", "docker-compose.yaml", "app.ini", ".env", ".env.prod", "nginx/app.conf"] {
            assert!(project_file_is_editable(FsPath::new(path)), "{path}");
        }
        assert!(!project_file_is_editable(FsPath::new("image.tar.gz")));
        assert!(!project_file_is_editable(FsPath::new("app.jar")));
    }

    #[test]
    fn create_subdirectory_creates_one_safe_child() {
        let root = temp_root();
        let child = create_subdirectory(root.to_str().unwrap(), "volume-data").unwrap();
        assert_eq!(child, root.join("volume-data"));
        assert!(child.is_dir());
        for invalid in ["", "..", "../escape", "nested/path", ".hidden"] {
            assert!(create_subdirectory(root.to_str().unwrap(), invalid).is_err(), "{invalid}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn temp_upload_id_must_be_a_uuid() {
        let id = Uuid::new_v4();
        assert_eq!(checked_temp_upload_id(&id.to_string()).unwrap(), id.to_string());
        for invalid in ["", "../escape", "not-a-uuid"] {
            assert!(checked_temp_upload_id(invalid).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn temp_upload_accepts_any_safe_filename_and_creates_temp_directory() {
        let root = temp_root();
        let project = Project {
            id: "project-id".into(),
            name: "project".into(),
            description: String::new(),
            directory: root.display().to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            current_version_no: None,
            current_version_id: None,
            version_count: 0,
        };
        let upload_id = Uuid::new_v4().to_string();
        let query = TempUploadQuery {
            upload_id: upload_id.clone(),
            name: "arbitrary-file.custom-extension".into(),
            total: 4,
            offset: None,
        };

        let (filename, target, part, done) = temp_upload_paths(&project, &query).await.unwrap();
        assert_eq!(filename, "arbitrary-file.custom-extension");
        assert_eq!(target, root.join("temp/arbitrary-file.custom-extension"));
        assert_eq!(part, root.join(format!("temp/.uploads/{upload_id}.part")));
        assert_eq!(done, root.join(format!("temp/.uploads/{upload_id}.done")));
        assert!(root.join("temp/.uploads").is_dir());

        let unsafe_query = TempUploadQuery {
            name: "../escape".into(),
            ..query
        };
        let (filename, target, _, _) = temp_upload_paths(&project, &unsafe_query).await.unwrap();
        assert_eq!(filename, "escape");
        assert_eq!(target, root.join("temp/escape"));
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn completed_temp_upload_replaces_same_named_file() {
        let root = temp_root();
        let target = root.join("same-name.bin");
        let part = root.join("upload.part");
        fs::write(&target, b"old content").unwrap();
        fs::write(&part, b"new content").unwrap();

        replace_temp_upload(&part, &target).await.unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new content");
        assert!(!part.exists());
        let _ = fs::remove_dir_all(&root);
    }
}
