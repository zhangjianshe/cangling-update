use crate::auth;
use crate::backup::{remove_dir_if_exists, restore_directory, snapshot_directory};
use crate::db;
use crate::docker::{parse_compose_ps, to_latest_tag};
use crate::error::AppError;
use crate::models::*;
use crate::paths::{
    find_compose_file, is_image_archive_name, parse_compose_images, require_absolute_dir,
    safe_filename,
};
use crate::state::AppState;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::header;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/auth/status", get(auth::status))
        .route("/api/auth/setup", post(auth::setup))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/meta", get(meta))
        .route("/api/validate-directory", post(validate_directory))
        .route("/api/projects", get(list_projects).post(create_project))
        .route(
            "/api/projects/{id}",
            get(get_project).put(update_project).delete(delete_project),
        )
        .route("/api/projects/{id}/versions", get(list_versions))
        .route("/api/projects/{id}/updates", post(create_update))
        .route("/api/projects/{id}/rollback", post(rollback))
        .route("/api/projects/{id}/compose", get(compose_status))
        .route("/api/projects/{id}/compose/up", post(compose_up))
        .route("/api/projects/{id}/compose/down", post(compose_down))
        .route(
            "/api/projects/{id}/compose/restart",
            post(compose_restart),
        )
        .route("/api/projects/{id}/compose/logs", get(compose_logs))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .with_state(state)
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024 * 1024))
}

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("assets/index.html"),
    )
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
        docker: state.docker.meta(),
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
    let (images, warning) = match &compose {
        Some(path) => {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            (parse_compose_images(&text), None)
        }
        None => (
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
        warning,
    })
}

async fn list_projects(State(state): State<AppState>) -> Result<Json<Vec<Project>>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    Ok(Json(db::list_projects(&conn)?))
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
    let tree_clone = tree.clone();
    tokio::task::spawn_blocking(move || snapshot_directory(&live, &tree_clone))
        .await
        .map_err(|e| AppError::internal(e.to_string()))??;

    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        if let Err(err) = db::insert_project(&conn, &project) {
            let _ = remove_dir_if_exists(&state.paths.version_dir(&id, &version_id));
            let msg = err.to_string();
            if msg.contains("UNIQUE") {
                return Err(AppError::Conflict("已存在同名项目".into()));
            }
            return Err(err.into());
        }
        let version = Version {
            id: version_id.clone(),
            project_id: id.clone(),
            version_no: 1,
            label: "v1".into(),
            note: "基线快照".into(),
            backup_path: tree.display().to_string(),
            images: Vec::new(),
            is_current: true,
            kind: "baseline".into(),
            created_at: now,
        };
        db::insert_version(&conn, &version)?;
    }

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
    db::update_project(&conn, &id, &name, &description, &directory, &db::now_rfc3339())?;
    db::get_project(&conn, &id)?
        .map(Json)
        .ok_or_else(|| AppError::not_found("项目不存在"))
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
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    if db::get_project(&conn, &id)?.is_none() {
        return Err(AppError::not_found("项目不存在"));
    }
    Ok(Json(db::list_versions(&conn, &id)?))
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

struct IncomingUpload {
    note: String,
    restart: bool,
    files: Vec<PathBuf>,
    tmp: PathBuf,
}

