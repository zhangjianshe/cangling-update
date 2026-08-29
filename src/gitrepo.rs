//! Git 软件仓库：把 cangling-repo 克隆到程序目录下的 `repo/`，并提供目录/文件浏览与更新。
//!
//! - 克隆目标与「软件仓库」扫描的目录一致（`<程序目录>/repo`），克隆后即可被离线安装使用。
//! - 仓库地址可用环境变量覆盖：`CANGLING_REPO_URL`；HTTPS 私有仓库可配
//!   `CANGLING_REPO_USERNAME` / `CANGLING_REPO_PASSWORD`（或 TOKEN）。

use crate::error::AppError;
use crate::paths::AppPaths;
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

pub const DEFAULT_REPO_URL: &str = "https://code.cangling.cn:22002/operation/cangling-repo.git";
/// 内置默认凭据（只读部署令牌）：用户名 / Token。可用环境变量覆盖。
pub const DEFAULT_REPO_USERNAME: &str = "cangling-update";
pub const DEFAULT_REPO_PASSWORD: &str = "2ab3f3968f50ea8650009f8f8f6f8fde20d8158a";
/// 文件内容超过该字节数时只返回头部，避免把大文件整个塞进响应。
const MAX_TEXT_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Serialize, Default)]
pub struct GitRepoStatus {
    pub exists: bool,
    pub is_git: bool,
    pub git_available: bool,
    pub root: String,
    pub branch: String,
    pub remote: String,
    pub last_commit: String,
    pub last_commit_time: String,
    pub subject: String,
    pub dirty: bool,
}

