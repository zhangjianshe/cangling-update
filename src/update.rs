use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "zhangjianshe/cangling-update";
const API: &str = "https://api.github.com/repos/zhangjianshe/cangling-update/releases/latest";
const USER_AGENT: &str = "cangling-update";

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub fn run(check_only: bool, force: bool, proxy: Option<String>) -> Result<()> {
    if let Some(p) = proxy {
        apply_proxy_env(&normalize_proxy(&p));
    } else if let Some(p) = detect_proxy() {
        apply_proxy_env(&p);
    }

    let current = env!("CARGO_PKG_VERSION");
    let exe = current_exe()?;
    let dest_dir = exe
        .parent()
        .map(Path::to_path_buf)
        .context("executable has no parent directory")?;
    let dest = dest_dir.join("cangling-update");
    let asset_name = asset_for_arch()?;

    eprintln!("当前版本  v{current}");
    eprintln!("仓库      https://github.com/{REPO}");
    eprintln!("架构资源  {asset_name}");
    match detect_proxy() {
        Some(p) => eprintln!("代理      {p}"),
        None => eprintln!("代理      未设置（示例：https_proxy=http://10.1.1.2:7890）"),
    }

    probe_github()?;

    let release = fetch_latest().map_err(annotate_network)?;
    let latest = strip_v(&release.tag_name);
    eprintln!("最新版本  {}", release.tag_name);

    if !force && !is_newer(latest, current) {
        eprintln!("已是最新，无需下载。");
        return Ok(());
    }

    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .with_context(|| format!("最新 Release 中没有 {asset_name}"))?;

    if check_only {
        eprintln!("有新版本，下载地址：{}", asset.browser_download_url);
        return Ok(());
    }

    let tmp = dest_dir.join(format!(".{asset_name}.download"));
    let _ = fs::remove_file(&tmp);
    download(&asset.browser_download_url, &tmp).map_err(annotate_network)?;

    if let Some(sum_url) = release
        .assets
        .iter()
        .find(|a| a.name == format!("{asset_name}.sha256"))
        .map(|a| a.browser_download_url.as_str())
    {
        verify_sha256(sum_url, &tmp, asset_name)?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(&tmp)?.permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&tmp, perm)?;
    }

    // Replace on disk only. A running systemd process keeps the old inode
    // until the next restart — we do not restart the service here.
    fs::rename(&tmp, &dest).with_context(|| {
        format!(
            "无法写入 {}（需要对该目录的写权限）",
            dest.display()
        )
    })?;

    eprintln!("已更新到 {}：{}", release.tag_name, dest.display());
    eprintln!("未重启服务。若正在以 systemd 运行，执行后再生效：");
    eprintln!("  {} restart", dest.display());
    Ok(())
}

fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("无法解析当前可执行文件")?;
    Ok(fs::canonicalize(&exe).unwrap_or(exe))
}

fn asset_for_arch() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("cangling-update-linux-amd64"),
        "aarch64" => Ok("cangling-update-linux-arm64"),
        other => bail!("不支持的架构 {other}，仅支持 x86_64 与 aarch64"),
    }
}

fn strip_v(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

fn is_newer(remote: &str, local: &str) -> bool {
    match (parse_semver(remote), parse_semver(local)) {
        (Some(r), Some(l)) => r > l,
        _ => remote != local,
    }
}

fn fetch_latest() -> Result<Release> {
    let body = http_get_string(API)?;
    serde_json::from_str(&body).context("解析 GitHub Release 信息失败")
}

fn download(url: &str, dest: &Path) -> Result<()> {
    eprintln!("下载      {url}");
    eprintln!("进度：");
    http_get_file(url, dest)?;
    let len = fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    if len < 1024 {
        let _ = fs::remove_file(dest);
        bail!("下载文件过小（{len} 字节），可能不是有效二进制");
    }
    eprintln!("已保存    {}（{} 字节）", dest.display(), len);
    Ok(())
}

fn detect_proxy() -> Option<String> {
    for key in [
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(normalize_proxy(v));
            }
        }
    }
    None
}