async fn receive_upload(
    state: &AppState,
    mut multipart: Multipart,
) -> Result<IncomingUpload, AppError> {
    let tmp = state.paths.uploads_dir.join(Uuid::new_v4().to_string());
    tokio::fs::create_dir_all(&tmp).await?;

    let mut note = String::new();
    let mut restart = true;
    let mut files = Vec::new();

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
            if field_name == "restart" {
                let v = field
                    .text()
                    .await
                    .map_err(|e| AppError::bad(e.to_string()))?;
                restart = matches!(v.trim(), "1" | "true" | "on" | "yes");
                continue;
            }
            if filename.is_empty() {
                continue;
            }
            let filename = safe_filename(&filename).map_err(|e| AppError::bad(e.to_string()))?;
            if !is_image_archive_name(&filename) {
                return Err(AppError::bad(format!(
                    "{filename} 不是 .tar / .tar.gz / .tgz 镜像包"
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
    Ok(IncomingUpload {
        note,
        restart,
        files,
        tmp,
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
        files: staged_files,
        tmp,
    } = upload;
    let version_id = Uuid::new_v4().to_string();
    let version_no = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::next_version_no(&conn, &project.id)?
    };
    let tree = state.paths.version_tree(&project.id, &version_id);
    let images_dir = state.paths.version_images(&project.id, &version_id);
    let live = PathBuf::from(&project.directory);

    let tree_clone = tree.clone();
    let live_clone = live.clone();
    if let Err(err) = tokio::task::spawn_blocking(move || snapshot_directory(&live_clone, &tree_clone))
        .await
        .map_err(|e| AppError::internal(e.to_string()))
        .and_then(|r| r.map_err(AppError::from))
    {
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return Err(err);
    }

    if let Err(err) = tokio::fs::create_dir_all(&images_dir).await {
        let _ = remove_dir_if_exists(&state.paths.version_dir(&project.id, &version_id));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        return Err(err.into());
    }

    let mut loaded = Vec::new();
    if let Err(err) = load_and_retag(&state, &staged_files, &images_dir, &mut loaded).await {
        let _ = remove_dir_if_exists(&state.paths.version_dir(&project.id, &version_id));
        let _ = tokio::fs::remove_dir_all(&tmp).await;
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
        is_current: true,
        kind: kind.to_string(),
        created_at: now,
    };

    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::insert_version(&conn, &version)?;
        db::mark_current(&conn, &project.id, &version_id)?;
    }

    if restart {
        if let Err(err) = state.docker.compose_up(&live).await {
            tracing::warn!("compose up after update failed: {err:#}");
            return Err(AppError::internal(format!(
                "镜像已导入且版本 {} 已保存，但 Compose 启动失败：{err}",
                version.label
            )));
        }
    }

    let version = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_version(&conn, &project.id, &version_id)?
            .ok_or_else(|| AppError::internal("version missing after insert"))?
    };

    Ok(Json(UpdateResult { version, loaded }))
}

async fn load_and_retag(
    state: &AppState,
    staged_files: &[PathBuf],
    images_dir: &std::path::Path,
    loaded: &mut Vec<LoadedImage>,
) -> Result<(), AppError> {
    for src in staged_files {
        let name = src
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("image.tar.gz")
            .to_string();
        let dest = images_dir.join(&name);
        tokio::fs::copy(src, &dest).await?;

        let images = state
            .docker
            .load_archive(&dest)
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
            return Err(AppError::Conflict(
                "正在恢复或升级中，请勿重复操作".into(),
            ));
        }
    };

    // Snapshot current live tree first so rollback itself can be undone.
    let safety_id = Uuid::new_v4().to_string();
    let safety_no = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::next_version_no(&conn, &id)?
    };
    let safety_tree = state.paths.version_tree(&id, &safety_id);
    let live = PathBuf::from(&project.directory);
    let safety_tree_clone = safety_tree.clone();
    let live_clone = live.clone();
    tokio::task::spawn_blocking(move || snapshot_directory(&live_clone, &safety_tree_clone))
        .await
        .map_err(|e| AppError::internal(e.to_string()))??;

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
                is_current: false,
                kind: "pre-rollback".into(),
                created_at: db::now_rfc3339(),
            },
        )?;
    }

    let snapshot = PathBuf::from(&target.backup_path);
    let live_restore = live.clone();
    tokio::task::spawn_blocking(move || restore_directory(&snapshot, &live_restore))
        .await
        .map_err(|e| AppError::internal(e.to_string()))??;

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
        state
            .docker
            .compose_up(&live)
            .await
            .map_err(|e| AppError::internal(e.to_string()))?;
    }

    let version = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_version(&conn, &id, &target.id)?
            .ok_or_else(|| AppError::internal("version missing"))?
    };

    Ok(Json(UpdateResult { version, loaded }))
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
    let images = match &compose_file {
        Some(p) => parse_compose_images(&std::fs::read_to_string(p).unwrap_or_default()),
        None => Vec::new(),
    };

    let (services, raw, error) = match state.docker.compose_ps_raw(&dir).await {
        Ok(raw) => {
            let services = parse_compose_ps(&raw);
            (services, Some(raw), None)
        }
        Err(err) => (Vec::new(), None, Some(err.to_string())),
    };

    Ok(Json(ComposeStatus {
        available: state.docker.available,
        compose_file: compose_file.map(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }),
        images,
        services,
        raw,
        error,
    }))
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
    compose_action(state, id, |d, dir| async move { d.compose_down(&dir).await }).await
}

async fn compose_restart(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<LogsResult>, AppError> {
    compose_action(state, id, |d, dir| async move { d.compose_restart(&dir).await }).await
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
    let logs = state
        .docker
        .compose_logs(&PathBuf::from(project.directory), tail)
        .await
        .map_err(|e| AppError::internal(e.to_string()))?;
    Ok(Json(LogsResult { logs }))
}
