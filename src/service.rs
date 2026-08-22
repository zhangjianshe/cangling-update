use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const SERVICE_NAME: &str = "cangling-update";
const UNIT_PATH: &str = "/etc/systemd/system/cangling-update.service";
const BIN_LINK_DIR: &str = "/usr/local/bin";

pub fn install(bind: &str, port: u16, data_dir: Option<&Path>) -> Result<()> {
    require_root()?;
    require_systemd()?;

    let exe = current_exe()?;
    let workdir = exe
        .parent()
        .map(Path::to_path_buf)
        .context("executable has no parent directory")?;

    let mut exec = format!(
        "{} --bind {} --port {}",
        shell_quote(&exe),
        shell_quote(Path::new(bind)),
        port
    );
    if let Some(dir) = data_dir {
        let dir = if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            std::fs::canonicalize(dir).unwrap_or_else(|_| workdir.join(dir))
        };
        exec.push_str(" --data-dir ");
        exec.push_str(&shell_quote(&dir));
    }

    let unit = format!(
        r#"[Unit]
Description=Cangling Update docker-compose host updater
Documentation=file:{exe}
After=network-online.target docker.service docker.socket
Wants=network-online.target docker.service

[Service]
Type=simple
WorkingDirectory={workdir}
ExecStart={exec}
Restart=on-failure
RestartSec=3
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
"#,
        exe = exe.display(),
        workdir = shell_quote(&workdir),
        exec = exec,
    );

    std::fs::write(UNIT_PATH, unit).with_context(|| format!("写入 {UNIT_PATH}，需要 root 权限"))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", SERVICE_NAME])?;

    eprintln!("已安装并启动 systemd 服务：{SERVICE_NAME}");
    eprintln!("  程序    {}", exe.display());
    eprintln!("  工作目录 {}", workdir.display());
    eprintln!("  单元文件 {UNIT_PATH}");
    install_bin_link(&exe)?;
    eprintln!("  管理：systemctl status|restart|stop {SERVICE_NAME}");
    eprintln!();
    print_access_urls(bind, port);
    Ok(())
}

pub fn uninstall() -> Result<()> {
    require_root()?;
    require_systemd()?;

    if !Path::new(UNIT_PATH).exists() {
        eprintln!("服务未安装：{SERVICE_NAME}");
        return Ok(());
    }

    // Stop and clear failed state while the unit file still exists, then
    // disable/remove. reset-failed after daemon-reload would print
    // "Unit … not loaded" even though uninstall already succeeded.
    systemctl_best_effort(&["stop", SERVICE_NAME]);
    systemctl_best_effort(&["reset-failed", SERVICE_NAME]);
    systemctl_best_effort(&["disable", SERVICE_NAME]);
    std::fs::remove_file(UNIT_PATH).with_context(|| format!("删除 {UNIT_PATH}"))?;
    systemctl_best_effort(&["daemon-reload"]);
    uninstall_bin_link();
    eprintln!("已卸载 systemd 服务：{SERVICE_NAME}");
    Ok(())
}

pub fn restart() -> Result<()> {
    require_systemd()?;
    if !Path::new(UNIT_PATH).exists() {
        bail!("尚未安装服务。请先执行：{} install-service", exe_name());
    }
    systemctl(&["restart", SERVICE_NAME])?;
    eprintln!("已重启服务：{SERVICE_NAME}");
    let _ = systemctl(&["--no-pager", "--full", "status", SERVICE_NAME]);
    Ok(())
}

pub fn is_installed() -> bool {
    Path::new(UNIT_PATH).exists()
}

