use crate::auth;
use crate::db;
use crate::error::AppError;
use crate::models::{
    PortalBackground, PortalItem, PortalPage, ReorderPortalItems, SavePortal, SavePortalItem,
};
use crate::state::AppState;
use axum::extract::{Multipart, Path, State};
use axum::http::{header, HeaderMap};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use std::path::{Path as FsPath, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

const TITLE_MAX: usize = 64;
const SUBTITLE_MAX: usize = 160;
const NAME_MAX: usize = 64;
const SUMMARY_MAX: usize = 200;
const ICON_TEXT_MAX: usize = 16;
const URL_MAX: usize = 1024;
const ICON_MAX_BYTES: u64 = 2 * 1024 * 1024;
const BACKGROUND_MAX_BYTES: u64 = 200 * 1024 * 1024;
const DEFAULT_BACKGROUND_URL: &str = "/vendor/portal.jpg";
const DEFAULT_BACKGROUND_JPEG: &[u8] = include_bytes!("assets/vendor/portal.jpg");

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/vendor/portal.jpg", get(default_background))
        .route("/api/portal", get(get_portal).put(put_portal))
        .route(
            "/api/portal/background",
            post(upload_background).delete(delete_background),
        )
        .route("/api/portal/items", post(create_item))
        .route("/api/portal/items/reorder", post(reorder_items))
        .route(
            "/api/portal/items/{id}",
            axum::routing::put(update_item).delete(delete_item),
        )
        .route(
            "/api/portal/items/{id}/icon",
            post(upload_item_icon).delete(delete_item_icon),
        )
        .route("/media/portal/background", get(serve_background))
        .route("/media/portal/icon/{id}", get(serve_item_icon))
}

pub async fn page() -> impl IntoResponse {
    let html =
        include_str!("assets/portal.html").replace("__APP_VERSION__", env!("CARGO_PKG_VERSION"));
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html)
}

async fn get_portal(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<PortalPage>, AppError> {
    let auth_status = auth::auth_status(&state, &headers)?;
    let (title, subtitle, background, items) = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let title = db::portal_setting(&conn, "title")?
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| db::DEFAULT_PORTAL_TITLE.into());
        let subtitle = db::portal_setting(&conn, "subtitle")?.unwrap_or_default();
        let kind = db::portal_setting(&conn, "background_kind")?.unwrap_or_else(|| "none".into());
        let file = db::portal_setting(&conn, "background_file")?.unwrap_or_default();
        let updated = db::portal_setting(&conn, "background_updated")?.unwrap_or_default();
        let items = db::list_portal_items(&conn)?
            .into_iter()
            .filter(|it| !db::is_builtin_portal_url(&it.url))
            .collect();
        (
            title,
            subtitle,
            background_view(&kind, &file, &updated),
            items,
        )
    };
    Ok(Json(PortalPage {
        title,
        subtitle,
        background,
        items,
        auth: auth_status,
    }))
}

fn background_view(kind: &str, file: &str, updated: &str) -> PortalBackground {
    if kind == "none" || file.is_empty() {
        return PortalBackground {
            kind: "image".into(),
            url: Some(DEFAULT_BACKGROUND_URL.into()),
        };
    }
    let mut url = "/media/portal/background".to_string();
    if !updated.is_empty() {
        url.push_str("?v=");
        url.push_str(updated);
    }
    PortalBackground {
        kind: kind.to_string(),
        url: Some(url),
    }
}

async fn default_background() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        DEFAULT_BACKGROUND_JPEG,
    )
}

async fn put_portal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SavePortal>,
) -> Result<Json<PortalPage>, AppError> {
    if let Some(title) = body.title.as_ref() {
        let title = normalize_text(title, "页面标题", 1, TITLE_MAX)?;
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::set_portal_setting(&conn, "title", &title)?;
    }
    if let Some(subtitle) = body.subtitle.as_ref() {
        let subtitle = normalize_text(subtitle, "副标题", 0, SUBTITLE_MAX)?;
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::set_portal_setting(&conn, "subtitle", &subtitle)?;
    }
    get_portal(State(state), headers).await
}

