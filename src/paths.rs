use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Layout next to the executable:
///
/// ```text
/// cangling-update          # this binary
/// logs/cangling-update.log # application log
/// config/
///   cangling.db            # sqlite
///   backups/<project>/<version>/
///   uploads/               # in-flight multipart
///   portal/                # homepage background and item icons
/// ```
#[derive(Clone, Debug)]
pub struct AppPaths {
    pub exe_dir: PathBuf,
    pub config_dir: PathBuf,
    pub db_path: PathBuf,
    pub backups_dir: PathBuf,
    pub uploads_dir: PathBuf,
    pub portal_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub log_file: PathBuf,
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
        let portal_dir = config_dir.join("portal");
        std::fs::create_dir_all(&backups_dir)
            .with_context(|| format!("create backups dir {}", backups_dir.display()))?;
        std::fs::create_dir_all(&uploads_dir)
            .with_context(|| format!("create uploads dir {}", uploads_dir.display()))?;
        std::fs::create_dir_all(&portal_dir)
            .with_context(|| format!("create portal dir {}", portal_dir.display()))?;
        std::fs::create_dir_all(portal_dir.join("icons"))
            .with_context(|| format!("create portal icons dir {}", portal_dir.display()))?;

        let logs_dir = exe_dir.join("logs");
        std::fs::create_dir_all(&logs_dir)
            .with_context(|| format!("create log dir {}", logs_dir.display()))?;
        let log_file = logs_dir.join("cangling-update.log");

        Ok(Self {
            db_path: config_dir.join("cangling.db"),
            exe_dir,
            config_dir,
            backups_dir,
            uploads_dir,
            portal_dir,
            logs_dir,
            log_file,
        })
    }

    pub fn portal_icons_dir(&self) -> PathBuf {
        self.portal_dir.join("icons")
    }

    pub fn version_dir(&self, project_id: &str, version_id: &str) -> PathBuf {
        self.backups_dir.join(project_id).join(version_id)
    }

    pub fn version_tree(&self, project_id: &str, version_id: &str) -> PathBuf {
        self.version_dir(project_id, version_id).join("tree.gitref")
    }

    pub fn version_images(&self, project_id: &str, version_id: &str) -> PathBuf {
        self.version_dir(project_id, version_id).join("images")
    }

    pub fn version_jars(&self, project_id: &str, version_id: &str) -> PathBuf {
        self.version_dir(project_id, version_id).join("jars")
    }

    pub fn project_backup_root(&self, project_id: &str) -> PathBuf {
        self.backups_dir.join(project_id)
    }
}

pub fn require_absolute_dir(path: &str) -> Result<PathBuf> {
    let path = path.trim();
    if path.is_empty() {
        bail!("请填写目录路径");
    }
    let p = PathBuf::from(path);
    if !p.is_absolute() {
        bail!("目录必须是本机绝对路径");
    }
    if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        bail!("目录路径不能包含 '..'");
    }
    if !p.exists() {
        bail!("目录不存在：{}", p.display());
    }
    if !p.is_dir() {
        bail!("路径不是目录：{}", p.display());
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

pub const COMPOSE_MAX_BYTES: usize = 1024 * 1024;

pub fn compose_live_path(dir: &Path) -> (PathBuf, String, bool) {
    match find_compose_file(dir) {
        Some(path) => {
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "docker-compose.yml".into());
            (path, name, true)
        }
        None => (
            dir.join("docker-compose.yml"),
            "docker-compose.yml".into(),
            false,
        ),
    }
}

pub fn compose_etag(content: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in content.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}-{}", h, content.len())
}

pub fn validate_compose_text(content: &str) -> Result<()> {
    if content.len() > COMPOSE_MAX_BYTES {
        bail!("Compose 文件不能超过 1 MB");
    }
    if content.trim().is_empty() {
        bail!("Compose 文件不能为空");
    }
    if content.as_bytes().contains(&0) {
        bail!("Compose 文件不能包含空字节");
    }
    match serde_yaml::from_str::<serde_yaml::Value>(content) {
        Ok(serde_yaml::Value::Mapping(map)) => {
            if map.is_empty() {
                bail!("Compose 文件不能是空的 YAML 对象");
            }
            Ok(())
        }
        Ok(_) => bail!("Compose 文件必须是 YAML 映射（例如包含 services:）"),
        Err(err) => bail!("YAML 无法解析：{err}"),
    }
}

pub fn compose_draft_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "docker-compose.yml".into());
    path.with_file_name(format!("{name}.cangling-draft"))
}

pub fn write_text_atomic(path: &Path, content: &str) -> Result<()> {
    let draft = compose_draft_path(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&draft, content).with_context(|| format!("write {}", draft.display()))?;
    commit_compose_draft(path, &draft)
}