/// True when this process is the systemd unit itself (must start the server).
pub fn running_as_systemd_service() -> bool {
    if let Ok(cgroup) = std::fs::read_to_string("/proc/self/cgroup") {
        let marker = format!("{SERVICE_NAME}.service");
        if cgroup.lines().any(|line| line.contains(&marker)) {
            return true;
        }
    }
    // Fallback: systemd 239+ sets this to the ExecStart PID.
    std::env::var("SYSTEMD_EXEC_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
        == Some(std::process::id())
}

/// Print the installed service's listen URLs. Used when the user re-runs the
/// binary after `install-service` instead of starting a second web process.
pub fn print_installed_access() -> Result<()> {
    let (bind, port) = listen_from_unit();
    let active = is_active();

    eprintln!("已安装为系统服务，不会在前台再次启动。");
    eprintln!();
    eprintln!("  服务     {SERVICE_NAME}");
    eprintln!(
        "  状态     {}",
        if active { "运行中" } else { "已停止" }
    );
    eprintln!("  单元文件 {UNIT_PATH}");
    eprintln!();
    if active {
        print_access_urls(&bind, port);
    } else {
        eprintln!("访问地址（启动后可用）：");
        for url in access_urls(&bind, port) {
            eprintln!("  {url}");
        }
        eprintln!();
        eprintln!("启动：systemctl start {SERVICE_NAME}");
        eprintln!("      或 {} restart", exe_name());
    }
    Ok(())
}

fn print_access_urls(bind: &str, port: u16) {
    eprintln!("访问地址：");
    for url in access_urls(bind, port) {
        eprintln!("  {url}");
    }
}

fn listen_from_unit() -> (String, u16) {
    let Ok(content) = std::fs::read_to_string(UNIT_PATH) else {
        return default_listen();
    };
    for line in content.lines() {
        let line = line.trim();
        if let Some(exec) = line.strip_prefix("ExecStart=") {
            return listen_from_exec(exec);
        }
    }
    default_listen()
}

fn default_listen() -> (String, u16) {
    ("0.0.0.0".into(), 5400)
}

fn listen_from_exec(exec: &str) -> (String, u16) {
    let mut bind = "0.0.0.0".to_string();
    let mut port = 5400u16;
    let parts: Vec<&str> = exec.split_whitespace().collect();
    let mut i = 0;
    while i < parts.len() {
        let p = parts[i].trim_matches('"');
        if p == "--bind" {
            if let Some(v) = parts.get(i + 1) {
                bind = v.trim_matches('"').to_string();
                i += 2;
                continue;
            }
        } else if let Some(v) = p.strip_prefix("--bind=") {
            bind = v.trim_matches('"').to_string();
        } else if p == "--port" {
            if let Some(v) = parts.get(i + 1) {
                if let Ok(n) = v.trim_matches('"').parse() {
                    port = n;
                }
                i += 2;
                continue;
            }
        } else if let Some(v) = p.strip_prefix("--port=") {
            if let Ok(n) = v.trim_matches('"').parse() {
                port = n;
            }
        }
        i += 1;
    }
    (bind, port)
}

fn access_urls(bind: &str, port: u16) -> Vec<String> {
    let mut hosts = Vec::new();
    if is_unspecified_bind(bind) {
        hosts.push("127.0.0.1".to_string());
        for ip in local_ipv4_addrs() {
            if ip != "127.0.0.1" {
                hosts.push(ip);
            }
        }
    } else if bind == "::1" || bind == "[::1]" {
        hosts.push("[::1]".to_string());
    } else if bind.parse::<std::net::Ipv6Addr>().is_ok() {
        hosts.push(format!("[{bind}]"));
    } else {
        hosts.push(bind.to_string());
    }
    hosts
        .into_iter()
        .map(|h| format!("http://{h}:{port}"))
        .collect()
}

fn is_unspecified_bind(bind: &str) -> bool {
    matches!(bind, "0.0.0.0" | "*" | "::" | "[::]")
}

fn local_ipv4_addrs() -> Vec<String> {
    let mut addrs = Vec::new();
    if let Ok(out) = Command::new("ip")
        .args(["-4", "-o", "addr", "show"])
        .output()
    {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(i) = parts.iter().position(|p| *p == "inet") {
                    if let Some(cidr) = parts.get(i + 1) {
                        let ip = cidr.split('/').next().unwrap_or("");
                        if is_usable_ipv4(ip) {
                            addrs.push(ip.to_string());
                        }
                    }
                }
            }
        }
    }
    if addrs.is_empty() {
        if let Ok(out) = Command::new("hostname").arg("-I").output() {
            if out.status.success() {
                for tok in String::from_utf8_lossy(&out.stdout).split_whitespace() {
                    if is_usable_ipv4(tok) {
                        addrs.push(tok.to_string());
                    }
                }
            }
        }
    }
    addrs.sort();
    addrs.dedup();
    addrs
}

