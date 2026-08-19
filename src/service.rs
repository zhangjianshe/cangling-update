use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SERVICE_NAME: &str = "cangling-update";
const UNIT_PATH: &str = "/etc/systemd/system/cangling-update.service";

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
After=network-online.target docker.service
Wants=network-online.target

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
    eprintln!("  管理：systemctl status|restart|stop {SERVICE_NAME}");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    require_root()?;
    require_systemd()?;

    let _ = systemctl(&["stop", SERVICE_NAME]);
    let _ = systemctl(&["disable", SERVICE_NAME]);
    if Path::new(UNIT_PATH).exists() {
        std::fs::remove_file(UNIT_PATH)
            .with_context(|| format!("删除 {UNIT_PATH}"))?;
    }
    let _ = systemctl(&["daemon-reload"]);
    let _ = systemctl(&["reset-failed", SERVICE_NAME]);
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
