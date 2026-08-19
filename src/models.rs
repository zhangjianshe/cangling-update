use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub description: String,
    pub directory: String,
    pub created_at: String,
    pub updated_at: String,
    pub current_version_no: Option<i64>,
    pub current_version_id: Option<String>,
    pub version_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    pub id: String,
    pub project_id: String,
    pub version_no: i64,
    pub label: String,
    pub note: String,
    pub backup_path: String,
    pub images: Vec<LoadedImage>,
    pub jars: Vec<DeployedJar>,
    pub is_current: bool,
    pub created_at: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoadedImage {
    pub file: String,
    pub loaded: Vec<String>,
    pub latest_tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeployedJar {
    pub file: String,
    pub dest: String,
    pub services: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateProject {
    pub name: String,
    pub description: Option<String>,
    pub directory: String,
    pub job_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateProject {
    pub name: Option<String>,
    pub description: Option<String>,
    pub directory: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RollbackBody {
    pub version_id: String,
    #[serde(default)]
    pub restart: bool,
    pub job_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ValidateDirBody {
    pub directory: String,
}

#[derive(Debug, Serialize)]
pub struct ValidateDirResult {
    pub ok: bool,
    pub directory: String,
    pub compose_file: Option<String>,
    pub images: Vec<String>,
    pub jar_mounts: Vec<crate::paths::JarMount>,
    pub warning: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Meta {
    pub name: &'static str,
    pub version: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
    pub exe_dir: String,
    pub config_dir: String,
    pub db_path: String,
    pub port: u16,
    pub docker: DockerMeta,
}

#[derive(Debug, Clone, Serialize)]
pub struct DockerMeta {
    pub available: bool,
    pub version: Option<String>,
    pub compose: String,
}

#[derive(Debug, Serialize)]
pub struct ComposeStatus {
    pub available: bool,
    pub compose_file: Option<String>,
    pub images: Vec<String>,
    pub jar_mounts: Vec<crate::paths::JarMount>,
    pub services: Vec<ComposeService>,
    pub raw: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComposeService {
    pub name: String,
    pub image: String,
    pub state: String,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct UpdateResult {
    pub version: Version,
    pub loaded: Vec<LoadedImage>,
    pub jars: Vec<DeployedJar>,
}

#[derive(Debug, Serialize)]
pub struct LogsResult {
    pub logs: String,
}

#[derive(Debug, Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct AuthUser {
    pub id: String,
    pub username: String,
}

#[derive(Debug, Serialize)]
pub struct AuthStatus {
    pub needs_setup: bool,
    pub user: Option<AuthUser>,
    pub idle_timeout_secs: u64,
}
