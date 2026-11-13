//! 安装资源：解析 cangling-update 可执行文件同目录下的 `cangling-repo/`。
//!
//! 布局与 cangling-keeper「软件同步」一致（按软件集分子目录）：
//! ```text
//! <cangling-update 所在目录>/cangling-repo/
//!   platforms/kylin/x86/v10/rpm/<软件包>/
//!   platforms/kylin/x86/v10/deb/<软件包>/
//!   platforms/kylin/x86/v11/rpm/<软件包>/
//!   platforms/kylin/arm/v10/rpm/<软件包>/
//!   platforms/kylin/arm/v11/rpm/<软件包>/
//!   platforms/ubuntu/x86/22.04/deb/<软件包>/
//!   platforms/ubuntu/x86/24.04/deb/<软件包>/
//!   np4/                   # 维护中心 Manifest 集
//!     np4-update/latest/   # cangling-update 自我更新二进制
//! ```
//!
//! master 本地扫描仓库并对外提供打包下载（m2m 接口），worker 通过 master 拉取
//! 仓库清单、下载软件包；有安装脚本时也可在本机运行。

use crate::cluster::Role;
use crate::error::AppError;
use crate::paths::AppPaths;
use crate::state::AppState;
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::{Path as FsPath, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

struct Platform {
    id: &'static str,
    name: &'static str,
    directory: &'static str,
}

/// API 使用不含斜杠的稳定 ID，磁盘按发行版、架构、版本和包格式分层。
const PLATFORMS: &[Platform] = &[
    Platform {
        id: "kylin-x86-v10-rpm",
        name: "Kylin / x86 / V10 (RPM)",
        directory: "platforms/kylin/x86/v10/rpm",
    },
    Platform {
        id: "kylin-x86-v10-deb",
        name: "Kylin / x86 / V10 (DEB)",
        directory: "platforms/kylin/x86/v10/deb",
    },
    Platform {
        id: "kylin-x86-v11-rpm",
        name: "Kylin / x86 / V11 (RPM)",
        directory: "platforms/kylin/x86/v11/rpm",
    },
    Platform {
        id: "kylin-arm-v10-rpm",
        name: "Kylin / ARM / V10 (RPM)",
        directory: "platforms/kylin/arm/v10/rpm",
    },
    Platform {
        id: "kylin-arm-v11-rpm",
        name: "Kylin / ARM / V11 (RPM)",
        directory: "platforms/kylin/arm/v11/rpm",
    },
    Platform {
        id: "ubuntu-x86-22.04-deb",
        name: "Ubuntu / x86 / 22.04",
        directory: "platforms/ubuntu/x86/22.04/deb",
    },
    Platform {
        id: "ubuntu-x86-24.04-deb",
        name: "Ubuntu / x86 / 24.04",
        directory: "platforms/ubuntu/x86/24.04/deb",
    },
    Platform {
        id: "windows-current",
        name: "Windows",
        directory: "platforms/windows/current",
    },
];

/// keeper 同步的 Git 离线安装集（原 repo-templates）。
pub const CANGLING_REPO_SET: &str = "cangling-repo";
/// keeper 同步的维护中心 Manifest 集。
pub const NP4_SET: &str = "np4";
/// np4 集里的自我更新软件。
pub const NP4_UPDATE: &str = "np4-update";
pub const NP4_UPDATE_VERSION: &str = "latest";
/// 初始化/检查集群及安装资源浏览共用的仓库目录名。
pub const INSTALL_RESOURCES_DIR: &str = "cangling-repo";

/// 安装脚本执行超时（安装可能比普通脚本更久）。
const RUN_TIMEOUT: Duration = Duration::from_secs(600);
/// 按优先级识别的安装脚本文件名（不区分大小写）。
const INSTALLER_NAMES: &[&str] = &[
    "install.sh",
    "install.bat",
    "install.ps1",
    "setup.sh",
    "setup.bat",
    "setup.ps1",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoPackage {
    pub name: String,
    pub description: String,
    /// 包内相对路径（已排序）。
    pub files: Vec<String>,
    pub file_count: usize,
    pub bytes: u64,
    /// 安装脚本的相对路径（识别不到则为 None）。
    pub install: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoTab {
    pub id: String,
    pub name: String,
    pub packages: Vec<RepoPackage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIndex {
    pub root: String,
    pub exists: bool,
    /// worker 拉取 master 仓库时为 true。
    pub remote: bool,
    pub master: Option<String>,
    /// 本机平台（系统包格式 + CPU 架构）。
    pub host_platform: String,
    pub tabs: Vec<RepoTab>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstallResult {
    pub package: String,
    pub installer: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub elapsed_ms: u64,
}

/// 本机所属平台。优先使用 `/etc/os-release` 判断 RPM / Debian 系，
/// 在发行版信息不足时再根据本机可用的包管理器兜底。
pub fn host_platform() -> &'static str {
    let mut os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    for release_file in ["/etc/kylin-release", "/etc/redhat-release"] {
        if let Ok(value) = std::fs::read_to_string(release_file) {
            os_release.push('\n');
            os_release.push_str(&value);
        }
    }
    let kernel_release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if std::path::Path::new("/etc/kylin-release").is_file()
        || kernel_release.contains(".ky10")
        || kernel_release.contains("kylin")
    {
        os_release.push_str("\nID=kylin\n");
    }
    detect_host_platform(
        std::env::consts::ARCH,
        &os_release,
        command_available("dpkg"),
        command_available("rpm"),
    )
}

fn detect_host_platform(
    arch: &str,
    os_release: &str,
    has_dpkg: bool,
    has_rpm: bool,
) -> &'static str {
    let arm = matches!(arch, "aarch64" | "arm" | "arm64");
    let distro = os_release.to_ascii_lowercase();
    let rpm_family = os_release_has_any(
        &distro,
        &[
            "kylin",
            "opencloudos",
            "openeuler",
            "centos",
            "rhel",
            "fedora",
            "rocky",
            "almalinux",
            "anolis",
            "alinux",
            "suse",
        ],
    ) || (has_rpm && !has_dpkg);

    let version = os_release_value(&distro, "version_id").unwrap_or_default();
    let kylin_v11 = version.starts_with("11") || distro.contains("ky11") || distro.contains("v11");
    let kylin_v10 = version.starts_with("10") || distro.contains("ky10") || distro.contains("v10");
    let is_kylin = os_release_has_any(&distro, &["kylin"])
        || distro.contains("kylin")
        || distro.contains(".ky10")
        || distro.contains(".ky11");
    if is_kylin {
        return match (arm, kylin_v11, rpm_family) {
            (true, true, _) => "kylin-arm-v11-rpm",
            (true, false, _) => "kylin-arm-v10-rpm",
            (false, true, _) => "kylin-x86-v11-rpm",
            (false, false, false) if kylin_v10 => "kylin-x86-v10-deb",
            (false, false, _) => "kylin-x86-v10-rpm",
        };
    }
    if !arm && os_release_has_any(&distro, &["ubuntu"]) {
        if version.starts_with("22.04") {
            return "ubuntu-x86-22.04-deb";
        }
        if version.starts_with("24.04") {
            return "ubuntu-x86-24.04-deb";
        }
    }
    "unsupported"
}

fn os_release_value<'a>(os_release: &'a str, wanted: &str) -> Option<&'a str> {
    os_release.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        key.eq_ignore_ascii_case(wanted)
            .then(|| value.trim_matches(|character| matches!(character, '"' | '\'')))
    })
}

