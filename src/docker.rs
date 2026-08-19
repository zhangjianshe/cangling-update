use crate::models::{ComposeService, DockerMeta};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

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
pub struct Docker {
    pub available: bool,
    pub version: Option<String>,
    pub compose: ComposeKind,
}

impl Docker {
    pub async fn detect() -> Self {
        let version = docker_version().await.ok();
        let available = version.is_some();
        let compose = if !available {
            ComposeKind::Missing
        } else if command_ok("docker", &["compose", "version"]).await {
            ComposeKind::Plugin
        } else if command_ok("docker-compose", &["version"]).await {
            ComposeKind::Standalone
        } else {
            ComposeKind::Missing
        };
        Self {
            available,
            version,
            compose,
        }
    }

    pub fn meta(&self) -> DockerMeta {
        DockerMeta {
            available: self.available,
            version: self.version.clone(),
            compose: self.compose.as_str().to_string(),
        }
    }

    pub fn require(&self) -> Result<()> {
        if !self.available {
            bail!("本机未安装 docker 命令");
        }
        Ok(())
    }

    pub async fn load_archive(&self, archive: &Path) -> Result<Vec<String>> {
        self.require()?;
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
        self.require()?;
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
        self.compose_run(dir, &["restart"]).await
    }

    pub async fn compose_ps_raw(&self, dir: &Path) -> Result<String> {
        match self.compose_run(dir, &["ps", "--format", "json"]).await {
            Ok(s) => Ok(s),
            Err(_) => self.compose_run(dir, &["ps"]).await,
        }
    }

    pub async fn compose_logs(&self, dir: &Path, tail: u32) -> Result<String> {
        let tail = tail.to_string();
        self.compose_run(dir, &["logs", "--no-color", "--tail", &tail])
            .await
    }

    async fn compose_run(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        self.compose_run_owned(dir, &owned).await
    }

    async fn compose_run_owned(&self, dir: &Path, args: &[String]) -> Result<String> {
        self.require()?;
        let output = match self.compose {
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

async fn docker_version() -> Result<String> {
    let output = Command::new("docker")
        .args(["version", "--format", "{{.Client.Version}}"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("docker not found")?;
    if !output.status.success() {
        bail!("docker version failed");
    }
    let v = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if v.is_empty() {
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
    fn parse_load_output() {
        let out = "Loaded image: foo/bar:1.0\nLoaded image: foo/bar:1.0-dbg\n";
        assert_eq!(
            parse_loaded_images(out),
            vec!["foo/bar:1.0".to_string(), "foo/bar:1.0-dbg".to_string()]
        );
    }
}