fn is_usable_ipv4(s: &str) -> bool {
    let Ok(ip) = s.parse::<std::net::Ipv4Addr>() else {
        return false;
    };
    !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified() && !ip.is_multicast()
}

fn is_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", SERVICE_NAME])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn bin_link_path() -> PathBuf {
    Path::new(BIN_LINK_DIR).join(SERVICE_NAME)
}

fn install_bin_link(exe: &Path) -> Result<()> {
    let link = bin_link_path();
    match ensure_bin_link(exe, &link)? {
        BinLinkAction::Created => {
            eprintln!("  命令    {} -> {}", link.display(), exe.display());
        }
        BinLinkAction::Updated => {
            eprintln!("  命令    已更新 {} -> {}", link.display(), exe.display());
        }
        BinLinkAction::Unchanged => {
            eprintln!("  命令    {} -> {}", link.display(), exe.display());
        }
        BinLinkAction::AlreadyOnPath => {
            eprintln!("  命令    {}", link.display());
        }
    }
    Ok(())
}

fn uninstall_bin_link() {
    let link = bin_link_path();
    match remove_bin_link(&link) {
        Ok(true) => eprintln!("已删除符号链接 {}", link.display()),
        Ok(false) => {}
        Err(err) => eprintln!("删除符号链接 {} 失败：{err}", link.display()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinLinkAction {
    Created,
    Updated,
    Unchanged,
    AlreadyOnPath,
}

fn ensure_bin_link(exe: &Path, link: &Path) -> Result<BinLinkAction> {
    if let Some(dir) = link.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("创建 {}", dir.display()))?;
    }

    let exe_canon = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let meta = match link.symlink_metadata() {
        Ok(m) => m,
        Err(_) => {
            unix_symlink(&exe_canon, link)?;
            return Ok(BinLinkAction::Created);
        }
    };

    if meta.file_type().is_symlink() {
        if let Ok(target) = std::fs::canonicalize(link) {
            if target == exe_canon {
                return Ok(BinLinkAction::Unchanged);
            }
        }
        std::fs::remove_file(link)
            .with_context(|| format!("删除旧符号链接 {}", link.display()))?;
        unix_symlink(&exe_canon, link)?;
        return Ok(BinLinkAction::Updated);
    }

    if let Ok(existing) = std::fs::canonicalize(link) {
        if existing == exe_canon {
            return Ok(BinLinkAction::AlreadyOnPath);
        }
    }
    bail!(
        "{} 已存在且不是指向本程序的符号链接，未覆盖。请先处理该路径后再执行 install-service",
        link.display()
    );
}

fn remove_bin_link(link: &Path) -> Result<bool> {
    let Ok(meta) = link.symlink_metadata() else {
        return Ok(false);
    };
    if !meta.file_type().is_symlink() {
        return Ok(false);
    }
    std::fs::remove_file(link).with_context(|| format!("删除 {}", link.display()))?;
    Ok(true)
}

fn unix_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("创建符号链接 {} -> {}", link.display(), target.display()))
}

fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("无法解析当前可执行文件")?;
    std::fs::canonicalize(&exe).or(Ok(exe))
}

fn exe_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| SERVICE_NAME.to_string())
}

fn shell_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':'))
    {
        s.into_owned()
    } else {
        format!("\"{}\"", s.replace('"', "\\\""))
    }
}

