use crate::models::{ComposeService, DockerMeta};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::RwLock;

#[derive(Clone, Debug)]
pub enum ComposeKind {
    Plugin,
    Standalone,
    Missing,
}

impl ComposeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ComposeKind::Plugin => "plugin",
            ComposeKind::Standalone => "standalone",
            ComposeKind::Missing => "missing",
        }
    }
}

#[derive(Clone, Debug)]
struct Snapshot {
    cli: bool,
    daemon: bool,
    version: Option<String>,
    compose: ComposeKind,
    checked_at: Instant,
}

impl Snapshot {
    fn available(&self) -> bool {
        self.daemon
    }

    fn to_meta(&self) -> DockerMeta {
        DockerMeta {
            available: self.available(),
            version: self.version.clone(),
            compose: self.compose.as_str().to_string(),
        }
    }

    fn ready_error(&self) -> Option<&'static str> {
        if self.daemon {
            None
        } else if self.cli {
            Some("Docker 守护进程尚未就绪，请先启动 docker")
        } else {
            Some("本机未安装 docker 命令")
        }
    }
}

/// Cached docker/compose detection. Re-probes when the daemon was down at
/// startup (or later disappears), so the UI can recover without a restart.
#[derive(Clone)]
pub struct Docker {
    inner: Arc<RwLock<Snapshot>>,
}

impl std::fmt::Debug for Docker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.inner.try_read() {
            Ok(s) => f
                .debug_struct("Docker")
                .field("available", &s.available())
                .field("version", &s.version)
                .field("compose", &s.compose)
                .finish(),
            Err(_) => f.write_str("Docker(..)"),
        }
    }
}

const REFRESH_WHEN_DOWN: Duration = Duration::from_secs(2);
const REFRESH_WHEN_UP: Duration = Duration::from_secs(30);

impl Docker {
    pub async fn detect() -> Self {
        Self {
            inner: Arc::new(RwLock::new(detect_now().await)),
        }
    }

    pub async fn meta(&self) -> DockerMeta {
        self.refresh(false).await;
        self.inner.read().await.to_meta()
    }

    pub async fn compose_kind(&self) -> ComposeKind {
        self.refresh(false).await;
        self.inner.read().await.compose.clone()
    }

    /// Re-detect immediately if the daemon is currently missing, then error if
    /// docker still cannot be used.
    async fn ensure(&self) -> Result<()> {
        self.refresh(true).await;
        if let Some(msg) = self.inner.read().await.ready_error() {
            bail!("{msg}");
        }
        Ok(())
    }

    /// `force`: always probe again when the daemon is currently down (user
    /// action such as backup). Periodic callers pass `false` and honor TTL.
    async fn refresh(&self, force: bool) {
        {
            let g = self.inner.read().await;
            let ttl = if g.available() {
                REFRESH_WHEN_UP
            } else {
                REFRESH_WHEN_DOWN
            };
            let stale = g.checked_at.elapsed() >= ttl;
            if g.available() {
                if !stale {
                    return;
                }
            } else if !force && !stale {
                return;
            }
        }
        let next = detect_now().await;
        let mut g = self.inner.write().await;
        let ttl = if g.available() {
            REFRESH_WHEN_UP
        } else {
            REFRESH_WHEN_DOWN
        };
        if !force && g.checked_at.elapsed() < ttl {
            return;
        }
        let was = g.available();
        let now = next.available();
        if !was && now {
            tracing::info!(
                version = ?next.version,
                compose = next.compose.as_str(),
                "docker daemon is now available"
            );
        } else if was && !now {
            tracing::warn!("docker daemon is no longer reachable");
        }
        *g = next;
    }