async fn create_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SavePortalItem>,
) -> Result<Json<PortalPage>, AppError> {
    let name = normalize_text(body.name.as_deref().unwrap_or(""), "名称", 1, NAME_MAX)?;
    let url = validate_user_item_url(body.url.as_deref().unwrap_or(""))?;
    let summary = normalize_text(
        body.summary.as_deref().unwrap_or(""),
        "简介",
        0,
        SUMMARY_MAX,
    )?;
    let icon = normalize_text(body.icon.as_deref().unwrap_or(""), "图标", 0, ICON_TEXT_MAX)?;
    let now = db::now_rfc3339();
    let item = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let item = PortalItem {
            id: Uuid::new_v4().to_string(),
            icon,
            icon_file: String::new(),
            icon_url: None,
            name,
            summary,
            url,
            open_new: body.open_new.unwrap_or(false),
            sort_order: db::next_portal_sort(&conn)?,
            created_at: now.clone(),
            updated_at: now,
        };
        db::insert_portal_item(&conn, &item)?;
        item
    };
    tracing::info!(id = %item.id, name = %item.name, "portal item created");
    get_portal(State(state), headers).await
}

async fn update_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SavePortalItem>,
) -> Result<Json<PortalPage>, AppError> {
    let mut item = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_portal_item(&conn, &id)?.ok_or_else(|| AppError::not_found("入口不存在"))?
    };
    if let Some(name) = body.name {
        item.name = normalize_text(&name, "名称", 1, NAME_MAX)?;
    }
    if let Some(url) = body.url {
        item.url = validate_user_item_url(&url)?;
    }
    if let Some(summary) = body.summary {
        item.summary = normalize_text(&summary, "简介", 0, SUMMARY_MAX)?;
    }
    if let Some(icon) = body.icon {
        item.icon = normalize_text(&icon, "图标", 0, ICON_TEXT_MAX)?;
    }
    if let Some(open_new) = body.open_new {
        item.open_new = open_new;
    }
    item.updated_at = db::now_rfc3339();
    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::update_portal_item(&conn, &item)?;
    }
    get_portal(State(state), headers).await
}

async fn delete_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<PortalPage>, AppError> {
    let item = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let item =
            db::get_portal_item(&conn, &id)?.ok_or_else(|| AppError::not_found("入口不存在"))?;
        if !db::delete_portal_item(&conn, &id)? {
            return Err(AppError::not_found("入口不存在"));
        }
        item
    };
    remove_icon_file(&state, &item);
    get_portal(State(state), headers).await
}

async fn reorder_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ReorderPortalItems>,
) -> Result<Json<PortalPage>, AppError> {
    if body.ids.is_empty() {
        return Err(AppError::bad("排序列表不能为空"));
    }
    if body.ids.len() > 200 {
        return Err(AppError::bad("入口数量过多"));
    }
    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let existing = db::list_portal_items(&conn)?;
        if existing.len() != body.ids.len()
            || !existing
                .iter()
                .all(|it| body.ids.iter().any(|id| id == &it.id))
        {
            return Err(AppError::bad("排序列表与现有入口不一致"));
        }
        db::reorder_portal_items(&conn, &body.ids)?;
    }
    get_portal(State(state), headers).await
}