fn require_root() -> Result<()> {
    if !running_as_root() {
        bail!("请使用 root 执行此命令，例如：sudo {}", exe_name());
    }
    Ok(())
}

fn running_as_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

fn require_systemd() -> Result<()> {
    if !Path::new("/run/systemd/system").exists() && !Path::new("/usr/bin/systemctl").exists() {
        bail!("当前系统未检测到 systemd，无法安装 service");
    }
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .args(args)
        .status()
        .with_context(|| format!("执行 systemctl {}", args.join(" ")))?;
    if !status.success() {
        bail!("systemctl {} 失败，退出码 {:?}", args.join(" "), status.code());
    }
    Ok(())
}

fn systemctl_best_effort(args: &[&str]) {
    let _ = Command::new("systemctl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_from_exec_defaults() {
        assert_eq!(
            listen_from_exec("/opt/cangling-update"),
            ("0.0.0.0".into(), 5400)
        );
    }

    #[test]
    fn listen_from_exec_flags() {
        assert_eq!(
            listen_from_exec("/opt/cangling-update --bind 10.1.1.8 --port 8080"),
            ("10.1.1.8".into(), 8080)
        );
        assert_eq!(
            listen_from_exec("/opt/cangling-update --bind=127.0.0.1 --port=6000"),
            ("127.0.0.1".into(), 6000)
        );
    }

    #[test]
    fn access_urls_specific_bind() {
        assert_eq!(
            access_urls("10.1.1.8", 5400),
            vec!["http://10.1.1.8:5400".to_string()]
        );
        assert_eq!(
            access_urls("::1", 5400),
            vec!["http://[::1]:5400".to_string()]
        );
    }

    #[test]
    fn access_urls_unspecified_includes_localhost() {
        let urls = access_urls("0.0.0.0", 5400);
        assert_eq!(urls[0], "http://127.0.0.1:5400");
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cangling-link-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ensure_bin_link_creates_and_is_idempotent() {
        let dir = temp_dir();
        let exe = dir.join("cangling-update");
        std::fs::write(&exe, b"fake").unwrap();
        let link = dir.join("bin").join("cangling-update");

        assert_eq!(ensure_bin_link(&exe, &link).unwrap(), BinLinkAction::Created);
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::canonicalize(&link).unwrap(), std::fs::canonicalize(&exe).unwrap());

        assert_eq!(
            ensure_bin_link(&exe, &link).unwrap(),
            BinLinkAction::Unchanged
        );
        assert!(remove_bin_link(&link).unwrap());
        assert!(!link.symlink_metadata().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_bin_link_replaces_stale_symlink() {
        let dir = temp_dir();
        let exe = dir.join("cangling-update");
        let old = dir.join("old");
        std::fs::write(&exe, b"new").unwrap();
        std::fs::write(&old, b"old").unwrap();
        let link = dir.join("cangling-update-link");
        std::os::unix::fs::symlink(&old, &link).unwrap();

        assert_eq!(ensure_bin_link(&exe, &link).unwrap(), BinLinkAction::Updated);
        assert_eq!(std::fs::canonicalize(&link).unwrap(), std::fs::canonicalize(&exe).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_bin_link_skips_when_binary_already_there() {
        let dir = temp_dir();
        let exe = dir.join("cangling-update");
        std::fs::write(&exe, b"fake").unwrap();
        assert_eq!(
            ensure_bin_link(&exe, &exe).unwrap(),
            BinLinkAction::AlreadyOnPath
        );
        assert!(!exe.symlink_metadata().unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_bin_link_refuses_to_clobber_regular_file() {
        let dir = temp_dir();
        let exe = dir.join("cangling-update");
        let other = dir.join("other");
        std::fs::write(&exe, b"a").unwrap();
        std::fs::write(&other, b"b").unwrap();
        assert!(ensure_bin_link(&exe, &other).is_err());
        assert_eq!(std::fs::read(&other).unwrap(), b"b");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
