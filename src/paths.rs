use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Layout next to the executable:
///
/// ```text
/// cangling-update          # this binary
/// config/
///   cangling.db            # sqlite
///   backups/<project>/<version>/
///   uploads/               # in-flight multipart
/// ```
#[derive(Clone, Debug)]
pub struct AppPaths {
    pub exe_dir: PathBuf,
    pub config_dir: PathBuf,
    pub db_path: PathBuf,
    pub backups_dir: PathBuf,
    pub uploads_dir: PathBuf,
}

impl AppPaths {
    pub fn resolve(data_dir: Option<PathBuf>) -> Result<Self> {
        let exe = std::env::current_exe().context("cannot resolve current executable")?;
        let exe_dir = exe
            .parent()
            .map(Path::to_path_buf)
            .context("executable has no parent directory")?;

        let config_dir = match data_dir {
            Some(p) => p,
            None => match std::env::var_os("CANGLING_HOME") {
                Some(home) => PathBuf::from(home),
                None => exe_dir.join("config"),
            },
        };

        std::fs::create_dir_all(&config_dir)
            .with_context(|| format!("create config dir {}", config_dir.display()))?;

        let backups_dir = config_dir.join("backups");
        let uploads_dir = config_dir.join("uploads");
        std::fs::create_dir_all(&backups_dir)
            .with_context(|| format!("create backups dir {}", backups_dir.display()))?;
        std::fs::create_dir_all(&uploads_dir)
            .with_context(|| format!("create uploads dir {}", uploads_dir.display()))?;

        Ok(Self {
            db_path: config_dir.join("cangling.db"),
            exe_dir,
            config_dir,
            backups_dir,
            uploads_dir,
        })
    }

    pub fn version_dir(&self, project_id: &str, version_id: &str) -> PathBuf {
        self.backups_dir.join(project_id).join(version_id)
    }

    pub fn version_tree(&self, project_id: &str, version_id: &str) -> PathBuf {
        self.version_dir(project_id, version_id).join("tree")
    }

    pub fn version_images(&self, project_id: &str, version_id: &str) -> PathBuf {
        self.version_dir(project_id, version_id).join("images")
    }

    pub fn project_backup_root(&self, project_id: &str) -> PathBuf {
        self.backups_dir.join(project_id)
    }
}

pub fn require_absolute_dir(path: &str) -> Result<PathBuf> {
    let path = path.trim();
    if path.is_empty() {
        bail!("directory path is required");
    }
    let p = PathBuf::from(path);
    if !p.is_absolute() {
        bail!("directory must be an absolute host path");
    }
    if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        bail!("directory path must not contain '..'");
    }
    if !p.exists() {
        bail!("directory does not exist: {}", p.display());
    }
    if !p.is_dir() {
        bail!("path is not a directory: {}", p.display());
    }
    Ok(p.canonicalize().unwrap_or(p))
}

pub const COMPOSE_FILENAMES: &[&str] = &[
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yml",
    "compose.yaml",
];

pub fn find_compose_file(dir: &Path) -> Option<PathBuf> {
    COMPOSE_FILENAMES
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.is_file())
}

pub fn parse_compose_images(compose_text: &str) -> Vec<String> {
    let mut images = Vec::new();
    for raw in compose_text.lines() {
        let line = raw.trim();
        let Some(rest) = line.strip_prefix("image:") else {
            continue;
        };
        let value = rest.trim().trim_matches(|c| c == '"' || c == '\'');
        if !value.is_empty() && !images.iter().any(|e: &String| e == value) {
            images.push(value.to_string());
        }
    }
    images
}

pub fn safe_filename(name: &str) -> Result<String> {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .trim();
    if base.is_empty() || base == "." || base == ".." {
        bail!("invalid file name");
    }
    if base.chars().any(|c| matches!(c, '/' | '\\' | '\0')) {
        bail!("invalid file name");
    }
    Ok(base.to_string())
}

pub fn is_image_archive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".tar.gz") || lower.ends_with(".tgz") || lower.ends_with(".tar")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_images_from_simple_yaml() {
        let yaml = r#"
services:
  web:
    image: nginx:1.25
  api:
    image: "my/app:2.0"
"#;
        assert_eq!(
            parse_compose_images(yaml),
            vec!["nginx:1.25".to_string(), "my/app:2.0".to_string()]
        );
    }

    #[test]
    fn rejects_path_in_filename() {
        assert_eq!(safe_filename("../x.tar.gz").unwrap(), "x.tar.gz");
        assert_eq!(safe_filename("a/b.tar.gz").unwrap(), "b.tar.gz");
        assert!(safe_filename("").is_err());
        assert!(safe_filename("..").is_err());
    }
}