async fn upload_background(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Json<PortalPage>, AppError> {
    let saved = receive_media(&state, multipart, BACKGROUND_MAX_BYTES, false).await?;
    let dest = state
        .paths
        .portal_dir
        .join(format!("background.{}", saved.ext));
    tokio::fs::rename(&saved.path, &dest).await?;
    if let Some(parent) = saved.path.parent() {
        let _ = tokio::fs::remove_dir_all(parent).await;
    }
    let old = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let old = db::portal_setting(&conn, "background_file")?.unwrap_or_default();
        db::set_portal_setting(&conn, "background_kind", saved.kind)?;
        db::set_portal_setting(
            &conn,
            "background_file",
            dest.file_name().and_then(|s| s.to_str()).unwrap_or(""),
        )?;
        db::set_portal_setting(&conn, "background_updated", &db::now_rfc3339())?;
        old
    };
    if !old.is_empty() && old != dest.file_name().and_then(|s| s.to_str()).unwrap_or("") {
        let _ = tokio::fs::remove_file(state.paths.portal_dir.join(old)).await;
    }
    get_portal(State(state), headers).await
}

async fn delete_background(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<PortalPage>, AppError> {
    let old = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let old = db::portal_setting(&conn, "background_file")?.unwrap_or_default();
        db::set_portal_setting(&conn, "background_kind", "none")?;
        db::set_portal_setting(&conn, "background_file", "")?;
        db::set_portal_setting(&conn, "background_updated", &db::now_rfc3339())?;
        old
    };
    if !old.is_empty() {
        let _ = tokio::fs::remove_file(state.paths.portal_dir.join(old)).await;
    }
    get_portal(State(state), headers).await
}

async fn upload_item_icon(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    multipart: Multipart,
) -> Result<Json<PortalPage>, AppError> {
    let mut item = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_portal_item(&conn, &id)?.ok_or_else(|| AppError::not_found("入口不存在"))?
    };
    let saved = receive_media(&state, multipart, ICON_MAX_BYTES, true).await?;
    let dest = state
        .paths
        .portal_icons_dir()
        .join(format!("{id}.{}", saved.ext));
    tokio::fs::rename(&saved.path, &dest).await?;
    if let Some(parent) = saved.path.parent() {
        let _ = tokio::fs::remove_dir_all(parent).await;
    }
    let old_file = item.icon_file.clone();
    item.icon_file = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    item.updated_at = db::now_rfc3339();
    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::update_portal_item(&conn, &item)?;
    }
    if !old_file.is_empty() && old_file != item.icon_file {
        let _ = tokio::fs::remove_file(state.paths.portal_icons_dir().join(old_file)).await;
    }
    get_portal(State(state), headers).await
}

async fn delete_item_icon(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<PortalPage>, AppError> {
    let mut item = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_portal_item(&conn, &id)?.ok_or_else(|| AppError::not_found("入口不存在"))?
    };
    let old = item.icon_file.clone();
    item.icon_file.clear();
    item.updated_at = db::now_rfc3339();
    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::update_portal_item(&conn, &item)?;
    }
    if !old.is_empty() {
        let _ = tokio::fs::remove_file(state.paths.portal_icons_dir().join(old)).await;
    }
    get_portal(State(state), headers).await
}

async fn serve_background(State(state): State<AppState>) -> Result<impl IntoResponse, AppError> {
    let (kind, file) = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let kind = db::portal_setting(&conn, "background_kind")?.unwrap_or_else(|| "none".into());
        let file = db::portal_setting(&conn, "background_file")?.unwrap_or_default();
        (kind, file)
    };
    if kind == "none" || file.is_empty() {
        return Err(AppError::not_found("尚未设置背景"));
    }
    if !safe_media_name(&file) {
        return Err(AppError::not_found("背景文件无效"));
    }
    serve_disk_file(state.paths.portal_dir.join(file)).await
}

async fn serve_item_icon(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let item = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        db::get_portal_item(&conn, &id)?.ok_or_else(|| AppError::not_found("入口不存在"))?
    };
    if item.icon_file.is_empty() || !safe_media_name(&item.icon_file) {
        return Err(AppError::not_found("该入口没有图标文件"));
    }
    serve_disk_file(state.paths.portal_icons_dir().join(item.icon_file)).await
}