fn os_release_has_any(os_release: &str, ids: &[&str]) -> bool {
    os_release.lines().any(|line| {
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        if key != "id" && key != "id_like" {
            return false;
        }
        let value = value.trim_matches(|c| c == '"' || c == '\'');
        value
            .split(|c: char| c.is_ascii_whitespace() || c == ',')
            .any(|token| ids.contains(&token))
    })
}

fn command_available(program: &str) -> bool {
    const SYSTEM_PATHS: &[&str] = &[
        "/usr/local/sbin",
        "/usr/local/bin",
        "/usr/sbin",
        "/usr/bin",
        "/sbin",
        "/bin",
    ];
    SYSTEM_PATHS
        .iter()
        .any(|dir| FsPath::new(dir).join(program).is_file())
        || std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

fn platform_name(id: &str) -> &str {
    PLATFORMS
        .iter()
        .find_map(|platform| (platform.id == id).then_some(platform.name))
        .unwrap_or(id)
}

fn ensure_install_platform(tab: &str) -> Result<(), AppError> {
    let current = host_platform();
    if PLATFORMS.iter().any(|platform| platform.id == tab) && tab != current {
        return Err(AppError::bad(format!(
            "软件包平台“{}”与当前系统“{}”不匹配；请把对应架构和包格式的软件包同步到 {current}/ 目录",
            platform_name(tab),
            platform_name(current)
        )));
    }
    Ok(())
}

pub fn repo_root(paths: &AppPaths) -> PathBuf {
    paths.exe_dir.join(INSTALL_RESOURCES_DIR)
}

pub fn packages_root(repo: &FsPath) -> PathBuf {
    repo.to_path_buf()
}

/// `repo/np4/np4-update/latest`
pub fn np4_update_latest_dir(exe_dir: &FsPath) -> PathBuf {
    exe_dir
        .join("repo")
        .join(NP4_SET)
        .join(NP4_UPDATE)
        .join(NP4_UPDATE_VERSION)
}

pub async fn install_package(
    state: &AppState,
    tab: &str,
    package: &str,
    envs: &[(String, String)],
) -> Result<InstallResult, AppError> {
    ensure_install_platform(tab)?;

    let bytes = if state.cluster.role == Role::Worker {
        let (master, token) = require_master(state)?;
        let url = format!(
            "{master}/api/cluster/repo/{}/{}/download",
            pct_encode(tab),
            pct_encode(package)
        );
        // Installing Docker/K3s can briefly rebuild host routes and firewall
        // rules. Retry transport failures so a short CNI transition does not
        // abort every remaining worker package.
        let mut response = None;
        let mut last_error = None;
        const MASTER_DOWNLOAD_ATTEMPTS: usize = 60;
        for attempt in 1..=MASTER_DOWNLOAD_ATTEMPTS {
            match crate::cluster::http::get_bytes(&url, &token).await {
                Ok(value) => {
                    response = Some(value);
                    break;
                }
                Err(error) => {
                    last_error = Some(error);
                    if attempt < MASTER_DOWNLOAD_ATTEMPTS {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
            }
        }
        let (status, data) = response.ok_or_else(|| {
            AppError::bad(format!(
                "从主节点下载失败：{:#}",
                last_error.unwrap_or_else(|| anyhow::anyhow!("未收到主节点响应"))
            ))
        })?;
        if !status.is_success() {
            return Err(AppError::bad(format!(
                "从主节点下载失败 {status}: {}",
                String::from_utf8_lossy(&data)
            )));
        }
        data.to_vec()
    } else {
        let root = repo_root(&state.paths);
        let tab = tab.to_string();
        let package = package.to_string();
        tokio::task::spawn_blocking(move || build_tarball(&root, &tab, &package))
            .await
            .map_err(|e| AppError::internal(e.to_string()))??
    };

    run_package_bytes(state, &bytes, package, envs).await
}

fn require_master(state: &AppState) -> Result<(String, String), AppError> {
    let token = state
        .cluster
        .token
        .clone()
        .ok_or_else(|| AppError::bad("未配置集群令牌"))?;
    let master = state
        .master_url
        .lock()
        .map_err(|_| AppError::internal("集群状态锁不可用"))?
        .clone()
        .ok_or_else(|| AppError::bad("尚未发现主节点，请稍后重试"))?;
    Ok((master, token))
}

async fn run_package_bytes(
    state: &AppState,
    bytes: &[u8],
    package: &str,
    envs: &[(String, String)],
) -> Result<InstallResult, AppError> {
    let tmp = state
        .paths
        .uploads_dir
        .join(format!("repo-install-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&tmp)
        .await
        .map_err(|e| AppError::internal(e.to_string()))?;

    let result = async {
        let bytes = bytes.to_vec();
        let dest = tmp.clone();
        tokio::task::spawn_blocking(move || extract_tarball(&bytes, &dest))
            .await
            .map_err(|e| AppError::internal(e.to_string()))??;

        let installer = find_installer_in_dir(&tmp)?;
        run_installer_in_dir(&tmp, &installer, package, envs).await
    }
    .await;

    let _ = tokio::fs::remove_dir_all(&tmp).await;
    result
}

async fn run_installer_in_dir(
    dir: &FsPath,
    installer: &str,
    package: &str,
    envs: &[(String, String)],
) -> Result<InstallResult, AppError> {
    let script = dir.join(installer);
    if !script.is_file() {
        return Err(AppError::not_found("安装脚本不存在"));
    }
    normalize_crlf(&script)
        .map_err(|e| AppError::internal(format!("无法规范化安装脚本换行：{e}")))?;
    let (program, args) = launch_command(&script)?;

    let started = Instant::now();
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args)
        .current_dir(dir)
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("CANGLING_REPO_PACKAGE", package)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = cmd
        .spawn()
        .map_err(|e| AppError::bad(format!("无法启动安装脚本：{e}")))?;

    let output = match tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => return Err(AppError::internal(format!("安装脚本执行失败：{err}"))),
        Err(_) => {
            return Ok(InstallResult {
                package: package.to_string(),
                installer: installer.to_string(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                timed_out: true,
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
        }
    };

    Ok(InstallResult {
        package: package.to_string(),
        installer: installer.to_string(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code(),
        timed_out: false,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

pub fn scan_index(paths: &AppPaths) -> RepoIndex {
    let root = repo_root(paths);
    let pkg_root = packages_root(&root);
    let exists = root.is_dir();
    let mut tabs = Vec::new();
    for platform in PLATFORMS {
        let tab_dir = pkg_root.join(platform.directory);
        let packages = if tab_dir.is_dir() {
            scan_tab(&tab_dir)
        } else {
            Vec::new()
        };
        tabs.push(RepoTab {
            id: platform.id.to_string(),
            name: platform.name.to_string(),
            packages,
        });
    }
    let np4_dir = root.join(NP4_SET);
    if np4_dir.is_dir() {
        tabs.push(RepoTab {
            id: NP4_SET.to_string(),
            name: "np4".to_string(),
            packages: scan_tab(&np4_dir),
        });
    }
    let accelerator_dir = root.join("accelerators/packages");
    if accelerator_dir.is_dir() {
        tabs.push(RepoTab {
            id: "accelerators".into(),
            name: "GPU/NPU/DCU 集成".into(),
            packages: scan_tab(&accelerator_dir),
        });
    }
    RepoIndex {
        root: root.display().to_string(),
        exists,
        remote: false,
        master: None,
        host_platform: host_platform().to_string(),
        tabs,
    }
}

fn scan_tab(dir: &FsPath) -> Vec<RepoPackage> {
    let mut packages = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        packages.push(scan_package(&path));
    }
    packages
}

fn scan_package(dir: &FsPath) -> RepoPackage {
    let name = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut files = Vec::new();
    let mut bytes = 0u64;
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(dir)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        bytes = bytes.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
        files.push(rel);
    }
    files.sort();
    let install = find_installer(&files);
    let description = read_description(dir, install.as_deref());
    RepoPackage {
        name,
        description,
        file_count: files.len(),
        files,
        bytes,
        install,
    }
}

fn find_installer(files: &[String]) -> Option<String> {
    // 根目录优先：install.sh / setup.sh 等标准命名
    for name in INSTALLER_NAMES {
        if let Some(f) = files
            .iter()
            .find(|f| !f.contains('/') && f.eq_ignore_ascii_case(name))
        {
            return Some(f.clone());
        }
    }
    // 任意层级：文件名匹配标准命名（不区分大小写）
    for name in INSTALLER_NAMES {
        if let Some(f) = files
            .iter()
            .find(|f| f.rsplit('/').next().unwrap_or(f).eq_ignore_ascii_case(name))
        {
            return Some(f.clone());
        }
    }
    // 兜底：install* / setup*
    for f in files {
        let base = f.rsplit('/').next().unwrap_or(f).to_ascii_lowercase();
        if base.starts_with("install") || base.starts_with("setup") {
            return Some(f.clone());
        }
    }
    None
}

fn find_installer_in_dir(dir: &FsPath) -> Result<String, AppError> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            let rel = entry
                .path()
                .strip_prefix(dir)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            files.push(rel);
        }
    }
    files.sort();
    find_installer(&files)
        .ok_or_else(|| AppError::bad("该软件包没有安装脚本，仅可下载。安装脚本不是必须的。"))
}

fn read_description(dir: &FsPath, install: Option<&str>) -> String {
    if let Some(rel) = install {
        if let Some(desc) = script_description(&dir.join(rel)) {
            return desc;
        }
    }
    for fname in ["description.txt", "说明.txt", "README.md"] {
        let path = dir.join(fname);
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Some(line) = s
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'))
            {
                return line.to_string();
            }
        }
    }
    String::new()
}

/// 从 shebang 脚本头部读取 `##` 描述行（连续）。
fn script_description(path: &FsPath) -> Option<String> {
    let lines = read_head_lines(path, 8 * 1024).ok()?;
    let first = lines.first()?;
    if !first.starts_with("#!") {
        return None;
    }
    let mut desc = Vec::new();
    for line in lines.iter().skip(1) {
        if let Some(rest) = line.strip_prefix("##") {
            desc.push(rest.trim().to_string());
        } else if line.starts_with('#') {
            continue;
        } else {
            break;
        }
    }
    if desc.is_empty() {
        None
    } else {
        Some(desc.join("\n"))
    }
}

pub fn build_tarball(root: &FsPath, tab: &str, package: &str) -> Result<Vec<u8>, AppError> {
    let dir = package_dir(root, tab, package)?;
    if !dir.is_dir() {
        return Err(AppError::not_found(format!(
            "软件包不存在：{}；当前节点需要平台目录 {tab}/",
            dir.display()
        )));
    }

    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.follow_symlinks(false);
    append_dir_recursive(&mut tar, &dir, "")?;
    let enc = tar
        .into_inner()
        .map_err(|e| AppError::internal(e.to_string()))?;
    enc.finish().map_err(|e| AppError::internal(e.to_string()))
}

fn append_dir_recursive<W: Write>(
    tar: &mut tar::Builder<W>,
    dir: &FsPath,
    prefix: &str,
) -> Result<(), AppError> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| AppError::internal(e.to_string()))?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        let base = entry.file_name().to_string_lossy().into_owned();
        let name = if prefix.is_empty() {
            base.clone()
        } else {
            format!("{prefix}/{base}")
        };
        let ft = entry
            .file_type()
            .map_err(|e| AppError::internal(e.to_string()))?;
        if ft.is_dir() {
            let meta = std::fs::metadata(&path).map_err(|e| AppError::internal(e.to_string()))?;
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&meta);
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_cksum();
            tar.append_data(&mut header, &name, std::io::empty())
                .map_err(|e| AppError::internal(e.to_string()))?;
            append_dir_recursive(tar, &path, &name)?;
        } else if ft.is_file() || ft.is_symlink() {
            tar.append_path_with_name(&path, &name)
                .map_err(|e| AppError::internal(e.to_string()))?;
        }
    }
    Ok(())
}

