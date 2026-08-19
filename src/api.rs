use crate::auth;
use crate::backup::{
    dir_size, remove_dir_if_exists, restore_directory, restore_directory_with_progress,
    snapshot_directory, snapshot_directory_with_progress,
};
use crate::db;
use crate::docker::{parse_compose_ps, to_latest_tag};
use crate::error::AppError;
use crate::models::*;
use crate::paths::{
    find_compose_file, is_image_archive_name, is_uploadable_name,
    parse_compose_images, parse_compose_jar_mounts, require_absolute_dir, resolve_host_path,
    safe_filename, JarMount,
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
        .route("/api/jobs", post(create_job))
        .route("/api/jobs/{id}", get(get_job))
        .route("/api/meta", get(meta))
        .route("/api/validate-directory", post(validate_directory))
        .route("/api/orphans", get(list_orphans))
        .route("/api/orphans/{*id}", axum::routing::delete(delete_orphan))
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
        .route(
            "/api/projects/{id}/compose/exec/{service}",
            get(compose_exec),
        )
        .route("/vendor/xterm.css", get(vendor_xterm_css))
        .route("/vendor/xterm.js", get(vendor_xterm_js))
        .route("/vendor/xterm-addon-fit.js", get(vendor_xterm_fit))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .with_state(state)
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024 * 1024))
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