async fn serve_disk_file(path: PathBuf) -> Result<impl IntoResponse, AppError> {
    let data = tokio::fs::read(&path)
        .await
        .map_err(|_| AppError::not_found("文件不存在"))?;
    let ctype = content_type_for(&path);
    Ok((
        [
            (header::CONTENT_TYPE, ctype),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        data,
    ))
}

fn remove_icon_file(state: &AppState, item: &PortalItem) {
    if item.icon_file.is_empty() || !safe_media_name(&item.icon_file) {
        return;
    }
    let path = state.paths.portal_icons_dir().join(&item.icon_file);
    let _ = std::fs::remove_file(path);
}

struct SavedMedia {
    path: PathBuf,
    ext: &'static str,
    kind: &'static str,
}

async fn receive_media(
    state: &AppState,
    mut multipart: Multipart,
    max_bytes: u64,
    image_only: bool,
) -> Result<SavedMedia, AppError> {
    let tmp_dir = state.paths.uploads_dir.join(Uuid::new_v4().to_string());
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let result = async {
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|e| AppError::bad(format!("无效的上传数据：{e}")))?
        {
            let name = field.name().unwrap_or("").to_string();
            let filename = field.file_name().unwrap_or("").to_string();
            if name != "file" && filename.is_empty() {
                continue;
            }
            let dest = tmp_dir.join("upload.bin");
            let mut file = tokio::fs::File::create(&dest).await?;
            let mut written = 0u64;
            let mut field = field;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|e| AppError::bad(format!("上传中断：{e}")))?
            {
                written += chunk.len() as u64;
                if written > max_bytes {
                    return Err(AppError::bad(format!(
                        "文件过大，不能超过 {}",
                        if max_bytes >= 1024 * 1024 {
                            format!("{} MB", max_bytes / 1024 / 1024)
                        } else {
                            format!("{max_bytes} B")
                        }
                    )));
                }
                file.write_all(&chunk).await?;
            }
            file.flush().await?;
            drop(file);
            if written == 0 {
                return Err(AppError::bad("请选择要上传的文件"));
            }
            let mut head = [0u8; 16];
            let mut f = tokio::fs::File::open(&dest).await?;
            let n = f.read(&mut head).await?;
            let sniffed = sniff_media(&head[..n]).ok_or_else(|| {
                AppError::bad(if image_only {
                    "图标只支持 jpg / png / webp / gif"
                } else {
                    "背景只支持 jpg / png / webp / gif 图片或 mp4 / webm 视频"
                })
            })?;
            if image_only && sniffed.kind != "image" {
                return Err(AppError::bad("图标只支持图片文件"));
            }
            return Ok(SavedMedia {
                path: dest,
                ext: sniffed.ext,
                kind: sniffed.kind,
            });
        }
        Err(AppError::bad("请选择要上传的文件"))
    }
    .await;

    match result {
        Ok(saved) => Ok(saved),
        Err(err) => {
            let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
            Err(err)
        }
    }
}

struct MediaType {
    ext: &'static str,
    kind: &'static str,
}

fn sniff_media(head: &[u8]) -> Option<MediaType> {
    if head.len() >= 3 && head[0] == 0xff && head[1] == 0xd8 && head[2] == 0xff {
        return Some(MediaType {
            ext: "jpg",
            kind: "image",
        });
    }
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(MediaType {
            ext: "png",
            kind: "image",
        });
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some(MediaType {
            ext: "gif",
            kind: "image",
        });
    }
    if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        return Some(MediaType {
            ext: "webp",
            kind: "image",
        });
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        return Some(MediaType {
            ext: "mp4",
            kind: "video",
        });
    }
    if head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Some(MediaType {
            ext: "webm",
            kind: "video",
        });
    }
    None
}

fn content_type_for(path: &FsPath) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}

fn safe_media_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