fn normalize_proxy(raw: &str) -> String {
    let s = raw.trim();
    if s.contains("://") {
        s.to_string()
    } else {
        format!("http://{s}")
    }
}

fn apply_proxy_env(proxy: &str) {
    // Clash/V2Ray 的 7890 是 HTTP 代理。无 scheme 时 curl 常会连错。
    std::env::set_var("http_proxy", proxy);
    std::env::set_var("https_proxy", proxy);
    std::env::set_var("HTTP_PROXY", proxy);
    std::env::set_var("HTTPS_PROXY", proxy);
    std::env::set_var("ALL_PROXY", proxy);
    std::env::set_var("all_proxy", proxy);
}

fn apply_curl_proxy(cmd: &mut Command) {
    if let Some(p) = detect_proxy() {
        cmd.args(["-x", &p]);
    }
}

fn apply_wget_proxy(cmd: &mut Command) {
    if let Some(p) = detect_proxy() {
        cmd.args(["-e", "use_proxy=yes"]);
        cmd.args(["-e", &format!("http_proxy={p}")]);
        cmd.args(["-e", &format!("https_proxy={p}")]);
    }
}

fn connect_timeout() -> &'static str {
    if detect_proxy().is_some() {
        "20"
    } else {
        "10"
    }
}

fn probe_github() -> Result<()> {
    eprintln!("检查      是否能访问 GitHub…");
    if have("curl") {
        let mut cmd = Command::new("curl");
        cmd.args([
            "-fsS",
            "-o",
            "/dev/null",
            "--connect-timeout",
            connect_timeout(),
            "--max-time",
            "45",
            "-A",
            USER_AGENT,
        ]);
        apply_curl_proxy(&mut cmd);
        cmd.arg("https://api.github.com");
        let output = cmd.output().context("执行 curl")?;
        if !output.status.success() {
            bail!("{}", github_unreachable(output.status.code(), &output.stderr));
        }
        return Ok(());
    }
    if have("wget") {
        let mut cmd = Command::new("wget");
        cmd.args([
            "-q",
            &format!("--timeout={}", connect_timeout()),
            "--tries=1",
            "-O",
            "/dev/null",
        ]);
        apply_wget_proxy(&mut cmd);
        cmd.arg("https://api.github.com");
        let output = cmd.output().context("执行 wget")?;
        if !output.status.success() {
            bail!("{}", github_unreachable(output.status.code(), &output.stderr));
        }
        return Ok(());
    }
    bail!("需要 curl 或 wget 才能检查/下载更新")
}

fn annotate_network(err: anyhow::Error) -> anyhow::Error {
    let text = err.to_string();
    if looks_like_network_error(&text) {
        anyhow::anyhow!("{}\n{}", github_unreachable(None, text.as_bytes()), err)
    } else {
        err
    }
}

fn looks_like_network_error(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("could not resolve")
        || t.contains("nodename nor servname")
        || t.contains("temporary failure in name resolution")
        || t.contains("failed to connect")
        || t.contains("connection timed out")
        || t.contains("connection refused")
        || t.contains("network is unreachable")
        || t.contains("ssl")
        || t.contains("timed out")
        || t.contains("timeout")
        || t.contains("无法")
}

fn github_unreachable(code: Option<i32>, stderr: &[u8]) -> String {
    let err = String::from_utf8_lossy(stderr);
    let extra = match code {
        Some(6) => "无法解析域名（DNS）。",
        Some(7) => "无法建立连接。",
        Some(28) | Some(4) => "连接超时。",
        Some(35) | Some(5) => "TLS/SSL 握手失败。",
        Some(22) | Some(8) => "GitHub 返回了 HTTP 错误。",
        _ if looks_like_network_error(&err) => "网络不可达。",
        _ => "网络不可访问。",
    };
    format!(
        "无法访问 GitHub（https://github.com/{REPO}）。{extra}\n请检查本机网络、DNS、防火墙或代理。代理请写成：https_proxy=http://主机:端口"
    )
}