fn extract_tarball(bytes: &[u8], dest: &FsPath) -> Result<(), AppError> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(std::io::Cursor::new(bytes)));
    archive.set_overwrite(true);
    archive.set_preserve_permissions(true);
    for entry in archive
        .entries()
        .map_err(|e| AppError::internal(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| AppError::internal(e.to_string()))?;
        let rel = entry
            .path()
            .map_err(|e| AppError::internal(e.to_string()))?
            .into_owned();
        if rel.as_os_str().is_empty() || unsafe_rel(&rel) {
            continue;
        }
        entry
            .unpack_in(dest)
            .map_err(|e| AppError::internal(format!("解压失败：{e}")))?;
    }
    Ok(())
}

fn unsafe_rel(rel: &FsPath) -> bool {
    rel.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    })
}

fn package_dir(root: &FsPath, tab: &str, package: &str) -> Result<PathBuf, AppError> {
    if !valid_segment(tab) {
        return Err(AppError::bad("无效的平台"));
    }
    if !valid_segment(package) {
        return Err(AppError::bad("无效的软件包名"));
    }
    if tab == "accelerators" {
        return Ok(root.join("accelerators/packages").join(package));
    }
    if tab == NP4_SET {
        return Ok(root.join(NP4_SET).join(package));
    }
    let platform = PLATFORMS
        .iter()
        .find(|platform| platform.id == tab)
        .ok_or_else(|| AppError::bad("不支持的平台"))?;
    Ok(packages_root(root).join(platform.directory).join(package))
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains('\0')
}

