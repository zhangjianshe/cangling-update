//! Host-side NFS service used as the small-cluster shared storage provider.
//!
//! The service is deliberately managed by Docker Compose outside K3s so it can
//! be available before any Cangling workload mounts shared data or stages JARs.

use crate::error::AppError;
use crate::paths::write_text_atomic;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

const COMPOSE_DIR: &str = "/opt/local/nfs";
const COMPOSE_FILE: &str = "docker-compose.yaml";
const CONFIG_FILE: &str = "cangling-internal-nfs.json";
const DEFAULT_DATA_DIR: &str = "/opt/local/nfs/data";
const DEFAULT_IMAGE: &str = "hub.cangling.cn/cangling/nfs-server:1.0";
const LEGACY_DEFAULT_IMAGE: &str = "itsthenetwork/nfs-server-alpine:12";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InternalNfsConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default = "default_image")]
    pub image: String,
    #[serde(default = "default_namespace")]
    pub namespace: String,
    #[serde(default)]
    pub server_address: String,
    #[serde(default = "default_export_path")]
    pub export_path: String,
}

impl Default for InternalNfsConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            image: default_image(),
            namespace: default_namespace(),
            server_address: String::new(),
            export_path: default_export_path(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InternalNfsStatus {
    config: InternalNfsConfig,
    detected_server_address: String,
    compose_dir: &'static str,
    compose_file: String,
    export_root: String,
    configured: bool,
    running: bool,
    state: &'static str,
    detail: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    action_output: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/cangling/internal-nfs", get(status))
        .route("/api/cangling/internal-nfs/start", post(start))
        .route("/api/cangling/internal-nfs/stop", post(stop))
}

async fn status(State(state): State<AppState>) -> Result<Json<InternalNfsStatus>, AppError> {
    Ok(Json(read_status(&state, String::new()).await?))
}

async fn start(
    State(state): State<AppState>,
    Json(mut config): Json<InternalNfsConfig>,
) -> Result<Json<InternalNfsStatus>, AppError> {
    if config.server_address.trim().is_empty() {
        config.server_address = detect_server_address().await;
    }
    config.export_path = normalize_export_path(&config.export_path)?;
    validate_config(&config)?;
    let compose_dir = Path::new(COMPOSE_DIR);
    let data_dir = Path::new(&config.data_dir);
    std::fs::create_dir_all(data_dir)
        .map_err(|error| path_error("创建 NFS 数据目录", data_dir, error))?;
    let export_directory = data_dir.join(config.export_path.trim_start_matches('/'));
    for category in ["shared", "jars"] {
        let directory = export_directory.join(category).join(&config.namespace);
        std::fs::create_dir_all(&directory)
            .map_err(|error| path_error("创建 NFS 命名空间目录", &directory, error))?;
    }
    std::fs::create_dir_all(compose_dir)
        .map_err(|error| path_error("创建 NFS Compose 目录", compose_dir, error))?;

    let compose = render_compose(&config)?;
    write_text_atomic(&compose_dir.join(COMPOSE_FILE), &compose).map_err(AppError::from)?;
    save_config(&state, &config)?;
    let output = state
        .docker
        .compose_up(compose_dir)
        .await
        .map_err(AppError::from)?;
    Ok(Json(read_status(&state, bounded(output)).await?))
}

async fn stop(State(state): State<AppState>) -> Result<Json<InternalNfsStatus>, AppError> {
    let compose_dir = Path::new(COMPOSE_DIR);
    if !compose_dir.join(COMPOSE_FILE).is_file() {
        return Err(AppError::bad("集群内 NFS 尚未生成 Docker Compose 配置"));
    }
    let output = state
        .docker
        .compose_down(compose_dir)
        .await
        .map_err(AppError::from)?;
    Ok(Json(read_status(&state, bounded(output)).await?))
}

async fn read_status(
    state: &AppState,
    action_output: String,
) -> Result<InternalNfsStatus, AppError> {
    let detected_server_address = detect_server_address().await;
    let mut config = load_config(state)?;
    if config.server_address.trim().is_empty() {
        config.server_address = detected_server_address.clone();
    }
    let export_root = config.export_path.clone();
    let compose_file = Path::new(COMPOSE_DIR).join(COMPOSE_FILE);
    if !compose_file.is_file() {
        return Ok(InternalNfsStatus {
            config,
            detected_server_address,
            compose_dir: COMPOSE_DIR,
            compose_file: compose_file.display().to_string(),
            export_root,
            configured: false,
            running: false,
            state: "unconfigured",
            detail: "尚未生成 Docker Compose 配置".into(),
            action_output,
        });
    }

    match state.docker.compose_ps_raw(Path::new(COMPOSE_DIR)).await {
        Ok(output) => {
            let running = compose_output_is_running(&output);
            Ok(InternalNfsStatus {
                config,
                detected_server_address,
                compose_dir: COMPOSE_DIR,
                compose_file: compose_file.display().to_string(),
                export_root: export_root.clone(),
                configured: true,
                running,
                state: if running { "running" } else { "stopped" },
                detail: if running {
                    "NFSv4 服务正在运行，TCP 2049 已发布".into()
                } else {
                    "Compose 已配置，但 NFS 容器未运行".into()
                },
                action_output,
            })
        }
        Err(error) => Ok(InternalNfsStatus {
            config,
            detected_server_address,
            compose_dir: COMPOSE_DIR,
            compose_file: compose_file.display().to_string(),
            export_root,
            configured: true,
            running: false,
            state: "unavailable",
            detail: format!("无法读取 NFS 容器状态：{error}"),
            action_output,
        }),
    }
}

fn config_path(state: &AppState) -> PathBuf {
    state.paths.config_dir.join(CONFIG_FILE)
}

fn load_config(state: &AppState) -> Result<InternalNfsConfig, AppError> {
    let path = config_path(state);
    if !path.is_file() {
        return Ok(InternalNfsConfig::default());
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|error| path_error("读取集群内 NFS 配置", &path, error))?;
    let mut config: InternalNfsConfig = serde_json::from_str(&content)
        .map_err(|error| AppError::internal(format!("解析 {} 失败：{error}", path.display())))?;
    if config.image == LEGACY_DEFAULT_IMAGE {
        config.image = default_image();
    }
    config.export_path = normalize_export_path(&config.export_path)?;
    Ok(config)
}

fn save_config(state: &AppState, config: &InternalNfsConfig) -> Result<(), AppError> {
    let path = config_path(state);
    let content = serde_json::to_string_pretty(config)
        .map_err(|error| AppError::internal(format!("序列化集群内 NFS 配置失败：{error}")))?;
    write_text_atomic(&path, &(content + "\n")).map_err(AppError::from)
}

fn render_compose(config: &InternalNfsConfig) -> Result<String, AppError> {
    validate_config(config)?;
    let document = serde_json::json!({
        "services": {
            "nfs": {
                "image": config.image,
                "container_name": "cangling-internal-nfs",
                "restart": "unless-stopped",
                "privileged": true,
                "environment": {
                    "SHARED_DIRECTORY": "/nfsshare",
                    "SYNC": "true"
                },
                "ports": ["2049:2049/tcp"],
                "volumes": [{
                    "type": "bind",
                    "source": config.data_dir,
                    "target": "/nfsshare",
                    "bind": { "create_host_path": true }
                }],
                "labels": {
                    "cangling.cn/component": "internal-nfs"
                }
            }
        }
    });
    let mut yaml = String::from("# 由 cangling-update 管理；NFS 必须先于苍灵应用启动。\n");
    yaml.push_str(
        &serde_yaml::to_string(&document)
            .map_err(|error| AppError::internal(format!("生成 NFS Compose 配置失败：{error}")))?,
    );
    Ok(yaml)
}

fn validate_config(config: &InternalNfsConfig) -> Result<(), AppError> {
    let path = Path::new(config.data_dir.trim());
    if !path.is_absolute()
        || path == Path::new("/")
        || path == Path::new(COMPOSE_DIR)
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(AppError::bad(
            "NFS 数据目录必须是安全的绝对路径，且不能是 / 或 Compose 工作目录",
        ));
    }
    let image = config.image.trim();
    if image.is_empty()
        || image.len() > 255
        || image.contains("://")
        || image.chars().any(char::is_whitespace)
    {
        return Err(AppError::bad("NFS 镜像名称无效"));
    }
    let server_address = config.server_address.trim();
    if server_address.is_empty()
        || server_address.len() > 253
        || server_address
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.')))
    {
        return Err(AppError::bad("NFS 服务地址必须是有效的 IPv4 地址或主机名"));
    }
    normalize_export_path(&config.export_path)?;
    let namespace = config.namespace.as_bytes();
    if namespace.is_empty()
        || namespace.len() > 63
        || !namespace[0].is_ascii_alphanumeric()
        || !namespace[namespace.len() - 1].is_ascii_alphanumeric()
        || !namespace
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    {
        return Err(AppError::bad("命名空间必须是有效的 DNS 标签"));
    }
    Ok(())
}