pub fn commit_compose_draft(dest: &Path, draft: &Path) -> Result<()> {
    if dest.exists() {
        copy_file_meta(dest, draft)?;
    }
    std::fs::rename(draft, dest).with_context(|| format!("replace {}", dest.display()))
}

fn copy_file_meta(from: &Path, to: &Path) -> Result<()> {
    let meta = std::fs::metadata(from)?;
    std::fs::set_permissions(to, meta.permissions())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = std::os::unix::fs::chown(to, Some(meta.uid()), Some(meta.gid()));
    }
    Ok(())
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
        bail!("无效的文件名");
    }
    if base.chars().any(|c| matches!(c, '/' | '\\' | '\0')) {
        bail!("无效的文件名");
    }
    Ok(base.to_string())
}

pub fn is_image_archive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".tar.gz") || lower.ends_with(".tgz") || lower.ends_with(".tar")
}

pub fn is_jar_name(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".jar")
}

pub fn is_uploadable_name(name: &str) -> bool {
    is_image_archive_name(name) || is_jar_name(name)
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct JarMount {
    pub service: String,
    pub host_path: String,
    pub basename: String,
}

pub fn parse_compose_jar_mounts(compose_text: &str) -> Vec<JarMount> {
    let mut current_service = String::new();
    let mut mounts = Vec::new();
    for raw in compose_text.lines() {
        if leading_spaces(raw) == 2 {
            if let Some(name) = raw.trim().strip_suffix(':') {
                if !name.is_empty() && !name.contains(' ') && !name.contains(':') && !name.starts_with('#')
                {
                    current_service = name.to_string();
                }
            }
        }
        let Some(host) = volume_host_path(raw) else {
            continue;
        };
        let Some(base) = std::path::Path::new(&host)
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if !is_jar_name(&base) || current_service.is_empty() {
            continue;
        }
        mounts.push(JarMount {
            service: current_service.clone(),
            host_path: host,
            basename: base,
        });
    }
    mounts
}

pub fn resolve_host_path(project_dir: &Path, host_rel: &str) -> PathBuf {
    let p = host_rel.trim();
    let p = p.strip_prefix("./").unwrap_or(p);
    let path = PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        project_dir.join(path)
    }
}

fn leading_spaces(s: &str) -> usize {
    s.chars().take_while(|c| *c == ' ').count()
}

fn volume_host_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let item = trimmed.strip_prefix('-')?.trim();
    let item = item.trim_matches(|c| c == '"' || c == '\'');
    if !item.contains(".jar") {
        return None;
    }
    let host = item.split(':').next()?.trim();
    if host.is_empty() {
        return None;
    }
    Some(host.to_string())
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

    #[test]
    fn parse_np5_style_jar_mounts() {
        let yaml = r#"
services:
  cis-server:
    image: hub.example/gdal-base:v4
    volumes:
      - ./jars/cis-server-1.0.0.jar:/app/app.jar:ro
  cis-k8s:
    volumes:
      - ./jars/cis-k8s-1.0.0.jar:/app/app.jar:ro
"#;
        let mounts = parse_compose_jar_mounts(yaml);
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].service, "cis-server");
        assert_eq!(mounts[0].basename, "cis-server-1.0.0.jar");
        assert_eq!(mounts[1].service, "cis-k8s");
    }

    #[test]
    fn compose_etag_stable_and_size_sensitive() {
        assert_eq!(compose_etag("a"), compose_etag("a"));
        assert_ne!(compose_etag("a"), compose_etag("b"));
        assert_ne!(compose_etag("ab"), compose_etag("a"));
        assert!(compose_etag("hello").contains("-5"));
    }

    #[test]
    fn validate_compose_text_rejects_empty_and_nul() {
        assert!(validate_compose_text("").is_err());
        assert!(validate_compose_text("   \n").is_err());
        assert!(validate_compose_text("services:\n  web:\n    image: n\0ginx\n").is_err());
        assert!(validate_compose_text("not: [ yaml").is_err());
        assert!(validate_compose_text("- just a list\n").is_err());
        assert!(validate_compose_text("services:\n  web:\n    image: nginx\n").is_ok());
    }

    #[test]
    fn write_text_atomic_replaces_and_cleans_draft() {
        let dir = std::env::temp_dir().join(format!(
            "cangling-compose-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("docker-compose.yml");
        std::fs::write(&path, "old:\n  x: 1\n").unwrap();
        write_text_atomic(&path, "services:\n  web:\n    image: nginx\n").unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        assert!(got.contains("nginx"));
        assert!(!compose_draft_path(&path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