    pub async fn load_archive(&self, archive: &Path) -> Result<Vec<String>> {
        self.ensure().await?;
        let output = Command::new("docker")
            .args(["load", "-i"])
            .arg(archive)
            .output()
            .await
            .context("failed to spawn docker load")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            bail!(
                "docker load 失败（{}）：{}",
                archive.display(),
                stderr.trim().if_empty(stdout.trim())
            );
        }
        Ok(parse_loaded_images(&stdout))
    }

    pub async fn tag(&self, source: &str, target: &str) -> Result<()> {
        self.ensure().await?;
        if source == target {
            return Ok(());
        }
        let output = Command::new("docker")
            .args(["tag", source, target])
            .output()
            .await
            .context("failed to spawn docker tag")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("docker tag {source} -> {target} 失败：{}", stderr.trim());
        }
        Ok(())
    }

    pub async fn compose_up(&self, dir: &Path) -> Result<String> {
        self.compose_run(dir, &["up", "-d", "--remove-orphans"]).await
    }

    pub async fn compose_up_recreate(&self, dir: &Path, services: &[String]) -> Result<String> {
        let mut args = vec![
            "up".to_string(),
            "-d".to_string(),
            "--remove-orphans".to_string(),
            "--force-recreate".to_string(),
        ];
        args.extend(services.iter().cloned());
        self.compose_run_owned(dir, &args).await
    }

    pub async fn compose_down(&self, dir: &Path) -> Result<String> {
        self.compose_run(dir, &["down"]).await
    }

    pub async fn compose_restart(&self, dir: &Path) -> Result<String> {
        // `compose restart` does not take --remove-orphans and will not pick up
        // compose-file edits. Recreate the stack instead.
        self.compose_run(
            dir,
            &["up", "-d", "--force-recreate", "--remove-orphans"],
        )
        .await
    }

    pub async fn compose_restart_service(&self, dir: &Path, service: &str) -> Result<String> {
        validate_service_name(service)?;
        self.compose_run(dir, &["restart", "--", service]).await
    }

    pub async fn compose_config_file(&self, dir: &Path, file_name: &str) -> Result<String> {
        if file_name.is_empty()
            || file_name.contains('/')
            || file_name.contains('\\')
            || file_name.contains("..")
        {
            bail!("无效的 Compose 文件名");
        }
        self.compose_run(dir, &["-f", file_name, "config"]).await
    }

    pub async fn compose_ps_raw(&self, dir: &Path) -> Result<String> {
        match self.compose_run(dir, &["ps", "--format", "json"]).await {
            Ok(s) => Ok(s),
            Err(_) => self.compose_run(dir, &["ps"]).await,
        }
    }

    pub async fn compose_logs(
        &self,
        dir: &Path,
        tail: u32,
        service: Option<&str>,
    ) -> Result<String> {
        let tail = tail.to_string();
        let mut args = vec![
            "logs".to_string(),
            "--tail".to_string(),
            tail,
        ];
        if let Some(name) = service {
            validate_service_name(name)?;
            args.push(name.to_string());
        }
        self.compose_run_owned(dir, &args).await
    }

    pub async fn compose_exec_output(
        &self,
        dir: &Path,
        service: &str,
        user: Option<&str>,
        env: &[(&str, &str)],
        command: &[String],
        timeout: Duration,
    ) -> Result<String> {
        self.ensure().await?;
        validate_service_name(service)?;
        if command.is_empty() {
            bail!("exec 命令为空");
        }
        if let Some(u) = user {
            if !is_safe_unix_user(u) {
                bail!("无效的容器用户");
            }
        }
        let compose = self.inner.read().await.compose.clone();
        let mut args = vec!["exec".to_string(), "-T".to_string()];
        if let Some(u) = user {
            args.push("-u".into());
            args.push(u.into());
        }
        for (k, v) in env {
            if !is_safe_env_key(k) {
                bail!("无效的环境变量名");
            }
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.push(service.into());
        args.extend(command.iter().cloned());

        let mut cmd = match compose {
            ComposeKind::Plugin => {
                let mut cmd = Command::new("docker");
                cmd.arg("compose");
                cmd
            }
            ComposeKind::Standalone => Command::new("docker-compose"),
            ComposeKind::Missing => bail!("本机未安装 docker compose"),
        };
        cmd.args(&args)
            .current_dir(dir)
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("容器命令执行超时"))?
            .context("failed to spawn docker compose exec")?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !output.status.success() {
            let shown: Vec<String> = args
                .iter()
                .map(|a| {
                    if a.starts_with("PGPASSWORD=") {
                        "PGPASSWORD=***".into()
                    } else {
                        a.clone()
                    }
                })
                .collect();
            bail!(
                "compose {} 失败：{}",
                shown.join(" "),
                stderr.trim().if_empty(stdout.trim())
            );
        }
        if stdout.trim().is_empty() {
            Ok(stderr)
        } else {
            Ok(stdout)
        }
    }

    pub async fn compose_exec_argv(&self, service: &str) -> Result<(String, Vec<String>)> {
        self.ensure().await?;
        validate_service_name(service)?;
        let compose = self.inner.read().await.compose.clone();
        match compose {
            ComposeKind::Plugin => Ok((
                "docker".into(),
                vec![
                    "compose".into(),
                    "exec".into(),
                    "-it".into(),
                    service.into(),
                    "/bin/sh".into(),
                ],
            )),
            ComposeKind::Standalone => Ok((
                "docker-compose".into(),
                vec![
                    "exec".into(),
                    "-it".into(),
                    service.into(),
                    "/bin/sh".into(),
                ],
            )),
            ComposeKind::Missing => bail!("本机未安装 docker compose"),
        }
    }

    async fn compose_run(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        self.compose_run_owned(dir, &owned).await
    }

    async fn compose_run_owned(&self, dir: &Path, args: &[String]) -> Result<String> {
        self.ensure().await?;
        let compose = self.inner.read().await.compose.clone();
        let output = match compose {
            ComposeKind::Plugin => {
                let mut cmd = Command::new("docker");
                cmd.arg("compose").args(args).current_dir(dir);
                cmd.output().await.context("failed to spawn docker compose")?
            }
            ComposeKind::Standalone => {
                let mut cmd = Command::new("docker-compose");
                cmd.args(args).current_dir(dir);
                cmd.output()
                    .await
                    .context("failed to spawn docker-compose")?
            }
            ComposeKind::Missing => bail!("本机未安装 docker compose"),
        };
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !output.status.success() {
            bail!(
                "compose {} 失败：{}",
                args.join(" "),
                stderr.trim().if_empty(stdout.trim())
            );
        }
        if stdout.trim().is_empty() {
            Ok(stderr)
        } else if stderr.trim().is_empty() {
            Ok(stdout)
        } else {
            Ok(format!("{stdout}\n{stderr}"))
        }
    }
}