fn normalize_export_path(value: &str) -> Result<String, AppError> {
    let value = value.trim();
    if value.is_empty() || !value.starts_with('/') || value.len() > 255 {
        return Err(AppError::bad("NFS export path 必须是以 / 开头的绝对路径"));
    }
    let mut components = Vec::new();
    for component in value.split('/').filter(|component| !component.is_empty()) {
        if matches!(component, "." | "..")
            || component
                .bytes()
                .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
        {
            return Err(AppError::bad(
                "NFS export path 仅允许字母、数字、-、_、. 和 /，且不能包含 . 或 .. 路径段",
            ));
        }
        components.push(component);
    }
    Ok(if components.is_empty() {
        "/".into()
    } else {
        format!("/{}", components.join("/"))
    })
}

async fn detect_server_address() -> String {
    crate::k3s_resources::local_node_internal_ip()
        .await
        .filter(|address| !address.trim().is_empty())
        .unwrap_or_else(crate::hostinfo::primary_ip)
}

fn compose_output_is_running(output: &str) -> bool {
    let compact: String = output
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    compact.contains("\"state\":\"running\"")
        || compact.contains("\"status\":\"up")
        || output.lines().any(|line| {
            let line = line.to_ascii_lowercase();
            line.contains("cangling-internal-nfs")
                && (line.contains("running") || line.contains("up"))
        })
}