fn verify_sha256(sum_url: &str, file: &Path, asset_name: &str) -> Result<()> {
    let text = http_get_string(sum_url)?;
    let expected = text
        .split_whitespace()
        .next()
        .context("sha256 文件为空")?
        .to_ascii_lowercase();
    let output = Command::new("sha256sum")
        .arg(file)
        .output()
        .context("需要 sha256sum 以校验下载文件")?;
    if !output.status.success() {
        bail!("sha256sum 失败");
    }
    let actual = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if actual != expected {
        let _ = fs::remove_file(file);
        bail!("{asset_name} 校验失败：期望 {expected}，实际 {actual}");
    }
    eprintln!("校验      sha256 通过");
    Ok(())
}

fn http_get_string(url: &str) -> Result<String> {
    let bytes = http_get_bytes(url)?;
    String::from_utf8(bytes).context("响应不是 UTF-8")
}

fn http_get_file(url: &str, dest: &Path) -> Result<()> {
    if have("curl") {
        let mut cmd = Command::new("curl");
        cmd.args([
            "-fL",
            "--retry",
            "3",
            "--connect-timeout",
            connect_timeout(),
            "--progress-bar",
            "-A",
            USER_AGENT,
            "-o",
        ]);
        cmd.arg(dest);
        apply_curl_proxy(&mut cmd);
        cmd.arg(url);
        let status = cmd.status().context("执行 curl")?;
        if !status.success() {
            let _ = fs::remove_file(dest);
            bail!("{}", github_unreachable(status.code(), b"curl download failed"));
        }
        return Ok(());
    }
    if have("wget") {
        let mut cmd = Command::new("wget");
        cmd.args(["--show-progress", "-O"]);
        cmd.arg(dest);
        apply_wget_proxy(&mut cmd);
        cmd.arg(url);
        let status = cmd.status().context("执行 wget")?;
        if !status.success() {
            let _ = fs::remove_file(dest);
            bail!("{}", github_unreachable(status.code(), b"wget download failed"));
        }
        return Ok(());
    }
    bail!("需要 curl 或 wget 才能检查/下载更新")
}

fn http_get_bytes(url: &str) -> Result<Vec<u8>> {
    if have("curl") {
        let mut cmd = Command::new("curl");
        cmd.args([
            "-fsSL",
            "--retry",
            "3",
            "--connect-timeout",
            connect_timeout(),
            "--max-time",
            "60",
            "-A",
            USER_AGENT,
            "-H",
            "Accept: application/vnd.github+json",
        ]);
        apply_curl_proxy(&mut cmd);
        cmd.arg(url);
        let output = cmd.output().context("执行 curl")?;
        if !output.status.success() {
            bail!(
                "{}",
                github_unreachable(output.status.code(), &output.stderr)
            );
        }
        return Ok(output.stdout);
    }
    if have("wget") {
        let mut cmd = Command::new("wget");
        cmd.args([
            "-q",
            &format!("--timeout={}", connect_timeout()),
            "--tries=1",
            "-O",
            "-",
        ]);
        apply_wget_proxy(&mut cmd);
        cmd.arg(url);
        let output = cmd.output().context("执行 wget")?;
        if !output.status.success() {
            bail!(
                "{}",
                github_unreachable(output.status.code(), &output.stderr)
            );
        }
        return Ok(output.stdout);
    }
    bail!("需要 curl 或 wget 才能检查/下载更新")
}

fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn proxy_gets_http_scheme() {
        assert_eq!(normalize_proxy("10.1.1.2:7890"), "http://10.1.1.2:7890");
        assert_eq!(
            normalize_proxy("http://10.1.1.2:7890"),
            "http://10.1.1.2:7890"
        );
        assert_eq!(
            normalize_proxy("socks5://127.0.0.1:1080"),
            "socks5://127.0.0.1:1080"
        );
    }
}