pub fn parse_loaded_images(stdout: &str) -> Vec<String> {
    let mut images = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Loaded image: ") {
            let name = rest.trim();
            if !name.is_empty() {
                images.push(name.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("Loaded image ID: ") {
            let name = rest.trim();
            if !name.is_empty() {
                images.push(name.to_string());
            }
        }
    }
    images
}

/// `repo/name:tag` -> `repo/name:latest`. Untagged image IDs are skipped.
pub fn to_latest_tag(image: &str) -> Option<String> {
    if image.starts_with("sha256:") {
        return None;
    }
    let slash = image.rfind('/').map(|i| i + 1).unwrap_or(0);
    let colon = image[slash..].rfind(':');
    let name = match colon {
        Some(i) => &image[..slash + i],
        None => image,
    };
    if name.is_empty() {
        return None;
    }
    Some(format!("{name}:latest"))
}

pub fn validate_service_name(name: &str) -> Result<()> {
    if !is_safe_service_name(name) {
        bail!("无效的服务名");
    }
    Ok(())
}

fn is_safe_unix_user(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && bytes[0].is_ascii_alphabetic()
        && bytes
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
}

fn is_safe_env_key(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphabetic()
        && bytes
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

pub fn is_safe_service_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && !name.contains("..")
        && bytes
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_' || *c == b'.')
}

pub fn parse_compose_ps(raw: &str) -> Vec<ComposeService> {
    let mut services = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let pick = |keys: &[&str]| {
            keys.iter()
                .find_map(|k| v.get(*k).and_then(|x| x.as_str()))
                .unwrap_or("")
                .to_string()
        };
        services.push(ComposeService {
            name: pick(&["Service", "Name", "Name"]),
            image: pick(&["Image"]),
            state: pick(&["State"]),
            status: pick(&["Status"]),
        });
    }
    services
}