fn job_set(state: &AppState, job_id: Option<&str>, phase: &str, message: &str, current: u64, total: u64) {
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
    let html = include_str!("assets/index.html").replace(
        "__APP_VERSION__",
        env!("CARGO_PKG_VERSION"),
    );
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
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
    let (images, jar_mounts, warning) = match &compose {
        Some(path) => {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            (parse_compose_images(&text), parse_compose_jar_mounts(&text), None)
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
    let stopped = compose_down_for_backup(
        &state,
        &live,
        body.job_id.as_deref(),
        body.stop_compose,
    )
    .await?;
    let tree_clone = tree.clone();
    let jobs = state.jobs.clone();
    let job_id = body.job_id.clone();
    let live_for_snap = live.clone();
    job_set(&state, job_id.as_deref(), "snapshot", "正在建立基线快照…", 0, 0);
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
            compose_up_best_effort(&state, &live, body.job_id.as_deref(), "备份失败，正在重新启动 Compose…").await;
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
    db::update_project(&conn, &id, &name, &description, &directory, &db::now_rfc3339())?;
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
            return Err(AppError::Conflict("该目录属于已登记版本，请用「删除项目」".into()));
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
            return Err(AppError::Conflict("该目录属于已登记项目，请用「删除项目」".into()));
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
    let mut restart = true;
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
    job_set(&state, job_id.as_deref(), "snapshot", "正在备份当前目录…", 0, 0);
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
            compose_up_best_effort(&state, &live, job_id.as_deref(), "备份失败，正在重新启动 Compose…").await;
        }
        return Err(err);
    }

    if let Err(err) = tokio::fs::create_dir_all(&images_dir).await {
        let _ = remove_dir_if_exists(&version_dir);
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        if stopped {
            compose_up_best_effort(&state, &live, job_id.as_deref(), "发布失败，正在重新启动 Compose…").await;
        }
        return Err(err.into());
    }

    let (archives, jar_files): (Vec<PathBuf>, Vec<PathBuf>) = staged_files
        .into_iter()
        .partition(|p| {
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
    if let Err(err) = load_and_retag(&state, &archives, &images_dir, &mut loaded, job_id.as_deref()).await {
        job_err(&state, job_id.as_deref(), &err.to_string());
        let _ = remove_dir_if_exists(&version_dir);
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        if stopped {
            compose_up_best_effort(&state, &live, job_id.as_deref(), "发布失败，正在重新启动 Compose…").await;
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
    if let Err(err) = deploy_jars(&jar_files, &jars_dir, &live, &mounts, &mut deployed_jars).await {
        job_err(&state, job_id.as_deref(), &err.to_string());
        let _ = remove_dir_if_exists(&version_dir);
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        if stopped {
            compose_up_best_effort(&state, &live, job_id.as_deref(), "发布失败，正在重新启动 Compose…").await;
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
        job_set(&state, job_id.as_deref(), "compose", "正在重启 Compose…", 0, 0);
        if let Err(err) = restart_after_update(&state, &live, &deployed_jars, !archives.is_empty()).await
        {
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
    archive_dir: &std::path::Path,
    live: &std::path::Path,
    mounts: &[JarMount],
    deployed: &mut Vec<DeployedJar>,
) -> Result<(), AppError> {
    if jar_files.is_empty() {
        return Ok(());
    }
    tokio::fs::create_dir_all(archive_dir).await?;
    for src in jar_files {
        let name = src
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("app.jar")
            .to_string();
        tokio::fs::copy(src, archive_dir.join(&name)).await?;

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
    loaded_images: bool,
) -> anyhow::Result<String> {
    let mut services: Vec<String> = jars
        .iter()
        .flat_map(|j| j.services.iter().cloned())
        .collect();
    services.sort();
    services.dedup();
    if !services.is_empty() {
        let out = state.docker.compose_up_recreate(live, &services).await?;
        if loaded_images {
            let more = state.docker.compose_up(live).await?;
            return Ok(format!("{out}\n{more}"));
        }
        return Ok(out);
    }
    state.docker.compose_up(live).await
}

async fn load_and_retag(
    state: &AppState,
    staged_files: &[PathBuf],
    images_dir: &std::path::Path,
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
        let dest = images_dir.join(&name);
        tokio::fs::copy(src, &dest).await?;
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
    let safety_dir = state.paths.version_dir(&id, &safety_id);
    let live = PathBuf::from(&project.directory);
    let stopped = compose_down_for_backup(
        &state,
        &live,
        body.job_id.as_deref(),
        body.stop_compose,
    )
    .await?;
    let safety_tree_clone = safety_tree.clone();
    let live_clone = live.clone();
    let jobs = state.jobs.clone();
    let job_id = body.job_id.clone();
    job_set(&state, job_id.as_deref(), "snapshot", "正在备份当前目录…", 0, 0);
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
            },
        )?;
    }

    let snapshot = PathBuf::from(&target.backup_path);
    let live_restore = live.clone();
    job_set(&state, body.job_id.as_deref(), "restore", "正在解压备份…", 0, 0);
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
        job_set(&state, body.job_id.as_deref(), "compose", "正在重启 Compose…", 0, 0);
        let mounts = read_jar_mounts(&live);
        let mut services: Vec<String> = mounts.into_iter().map(|m| m.service).collect();
        services.sort();
        services.dedup();
        let result = if !services.is_empty() {
            state.docker.compose_up_recreate(&live, &services).await
        } else {
            state.docker.compose_up(&live).await
        };
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

    Ok(Json(ComposeStatus {
        available: state.docker.available,
        compose_file: compose_file.map(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }),
        images,
        jar_mounts,
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
    let service = match q.service.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => {
            crate::term::require_service_name(name)?;
            Some(name.to_string())
        }
        None => None,
    };
    let logs = state
        .docker
        .compose_logs(
            &PathBuf::from(project.directory),
            tail,
            service.as_deref(),
        )
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
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("assets/vendor/xterm.js"),
    )
}

async fn vendor_xterm_fit() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("assets/vendor/xterm-addon-fit.js"),
    )
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

    #[test]
    fn lists_failed_first_backup_dir() {
        let root = temp_root();
        fs::create_dir_all(root.join("leftover-id").join("repo.git")).unwrap();
        fs::write(root.join("leftover-id").join("repo.git").join("HEAD"), b"ref").unwrap();
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
        fs::write(root.join("proj").join("ghost-ver").join("tree.gitref"), b"dead").unwrap();
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
}
