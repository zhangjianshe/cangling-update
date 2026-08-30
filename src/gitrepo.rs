//! 浏览 keeper 同步到 `<程序目录>/repo/` 的软件仓库（含 `cangling-repo/` 与 `np4/`）。
//!
//! 仓库内容由维护中心「软件同步」写入，本程序不再提供克隆 / 拉取。

use crate::error::AppError;
use crate::paths::AppPaths;
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// 文件内容超过该字节数时只返回头部，避免把大文件整个塞进响应。
const MAX_TEXT_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Serialize, Default)]
pub struct GitRepoStatus {
    pub exists: bool,
    pub root: String,
}

#[derive(Debug, Serialize)]
pub struct RepoEntry {
    pub name: String,
    pub path: String,
    pub kind: String, // "dir" | "file"
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct RepoFileView {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub binary: bool,
    pub truncated: bool,
    pub content: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct PathQuery {
    pub path: Option<String>,
}

fn browse_root(paths: &AppPaths) -> PathBuf {
    crate::repo::repo_root(paths)
}

fn dir_nonempty(dir: &Path) -> bool {
    dir.is_dir()
        && std::fs::read_dir(dir)
            .map(|mut it| it.next().is_some())
            .unwrap_or(false)
}

fn status_sync(paths: &AppPaths) -> GitRepoStatus {
    let browse = browse_root(paths);
    GitRepoStatus {
        exists: dir_nonempty(&browse),
        root: browse.display().to_string(),
    }
}

/// 解析相对路径到仓库内的绝对路径，拒绝 `..` / 绝对路径 / 越界。
fn resolve_rel(root: &Path, rel: &str) -> Result<PathBuf, AppError> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Ok(root.to_path_buf());
    }
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(AppError::bad("路径不能是绝对路径"));
    }
    let mut out = root.to_path_buf();
    for c in p.components() {
        match c {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            _ => return Err(AppError::bad("路径非法")),
        }
    }
    if !out.starts_with(root) {
        return Err(AppError::bad("路径越界"));
    }
    Ok(out)
}

fn collect_dirs(base: &Path, rel: &Path, out: &mut Vec<String>) {
    let dir = if rel.as_os_str().is_empty() {
        base.to_path_buf()
    } else {
        base.join(rel)
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for e in entries.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let child = rel.join(&name);
        out.push(child.to_string_lossy().into_owned());
        collect_dirs(base, &child, out);
    }
}

fn list_sync(paths: &AppPaths, rel: &str) -> Result<Vec<RepoEntry>, AppError> {
    let dir = resolve_rel(&browse_root(paths), rel)?;
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir).map_err(AppError::from)? {
        let e = e.map_err(AppError::from)?;
        let ft = e.file_type().map_err(AppError::from)?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let kind = if ft.is_dir() { "dir" } else { "file" };
        let size = if kind == "file" {
            e.metadata().map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        let path = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        out.push(RepoEntry {
            name,
            path,
            kind: kind.to_string(),
            size,
        });
    }
    out.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

fn file_sync(paths: &AppPaths, rel: &str) -> Result<RepoFileView, AppError> {
    let path = resolve_rel(&browse_root(paths), rel)?;
    let meta = std::fs::metadata(&path).map_err(AppError::from)?;
    if !meta.is_file() {
        return Err(AppError::bad("不是文件"));
    }
    let size = meta.len();
    let bytes = std::fs::read(&path).map_err(AppError::from)?;
    let binary = bytes.iter().take(8000).any(|&b| b == 0);
    let (content, truncated) = if binary {
        (String::new(), false)
    } else if bytes.len() > MAX_TEXT_BYTES {
        (
            String::from_utf8_lossy(&bytes[..MAX_TEXT_BYTES]).into_owned(),
            true,
        )
    } else {
        (String::from_utf8_lossy(&bytes).into_owned(), false)
    };
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(RepoFileView {
        path: rel.to_string(),
        name,
        size,
        binary,
        truncated,
        content,
    })
}

pub async fn status(State(state): State<AppState>) -> Json<GitRepoStatus> {
    let paths = state.paths.clone();
    let s = tokio::task::spawn_blocking(move || status_sync(&paths))
        .await
        .unwrap_or_default();
    Json(s)
}

pub async fn tree(State(state): State<AppState>) -> Result<Json<Vec<String>>, AppError> {
    let paths = state.paths.clone();
    tokio::task::spawn_blocking(move || {
        let root = browse_root(&paths);
        let mut out = Vec::new();
        collect_dirs(&root, Path::new(""), &mut out);
        out.sort();
        Ok(Json(out))
    })
    .await
    .map_err(|e| AppError::internal(e.to_string()))?
}

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Vec<RepoEntry>>, AppError> {
    let paths = state.paths.clone();
    let rel = q.path.unwrap_or_default();
    tokio::task::spawn_blocking(move || list_sync(&paths, &rel).map(Json))
        .await
        .map_err(|e| AppError::internal(e.to_string()))?
}

pub async fn file(
    State(state): State<AppState>,
    Query(q): Query<PathQuery>,
) -> Result<Json<RepoFileView>, AppError> {
    let paths = state.paths.clone();
    let rel = q.path.unwrap_or_default();
    tokio::task::spawn_blocking(move || file_sync(&paths, &rel).map(Json))
        .await
        .map_err(|e| AppError::internal(e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rel_rejects_traversal() {
        let root = Path::new("/tmp/repo");
        assert_eq!(resolve_rel(root, "").unwrap(), root);
        assert_eq!(
            resolve_rel(root, "kylin-arm/git").unwrap(),
            root.join("kylin-arm/git")
        );
        assert!(resolve_rel(root, "/etc").is_err());
        assert!(resolve_rel(root, "../evil").is_err());
        assert!(resolve_rel(root, "a/../../b").is_err());
    }
}