async fn detect_now() -> Snapshot {
    let client = docker_version_field("{{.Client.Version}}", false).await;
    let server = docker_version_field("{{.Server.Version}}", true).await;
    let cli = client.is_ok() || server.is_ok();
    let daemon = server.is_ok();
    let version = server.ok().or_else(|| client.ok());
    let compose = if !daemon {
        ComposeKind::Missing
    } else if command_ok("docker", &["compose", "version"]).await {
        ComposeKind::Plugin
    } else if command_ok("docker-compose", &["version"]).await {
        ComposeKind::Standalone
    } else {
        ComposeKind::Missing
    };
    Snapshot {
        cli,
        daemon,
        version,
        compose,
        checked_at: Instant::now(),
    }
}

/// `require_success` is for the server field (daemon must be up). Client
/// version is accepted from stdout even when `docker version` exits 1 because
/// the daemon is down.
async fn docker_version_field(fmt: &str, require_success: bool) -> Result<String> {
    let output = Command::new("docker")
        .args(["version", "--format", fmt])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("docker not found")?;
    let v = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if require_success && !output.status.success() {
        bail!("docker version failed");
    }
    if v.is_empty() || v == "<no value>" {
        bail!("empty docker version");
    }
    Ok(v)
}

async fn command_ok(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

trait IfEmpty {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str;
}

impl IfEmpty for str {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str {
        if self.is_empty() {
            fallback
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_error_distinguishes_missing_cli_and_daemon() {
        let down = Snapshot {
            cli: true,
            daemon: false,
            version: Some("27.0.0".into()),
            compose: ComposeKind::Missing,
            checked_at: Instant::now(),
        };
        assert_eq!(
            down.ready_error(),
            Some("Docker 守护进程尚未就绪，请先启动 docker")
        );
        assert!(!down.available());
        assert_eq!(down.to_meta().version.as_deref(), Some("27.0.0"));

        let missing = Snapshot {
            cli: false,
            daemon: false,
            version: None,
            compose: ComposeKind::Missing,
            checked_at: Instant::now(),
        };
        assert_eq!(missing.ready_error(), Some("本机未安装 docker 命令"));

        let ok = Snapshot {
            cli: true,
            daemon: true,
            version: Some("27.0.0".into()),
            compose: ComposeKind::Plugin,
            checked_at: Instant::now(),
        };
        assert_eq!(ok.ready_error(), None);
        assert!(ok.available());
    }

    #[test]
    fn latest_tags() {
        assert_eq!(
            to_latest_tag("my/app:1.2.3").as_deref(),
            Some("my/app:latest")
        );
        assert_eq!(
            to_latest_tag("registry.local:5000/app:v2").as_deref(),
            Some("registry.local:5000/app:latest")
        );
        assert_eq!(to_latest_tag("sha256:abc"), None);
        assert_eq!(to_latest_tag("nginx").as_deref(), Some("nginx:latest"));
    }

    #[test]
    fn service_name_allows_compose_ids() {
        assert!(is_safe_service_name("web"));
        assert!(is_safe_service_name("cis-server"));
        assert!(is_safe_service_name("db.1"));
        assert!(!is_safe_service_name(""));
        assert!(!is_safe_service_name("../etc"));
        assert!(!is_safe_service_name("a/b"));
        assert!(!is_safe_service_name("web;reboot"));
    }

    #[test]
    fn parse_load_output() {
        let out = "Loaded image: foo/bar:1.0\nLoaded image: foo/bar:1.0-dbg\n";
        assert_eq!(
            parse_loaded_images(out),
            vec!["foo/bar:1.0".to_string(), "foo/bar:1.0-dbg".to_string()]
        );
    }
}