#[derive(Debug, Serialize)]
pub struct GitOpResult {
    pub ok: bool,
    pub output: String,
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
pub struct CloneBody {
    pub url: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct PathQuery {
    pub path: Option<String>,
}

fn repo_dir(paths: &AppPaths) -> PathBuf {
    crate::repo::repo_root(paths)
}

fn env_repo_url() -> String {
    std::env::var("CANGLING_REPO_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_REPO_URL.to_string())
}

fn env_username() -> String {
    std::env::var("CANGLING_REPO_USERNAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_REPO_USERNAME.to_string())
}

fn env_password() -> String {
    std::env::var("CANGLING_REPO_PASSWORD")
        .or_else(|_| std::env::var("CANGLING_REPO_TOKEN"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_REPO_PASSWORD.to_string())
}

/// 本机是否安装了 git 命令。
fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 把用户名/密码（或 token）嵌入 HTTPS/HTTP 地址，供克隆与后续 pull 使用。
fn auth_url(base: &str, username: Option<&str>, password: Option<&str>) -> String {
    let (u, p) = match (username, password) {
        (Some(u), Some(p)) => (u.trim(), p.trim()),
        _ => return base.to_string(),
    };
    if u.is_empty() {
        return base.to_string();
    }
    let rest = base
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"));
    let Some(rest) = rest else {
        return base.to_string();
    };
    let scheme = if base.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://{}:{}@{}", url_encode(u), url_encode(p), rest)
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 从 URL 中去掉 `user:pass@`，避免在界面上泄露凭据。
fn strip_credentials(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    let Some(rest) = rest else {
        return url.to_string();
    };
    if let Some(at) = rest.find('@') {
        let scheme = if url.starts_with("https://") {
            "https"
        } else {
            "http"
        };
        format!("{scheme}://{}", &rest[at + 1..])
    } else {
        url.to_string()
    }
}

/// 网络类 git 命令（clone/pull）用非交互 SSH，避免卡在主机密钥/密码提示。
fn git_net_command() -> Command {
    let mut c = Command::new("git");
    c.env(
        "GIT_SSH_COMMAND",
        "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
    );
    c
}

fn run_git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("无法运行 git：{e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        let msg = if stderr.is_empty() {
            stdout.clone()
        } else {
            stderr
        };
        return Err(msg);
    }
    Ok(stdout)
}

fn status_sync(paths: &AppPaths) -> GitRepoStatus {
    let dir = repo_dir(paths);
    let exists = dir.exists();
    let is_git = dir.join(".git").exists();
    let mut s = GitRepoStatus {
        exists,
        is_git,
        git_available: git_available(),
        root: dir.display().to_string(),
        ..Default::default()
    };
    if is_git {
        s.branch = run_git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
        s.remote = run_git(&dir, &["remote", "get-url", "origin"])
            .map(|u| strip_credentials(&u))
            .unwrap_or_default();
        s.last_commit = run_git(&dir, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
        s.last_commit_time = run_git(&dir, &["log", "-1", "--format=%ci"]).unwrap_or_default();
        s.subject = run_git(&dir, &["log", "-1", "--format=%s"]).unwrap_or_default();
        s.dirty = !run_git(&dir, &["status", "--porcelain"])
            .unwrap_or_default()
            .is_empty();
    }
    s
}

fn clone_sync(paths: &AppPaths, body: &CloneBody) -> Result<String, String> {
    let dir = repo_dir(paths);
    if dir.join(".git").exists() {
        return Err("仓库已存在，如需更新请点「更新」".to_string());
    }
    if dir.exists() {
        let non_empty = std::fs::read_dir(&dir)
            .map(|mut it| it.next().is_some())
            .unwrap_or(false);
        if non_empty {
            return Err(format!("目录 {} 已存在且非空，无法克隆", dir.display()));
        }
    } else if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(format!("创建目录失败：{e}"));
    }

    let url = body
        .url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(env_repo_url);
    let username_env = env_username();
    let password_env = env_password();
    let username = body
        .username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(username_env.as_str());
    let password = body
        .password
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(password_env.as_str());
    let url = auth_url(&url, Some(username), Some(password));

    let out = git_net_command()
        .arg("clone")
        .arg(&url)
        .arg(&dir)
        .output()
        .map_err(|e| format!("无法运行 git clone：{e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        let msg = if stderr.is_empty() { stdout } else { stderr };
        return Err(msg);
    }
    Ok(format!("{stdout}\n{stderr}").trim().to_string())
}

fn pull_sync(paths: &AppPaths) -> Result<String, String> {
    let dir = repo_dir(paths);
    if !dir.join(".git").exists() {
        return Err("尚未克隆仓库，请先点「克隆」".to_string());
    }
    let out = git_net_command()
        .arg("-C")
        .arg(&dir)
        .arg("pull")
        .arg("--ff-only")
        .output()
        .map_err(|e| format!("无法运行 git pull：{e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        let msg = if stderr.is_empty() { stdout } else { stderr };
        return Err(msg);
    }
    let combined = format!("{stdout}\n{stderr}").trim().to_string();
    Ok(if combined.is_empty() {
        "已是最新版本".to_string()
    } else {
        combined
    })
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
    let dir = resolve_rel(&repo_dir(paths), rel)?;
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
    let path = resolve_rel(&repo_dir(paths), rel)?;
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

pub async fn clone_repo(
    State(state): State<AppState>,
    Json(body): Json<CloneBody>,
) -> Result<Json<GitOpResult>, AppError> {
    let paths = state.paths.clone();
    let body = CloneBody {
        url: body.url,
        username: body.username,
        password: body.password,
    };
    tokio::task::spawn_blocking(move || match clone_sync(&paths, &body) {
        Ok(output) => GitOpResult { ok: true, output },
        Err(e) => GitOpResult {
            ok: false,
            output: e,
        },
    })
    .await
    .map(Json)
    .map_err(|e| AppError::internal(e.to_string()))
}

pub async fn pull(State(state): State<AppState>) -> Result<Json<GitOpResult>, AppError> {
    let paths = state.paths.clone();
    tokio::task::spawn_blocking(move || match pull_sync(&paths) {
        Ok(output) => GitOpResult { ok: true, output },
        Err(e) => GitOpResult {
            ok: false,
            output: e,
        },
    })
    .await
    .map(Json)
    .map_err(|e| AppError::internal(e.to_string()))
}

pub async fn tree(State(state): State<AppState>) -> Result<Json<Vec<String>>, AppError> {
    let paths = state.paths.clone();
    tokio::task::spawn_blocking(move || {
        let root = repo_dir(&paths);
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
    fn auth_url_embeds_credentials() {
        assert_eq!(
            auth_url("https://code.cangling.cn:22002/op/repo.git", None, None),
            "https://code.cangling.cn:22002/op/repo.git"
        );
        assert_eq!(
            auth_url(
                "https://code.cangling.cn:22002/op/repo.git",
                Some("admin"),
                Some("p@ss word")
            ),
            "https://admin:p%40ss%20word@code.cangling.cn:22002/op/repo.git"
        );
        assert_eq!(
            auth_url("http://host/repo.git", Some("u"), Some("p")),
            "http://u:p@host/repo.git"
        );
    }

    #[test]
    fn strip_credentials_masks_userinfo() {
        assert_eq!(
            strip_credentials("https://admin:p%40ss@code.cangling.cn:22002/op/repo.git"),
            "https://code.cangling.cn:22002/op/repo.git"
        );
        assert_eq!(
            strip_credentials("https://code.cangling.cn:22002/op/repo.git"),
            "https://code.cangling.cn:22002/op/repo.git"
        );
    }

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