fn bounded(mut value: String) -> String {
    const MAX: usize = 8 * 1024;
    if value.len() > MAX {
        value.truncate(MAX);
        value.push_str("\n…输出已截断");
    }
    value
}

fn path_error(action: &str, path: &Path, error: std::io::Error) -> AppError {
    AppError::internal(format!("{action} {} 失败：{error}", path.display()))
}

fn default_data_dir() -> String {
    DEFAULT_DATA_DIR.into()
}

fn default_image() -> String {
    DEFAULT_IMAGE.into()
}

fn default_namespace() -> String {
    "cangling".into()
}

fn default_export_path() -> String {
    "/".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_image_uses_cangling_multi_arch_registry() {
        assert_eq!(
            InternalNfsConfig::default().image,
            "hub.cangling.cn/cangling/nfs-server:1.0"
        );
    }

    #[test]
    fn compose_uses_fixed_service_shape_and_configured_directory() {
        let config = InternalNfsConfig {
            data_dir: "/srv/cangling/nfs".into(),
            image: "registry.example.com/nfs:v1".into(),
            namespace: "cangling-prod".into(),
            server_address: "10.0.0.8".into(),
            export_path: "/tenant-a".into(),
        };
        let yaml = render_compose(&config).unwrap();
        assert!(yaml.contains("container_name: cangling-internal-nfs"));
        assert!(yaml.contains("source: /srv/cangling/nfs"));
        assert!(yaml.contains("target: /nfsshare"));
        assert!(yaml.contains("2049:2049/tcp"));
        assert!(yaml.contains("privileged: true"));
    }

    #[test]
    fn unsafe_data_directories_are_rejected() {
        for directory in ["relative", "/", "/opt/local/nfs", "/data/../root"] {
            let config = InternalNfsConfig {
                data_dir: directory.into(),
                ..InternalNfsConfig::default()
            };
            assert!(validate_config(&config).is_err(), "accepted {directory}");
        }
    }

    #[test]
    fn invalid_server_addresses_are_rejected() {
        for address in ["", "http://10.0.0.8", "10.0.0.8:/", "bad host"] {
            let config = InternalNfsConfig {
                server_address: address.into(),
                ..InternalNfsConfig::default()
            };
            assert!(validate_config(&config).is_err(), "accepted {address}");
        }
        let config = InternalNfsConfig {
            server_address: "nfs.internal.example".into(),
            ..InternalNfsConfig::default()
        };
        assert!(validate_config(&config).is_ok());
    }

    #[test]
    fn export_path_defaults_to_root_and_normalizes_safe_subpaths() {
        assert_eq!(InternalNfsConfig::default().export_path, "/");
        assert_eq!(
            normalize_export_path("/tenant-a/maps/").unwrap(),
            "/tenant-a/maps"
        );
        for path in ["relative", "/../root", "/bad path", "/bad:port"] {
            assert!(normalize_export_path(path).is_err(), "accepted {path}");
        }
    }

    #[test]
    fn compose_status_understands_plugin_and_table_output() {
        assert!(compose_output_is_running(
            r#"{"Name":"cangling-internal-nfs","State":"running"}"#
        ));
        assert!(compose_output_is_running(
            "cangling-internal-nfs  image  command  service  Up 2 minutes"
        ));
        assert!(!compose_output_is_running(
            "NAME IMAGE COMMAND SERVICE CREATED STATUS PORTS"
        ));
    }
}