fn validate_user_item_url(raw: &str) -> Result<String, AppError> {
    let url = validate_url(raw)?;
    if db::is_builtin_portal_url(&url) {
        return Err(AppError::bad("系统管理已固定在首页底部，不必再添加"));
    }
    Ok(url)
}

pub fn validate_url(raw: &str) -> Result<String, AppError> {
    let url = raw.trim();
    if url.is_empty() {
        return Err(AppError::bad("请填写跳转地址"));
    }
    if url.len() > URL_MAX {
        return Err(AppError::bad("跳转地址过长"));
    }
    if url.contains('\0') || url.chars().any(|c| c.is_control()) {
        return Err(AppError::bad("跳转地址包含非法字符"));
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("javascript:")
        || lower.starts_with("data:")
        || lower.starts_with("vbscript:")
        || lower.starts_with("file:")
    {
        return Err(AppError::bad("不支持该类型的地址"));
    }
    if url.starts_with('/') {
        if url.starts_with("//") {
            return Err(AppError::bad("相对地址不能以 // 开头"));
        }
        return Ok(url.to_string());
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(url.to_string());
    }
    Err(AppError::bad("地址需以 http://、https:// 或 / 开头"))
}

fn normalize_text(raw: &str, label: &str, min: usize, max: usize) -> Result<String, AppError> {
    let text = raw.trim();
    let chars = text.chars().count();
    if chars < min {
        return Err(AppError::bad(format!("{label}不能为空")));
    }
    if chars > max {
        return Err(AppError::bad(format!("{label}不能超过 {max} 个字符")));
    }
    if text.contains('\0') {
        return Err(AppError::bad(format!("{label}包含非法字符")));
    }
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_accepts_http_and_relative() {
        assert_eq!(validate_url(" /console ").unwrap(), "/console");
        assert!(validate_user_item_url("/console").is_err());
        assert_eq!(
            validate_url("https://example.com/a").unwrap(),
            "https://example.com/a"
        );
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("data:text/html,x").is_err());
        assert!(validate_url("//evil").is_err());
        assert!(validate_url("ftp://x").is_err());
        assert!(validate_url("").is_err());
    }

    #[test]
    fn sniff_common_media() {
        assert_eq!(sniff_media(&[0xff, 0xd8, 0xff, 0xe0]).unwrap().ext, "jpg");
        assert_eq!(sniff_media(b"\x89PNG\r\n\x1a\nxxxx").unwrap().ext, "png");
        assert_eq!(sniff_media(b"GIF89a........").unwrap().kind, "image");
        let mut webp = [0u8; 12];
        webp[..4].copy_from_slice(b"RIFF");
        webp[8..12].copy_from_slice(b"WEBP");
        assert_eq!(sniff_media(&webp).unwrap().ext, "webp");
        let mut mp4 = [0u8; 12];
        mp4[4..8].copy_from_slice(b"ftyp");
        assert_eq!(sniff_media(&mp4).unwrap().kind, "video");
        assert!(sniff_media(b"not-a-media").is_none());
    }

    #[test]
    fn default_background_when_none() {
        let bg = background_view("none", "", "");
        assert_eq!(bg.kind, "image");
        assert_eq!(bg.url.as_deref(), Some(DEFAULT_BACKGROUND_URL));
        let custom = background_view("video", "background.mp4", "v1");
        assert_eq!(custom.kind, "video");
        assert_eq!(custom.url.as_deref(), Some("/media/portal/background?v=v1"));
        assert!(!DEFAULT_BACKGROUND_JPEG.is_empty());
        assert_eq!(&DEFAULT_BACKGROUND_JPEG[..3], &[0xff, 0xd8, 0xff]);
    }

    #[test]
    fn media_name_rejects_traversal() {
        assert!(safe_media_name("background.mp4"));
        assert!(safe_media_name("abc-1.png"));
        assert!(!safe_media_name("../x.png"));
        assert!(!safe_media_name("a/b.png"));
        assert!(!safe_media_name(""));
    }
}