pub fn tarball_response(package: &str, bytes: Vec<u8>) -> Response {
    let name = download_name(package);
    let disposition = format!("attachment; filename=\"{name}\"");
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/gzip"),
    );
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    (headers, bytes).into_response()
}

fn download_name(package: &str) -> String {
    let mut s: String = package
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        s = "package".into();
    }
    format!("{s}.tar.gz")
}

fn pct_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn read_head_lines(path: &FsPath, max_bytes: usize) -> std::io::Result<Vec<String>> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut lines = Vec::new();
    let mut total = 0usize;
    for line in reader.lines() {
        let line = line?;
        total += line.len() + 1;
        lines.push(line);
        if total >= max_bytes {
            break;
        }
    }
    Ok(lines)
}

fn first_line(path: &FsPath) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    Some(
        line.trim_end_matches(|c| c == '\n' || c == '\r')
            .to_string(),
    )
}

/// 把 CRLF 行尾规范化为 LF。离线包若经 Windows 检出，安装脚本可能带 `\r`，
/// bash 会把 `pipefail\r` 当成非法选项名（退出码 2），导致安装失败。
fn normalize_crlf(path: &FsPath) -> std::io::Result<()> {
    let bytes = std::fs::read(path)?;
    if !bytes.windows(2).any(|w| w == b"\r\n") {
        return Ok(());
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    std::fs::write(path, out)
}

fn is_executable_path(path: &FsPath) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// 返回 (程序, 参数列表，已含脚本路径)。
fn launch_command(script: &FsPath) -> Result<(String, Vec<String>), AppError> {
    if is_executable_path(script) {
        return Ok((script.display().to_string(), Vec::new()));
    }
    let first = first_line(script).ok_or_else(|| AppError::bad("无法读取安装脚本"))?;
    if !first.starts_with("#!") {
        return Err(AppError::bad(
            "安装脚本缺少 shebang 且没有执行权限，无法运行",
        ));
    }
    let (program, mut args) = interpreter_from_shebang(&first)
        .ok_or_else(|| AppError::bad("无法解析 shebang，且安装脚本没有执行权限"))?;
    args.push(script.display().to_string());
    Ok((program, args))
}

fn interpreter_from_shebang(shebang: &str) -> Option<(String, Vec<String>)> {
    let rest = shebang.trim_start_matches("#!").trim();
    if rest.is_empty() {
        return None;
    }
    let mut parts = rest.split_whitespace();
    let first = parts.next()?;
    if first == "env" || first.ends_with("/env") {
        let program = parts.next()?;
        let args: Vec<String> = parts.map(str::to_string).collect();
        Some((program.to_string(), args))
    } else {
        let args: Vec<String> = parts.map(str::to_string).collect();
        Some((first.to_string(), args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cangling-repo-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn host_platform_is_known() {
        let p = host_platform();
        assert!(PLATFORMS.iter().any(|platform| platform.id == p));
    }

    #[test]
    fn detects_platform_by_distribution_and_architecture() {
        assert_eq!(
            detect_host_platform("x86_64", "ID=kylin\nID_LIKE=rhel", false, true),
            "kylin-x86-v10-rpm"
        );
        assert_eq!(
            detect_host_platform("aarch64", "ID=kylin", false, true),
            "kylin-arm-v10-rpm"
        );
        assert_eq!(
            detect_host_platform(
                "x86_64",
                "ID=ubuntu\nID_LIKE=debian\nVERSION_ID=22.04",
                true,
                false
            ),
            "ubuntu-x86-22.04-deb"
        );
        assert_eq!(
            detect_host_platform("aarch64", "ID=debian", true, false),
            "unsupported"
        );
    }

    #[test]
    fn package_manager_is_used_when_os_release_is_unknown() {
        assert_eq!(
            detect_host_platform("x86_64", "", false, true),
            "unsupported"
        );
        assert_eq!(
            detect_host_platform("aarch64", "", true, false),
            "unsupported"
        );
    }

    #[test]
    fn find_installer_prefers_root_standard_name() {
        let files = vec![
            "a/install.sh".to_string(),
            "install.sh".to_string(),
            "README.md".to_string(),
        ];
        assert_eq!(find_installer(&files).as_deref(), Some("install.sh"));
        assert_eq!(
            find_installer(&["setup.bat".to_string()]).as_deref(),
            Some("setup.bat")
        );
        assert!(find_installer(&["readme.txt".to_string()]).is_none());
    }

    #[test]
    fn path_validation_rejects_traversal() {
        let root = FsPath::new("/tmp/repo");
        assert!(package_dir(root, "ubuntu-x86-22.04-deb", "pkg").is_ok());
        assert!(package_dir(root, "../x", "pkg").is_err());
        assert!(package_dir(root, "ubuntu-x86-22.04-deb", "../pkg").is_err());
        assert!(package_dir(root, "ubuntu-x86-22.04-deb", "a/b").is_err());
    }

    #[test]
    fn package_dir_uses_versioned_platform_tree() {
        let dir = tmpdir("nested");
        let nested = dir.join("platforms/ubuntu/x86/22.04/deb/git");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("new"), "new").unwrap();
        let got = package_dir(&dir, "ubuntu-x86-22.04-deb", "git").unwrap();
        assert_eq!(got, nested);
        let np4 = package_dir(&dir, NP4_SET, NP4_UPDATE).unwrap();
        assert_eq!(np4, dir.join(NP4_SET).join(NP4_UPDATE));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn packages_root_is_the_repository_root() {
        let dir = tmpdir("pkgroot");
        assert_eq!(packages_root(&dir), dir);
        std::fs::create_dir_all(dir.join(CANGLING_REPO_SET)).unwrap();
        assert_eq!(packages_root(&dir), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn script_description_reads_double_hash_lines() {
        let dir = tmpdir("desc");
        let script = dir.join("install.sh");
        std::fs::write(
            &script,
            "#!/bin/bash\n# 应用名\n## 第一行\n## 第二行\n\necho hi\n",
        )
        .unwrap();
        assert_eq!(
            script_description(&script).as_deref(),
            Some("第一行\n第二行")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalize_crlf_converts_windows_line_endings() {
        let dir = tmpdir("crlf");
        let script = dir.join("install.sh");
        std::fs::write(&script, "#!/bin/bash\r\nset -euo pipefail\r\necho ok\r\n").unwrap();
        normalize_crlf(&script).unwrap();
        assert_eq!(
            std::fs::read_to_string(&script).unwrap(),
            "#!/bin/bash\nset -euo pipefail\necho ok\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tar_roundtrip_keeps_files() {
        let dir = tmpdir("tar");
        let pkg = dir.join("platforms/ubuntu/x86/22.04/deb/demo");
        std::fs::create_dir_all(pkg.join("sub")).unwrap();
        std::fs::write(pkg.join("install.sh"), "#!/bin/bash\necho ok\n").unwrap();
        std::fs::write(pkg.join("sub").join("a.txt"), "hello").unwrap();

        let bytes = build_tarball(&dir, "ubuntu-x86-22.04-deb", "demo").unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        extract_tarball(&bytes, &out).unwrap();

        assert!(out.join("install.sh").is_file());
        assert_eq!(
            std::fs::read_to_string(out.join("sub/a.txt")).unwrap(),
            "hello"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsafe_rel_rejects_traversal() {
        assert!(unsafe_rel(FsPath::new("../evil.txt")));
        assert!(unsafe_rel(FsPath::new("/abs")));
        assert!(unsafe_rel(FsPath::new("a/../../evil")));
        assert!(!unsafe_rel(FsPath::new("a/b.txt")));
        assert!(!unsafe_rel(FsPath::new("install.sh")));
    }
}
