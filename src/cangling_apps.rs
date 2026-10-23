//! Editable Cangling application deployment defaults and manifest generation.
//!
//! The embedded template mirrors the production `cangling` namespace layout.
//! This module deliberately generates YAML only; applying it remains an explicit
//! operator action so configuration review and cluster mutation stay separate.

use crate::error::AppError;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::net::IpAddr;
use std::path::Path as FsPath;

const MANIFEST_TEMPLATE: &str = include_str!("assets/cangling-apps.yaml");
const DEFAULT_IMAGE_PREFIX: &str = "hub.cangling.cn/cangling";
const DEFAULT_NAMESPACE: &str = "cangling";
const DEFAULT_PRIMARY_NODE: &str = "n01";
const DEFAULT_HOST_IP: &str = "192.168.3.121";
const TEMPLATE_SHARED_ROOT: &str = "/mnt/cangling/cangling-share/storage";
const TEMPLATE_LOCAL_ROOT: &str = "/opt/cangling/local";
const TEMPLATE_CORE_ROOT: &str = "/opt/cangling-core";
const TEMPLATE_JAR_ROOT: &str = "/opt/cangling-np4/jars";
const DEFAULT_SHARED_ROOT: &str = "/opt/cl/share";
const DEFAULT_LOCAL_ROOT: &str = "/opt/cl/local";
const DEFAULT_CORE_ROOT: &str = "/opt/cl/core";
const DEFAULT_JAR_ROOT: &str = "/opt/cl/jars";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanglingConfig {
    #[serde(default = "default_image_prefix")]
    pub image_prefix: String,
    pub namespace: String,
    pub primary_node: String,
    pub host_ip: String,
    pub shared_root: String,
    pub local_root: String,
    pub core_root: String,
    pub jar_root: String,
    #[serde(default = "default_shared_storage")]
    pub shared_storage: StorageConfig,
    #[serde(default = "default_local_storage")]
    pub local_storage: StorageConfig,
    #[serde(default = "default_core_storage")]
    pub core_storage: StorageConfig,
    #[serde(default = "default_jar_storage")]
    pub jar_storage: StorageConfig,
    #[serde(default = "default_environment")]
    pub environment: BTreeMap<String, String>,
    #[serde(default = "default_secrets")]
    pub secrets: BTreeMap<String, String>,
    pub applications: Vec<ApplicationConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StorageConfig {
    #[serde(rename = "type")]
    pub storage_type: StorageType,
    #[serde(default)]
    pub server: String,
    /// 实际存储来源。本地目录使用绝对路径，NFS 使用 `server:/export/path`。
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StorageType {
    Local,
    Nfs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationConfig {
    pub id: String,
    pub title: String,
    pub group: ApplicationGroup,
    pub workload: WorkloadKind,
    pub replicas: u32,
    pub node: String,
    pub image: String,
    pub ports: Vec<u16>,
    pub storage: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ApplicationGroup {
    Single,
    Multi,
    Entry,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum WorkloadKind {
    Deployment,
    DaemonSet,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestResponse {
    filename: &'static str,
    yaml: String,
    warnings: Vec<String>,
}

pub fn routes() -> Router<crate::state::AppState> {
    Router::new()
        .route("/api/cangling/apps/defaults", get(defaults))
        .route("/api/cangling/apps/manifest", post(generate_manifest))
}

async fn defaults() -> Json<CanglingConfig> {
    Json(default_config())
}

async fn generate_manifest(
    Json(config): Json<CanglingConfig>,
) -> Result<Json<ManifestResponse>, AppError> {
    let (yaml, warnings) = render_manifest(&config)?;
    Ok(Json(ManifestResponse {
        filename: "cangling.yaml",
        yaml,
        warnings,
    }))
}

fn default_config() -> CanglingConfig {
    let namespace = DEFAULT_NAMESPACE;
    let shared_root = namespaced_root(DEFAULT_SHARED_ROOT, namespace);
    let local_root = namespaced_root(DEFAULT_LOCAL_ROOT, namespace);
    let core_root = namespaced_root(DEFAULT_CORE_ROOT, namespace);
    let jar_root = namespaced_root(DEFAULT_JAR_ROOT, namespace);
    let mut applications = vec![
        app(
            "cis-postgis",
            "PostGIS",
            ApplicationGroup::Single,
            WorkloadKind::Deployment,
            "postgis:17-3.5",
            &[5432],
            "/opt/cangling-core/postgis",
        ),
        app(
            "cangling-broker",
            "消息代理",
            ApplicationGroup::Single,
            WorkloadKind::Deployment,
            "cangling-broker:0.1.60",
            &[7500, 7501],
            "/opt/cangling-core/broker",
        ),
        app(
            "cangling-tile",
            "瓦片缓存",
            ApplicationGroup::Single,
            WorkloadKind::Deployment,
            "cangling-tile:0.1.20",
            &[7601],
            "/opt/cangling/local/tiledata",
        ),
        app(
            "cangling-map",
            "地图服务",
            ApplicationGroup::Multi,
            WorkloadKind::Deployment,
            "cangling-map:0.1.11",
            &[3000],
            "/opt/cangling/local/cangling-map",
        ),
        app(
            "cis-server",
            "业务服务",
            ApplicationGroup::Single,
            WorkloadKind::Deployment,
            "gdal-base:v4.2.35",
            &[8078],
            "/opt/cangling/local/ibcache",
        ),
        app(
            "cis-k8s",
            "算法调度",
            ApplicationGroup::Single,
            WorkloadKind::Deployment,
            "gdal-base:v4.2.35",
            &[8077],
            "/opt/cangling/local/ibcache",
        ),
        app(
            "cangling-nginx",
            "统一入口",
            ApplicationGroup::Entry,
            WorkloadKind::Deployment,
            "nginx-rtmp:v4.2.35",
            &[7600],
            "/opt/cangling/local/nginx-log",
        ),
        app(
            "cis-map",
            "地图计算节点",
            ApplicationGroup::Multi,
            WorkloadKind::Deployment,
            "gdal-base:v4.2.35",
            &[8079],
            "/opt/cangling/local/ibcache",
        ),
        app(
            "np4-base",
            "应用前端节点",
            ApplicationGroup::Multi,
            WorkloadKind::Deployment,
            "gdal-base:v4.2.35",
            &[8080],
            "/opt/cangling/local/base-logs",
        ),
    ];
    for application in &mut applications {
        application.storage = namespace_template_path(&application.storage, namespace);
    }
    CanglingConfig {
        image_prefix: default_image_prefix(),
        namespace: namespace.into(),
        primary_node: DEFAULT_PRIMARY_NODE.into(),
        host_ip: DEFAULT_HOST_IP.into(),
        shared_root: shared_root.clone(),
        local_root: local_root.clone(),
        core_root: core_root.clone(),
        jar_root: jar_root.clone(),
        shared_storage: storage(StorageType::Nfs, format!("{DEFAULT_HOST_IP}:{shared_root}")),
        local_storage: storage(StorageType::Local, local_root),
        core_storage: storage(StorageType::Local, core_root),
        jar_storage: storage(StorageType::Nfs, format!("{DEFAULT_HOST_IP}:{jar_root}")),
        environment: default_environment(),
        secrets: default_secrets(),
        applications,
    }
}

fn namespaced_root(root: &str, namespace: &str) -> String {
    format!("{}/{}", root.trim_end_matches('/'), namespace)
}

fn namespace_template_path(path: &str, namespace: &str) -> String {
    for (template_root, default_root) in [
        (TEMPLATE_SHARED_ROOT, DEFAULT_SHARED_ROOT),
        (TEMPLATE_LOCAL_ROOT, DEFAULT_LOCAL_ROOT),
        (TEMPLATE_CORE_ROOT, DEFAULT_CORE_ROOT),
        (TEMPLATE_JAR_ROOT, DEFAULT_JAR_ROOT),
    ] {
        if path == template_root {
            return namespaced_root(default_root, namespace);
        }
        if let Some(suffix) = path.strip_prefix(&format!("{template_root}/")) {
            return format!(
                "{}/{namespace}/{suffix}",
                default_root.trim_end_matches('/')
            );
        }
    }
    path.to_owned()
}

fn storage(storage_type: StorageType, source: impl Into<String>) -> StorageConfig {
    let source = source.into();
    StorageConfig {
        storage_type,
        server: if storage_type == StorageType::Nfs {
            source
                .split_once(':')
                .map(|(server, _)| server.to_string())
                .unwrap_or_else(|| DEFAULT_HOST_IP.into())
        } else {
            String::new()
        },
        source,
    }
}

fn default_shared_storage() -> StorageConfig {
    storage(StorageType::Nfs, String::new())
}

fn default_local_storage() -> StorageConfig {
    storage(StorageType::Local, String::new())
}

fn default_core_storage() -> StorageConfig {
    storage(StorageType::Local, String::new())
}

fn default_jar_storage() -> StorageConfig {
    storage(StorageType::Nfs, String::new())
}

fn app(
    id: &str,
    title: &str,
    group: ApplicationGroup,
    workload: WorkloadKind,
    image: impl Into<String>,
    ports: &[u16],
    storage: &str,
) -> ApplicationConfig {
    ApplicationConfig {
        id: id.into(),
        title: title.into(),
        group,
        workload,
        replicas: if workload == WorkloadKind::Deployment {
            1
        } else {
            0
        },
        node: if workload == WorkloadKind::Deployment {
            DEFAULT_PRIMARY_NODE.into()
        } else {
            String::new()
        },
        image: image.into(),
        ports: ports.to_vec(),
        storage: storage.into(),
    }
}

fn default_image_prefix() -> String {
    DEFAULT_IMAGE_PREFIX.into()
}

fn default_environment() -> BTreeMap<String, String> {
    template_string_map("cangling-env", "data")
}

fn default_secrets() -> BTreeMap<String, String> {
    template_string_map("cangling-secret", "stringData")
}

fn template_string_map(resource_name: &str, field: &str) -> BTreeMap<String, String> {
    let documents = parse_documents().expect("embedded cangling manifest must be valid YAML");
    let values = documents
        .iter()
        .find(|document| metadata_name(document) == Some(resource_name))
        .and_then(Value::as_mapping)
        .and_then(|document| document.get(key(field)))
        .and_then(Value::as_mapping)
        .unwrap_or_else(|| panic!("embedded {resource_name}.{field} must be a mapping"));
    values
        .iter()
        .map(|(name, value)| {
            (
                name.as_str()
                    .unwrap_or_else(|| panic!("embedded {resource_name}.{field} key must be text"))
                    .to_owned(),
                value
                    .as_str()
                    .unwrap_or_else(|| {
                        panic!("embedded {resource_name}.{field} value must be text")
                    })
                    .to_owned(),
            )
        })
        .collect()
}

fn image_with_prefix(prefix: &str, image: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    let image = image
        .strip_prefix(DEFAULT_IMAGE_PREFIX)
        .and_then(|suffix| suffix.strip_prefix('/'))
        .unwrap_or(image);
    format!("{prefix}/{image}")
}

fn render_manifest(config: &CanglingConfig) -> Result<(String, Vec<String>), AppError> {
    validate_config(config)?;
    let expected = default_config();
    let expected_ids: HashSet<&str> = expected
        .applications
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    let applications: Vec<ApplicationConfig> = config
        .applications
        .iter()
        .cloned()
        .map(|mut item| {
            item.image = image_with_prefix(&config.image_prefix, &item.image);
            item
        })
        .collect();
    let apps: BTreeMap<&str, &ApplicationConfig> = applications
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect();
    if apps.len() != expected_ids.len() || !expected_ids.iter().all(|id| apps.contains_key(id)) {
        return Err(AppError::bad("应用列表不完整，请恢复默认配置后重试"));
    }

    let replacements = [
        (TEMPLATE_SHARED_ROOT, config.shared_root.as_str()),
        (TEMPLATE_LOCAL_ROOT, config.local_root.as_str()),
        (TEMPLATE_CORE_ROOT, config.core_root.as_str()),
        (TEMPLATE_JAR_ROOT, config.jar_root.as_str()),
    ];
    let mut docs = parse_documents()?;
    let jobs = storage_mapping_jobs(config, &applications)?;
    let insert_at = docs
        .iter()
        .position(|doc| value_str(doc, &["kind"]) == Some("Namespace"))
        .map_or(0, |index| index + 1);
    docs.splice(insert_at..insert_at, jobs);
    for doc in &mut docs {
        rebase_template_images(doc, &config.image_prefix);
        replace_strings(doc, &replacements);
        patch_namespace(doc, &config.namespace);
        attach_shared_configuration(doc)?;
        if metadata_name(doc) == Some("cangling-env") {
            patch_environment(doc, config, &apps)?;
        } else if metadata_name(doc) == Some("cangling-secret") {
            patch_secret(doc, config)?;
        } else if metadata_name(doc) == Some("cangling-nginx")
            && value_str(doc, &["kind"]) == Some("ConfigMap")
        {
            patch_nginx_config(doc, &apps)?;
        }
        if let Some(name) = metadata_name(doc).map(ToOwned::to_owned) {
            if let Some(app) = apps.get(name.as_str()) {
                patch_application_document(doc, app, config)?;
            }
        }
        patch_storage_volumes(doc, config)?;
    }

    let mut yaml =
        String::from("# 由 cangling-update 生成，请在应用前检查密码、路径和稳定节点标识。\n");
    for (index, doc) in docs.iter().enumerate() {
        if index > 0 {
            yaml.push_str("---\n");
        }
        let rendered = serde_yaml::to_string(doc)
            .map_err(|error| AppError::internal(format!("序列化 cangling.yaml 失败：{error}")))?;
        yaml.push_str(rendered.trim_start_matches("---\n"));
        if !yaml.ends_with('\n') {
            yaml.push('\n');
        }
    }
    // A final parse catches accidental invalid output before it reaches the operator.
    for document in serde_yaml::Deserializer::from_str(&yaml) {
        Value::deserialize(document)
            .map_err(|error| AppError::internal(format!("生成的 YAML 校验失败：{error}")))?;
    }

    let mut warnings = Vec::new();
    if config
        .secrets
        .values()
        .any(|value| value.is_empty() || value == "CHANGE_ME")
    {
        warnings.push("Secret 仍包含 CHANGE_ME，部署前必须替换".into());
    }
    if config.core_storage.storage_type == StorageType::Local
        || config.local_storage.storage_type == StorageType::Local
    {
        warnings.push("本地存储依赖所选节点磁盘，请确认备份与节点调度策略".into());
    }
    warnings.push("JAR 会由 initContainer 复制到 emptyDir；更新发布源后需滚动重启 Pod".into());
    if local_storage_mappings(config)
        .iter()
        .any(|(source, target)| source != target)
    {
        warnings.push(
            "已生成节点存储准备 Job：本地 Value 与标准位置不同时会创建标准位置到实际目录的软链接"
                .into(),
        );
    }
    Ok((yaml, warnings))
}

fn local_storage_mappings(config: &CanglingConfig) -> Vec<(String, String)> {
    [
        (&config.shared_storage, config.shared_root.as_str()),
        (&config.local_storage, config.local_root.as_str()),
        (&config.core_storage, config.core_root.as_str()),
        (&config.jar_storage, config.jar_root.as_str()),
    ]
    .into_iter()
    .filter(|(storage, _)| storage.storage_type == StorageType::Local)
    .map(|(storage, target)| (storage_source(storage, target), target.to_string()))
    .collect()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// 为每个实际参与部署的节点生成一次性准备 Job。
///
/// Job 只挂载明确配置的来源目录与标准目录的父目录，不挂载宿主机根目录，
/// 也不需要 privileged。若两者不同，会在宿主机标准位置创建指向来源的软链接。
fn storage_mapping_jobs(
    config: &CanglingConfig,
    applications: &[ApplicationConfig],
) -> Result<Vec<Value>, AppError> {
    let mappings: Vec<(String, String)> = local_storage_mappings(config)
        .into_iter()
        .filter(|(source, target)| source != target)
        .collect();
    if mappings.is_empty() {
        return Ok(Vec::new());
    }

    let mut nodes = BTreeSet::new();
    for app in applications {
        if app.workload == WorkloadKind::Deployment {
            nodes.extend(
                deployment_nodes(app, &config.primary_node)
                    .into_iter()
                    .map(ToOwned::to_owned),
            );
        }
    }

    let mut command = String::from("set -eu\n");
    let mut volumes = Vec::new();
    let mut mounts = Vec::new();
    for (index, (source, target)) in mappings.iter().enumerate() {
        let target_path = FsPath::new(target);
        let parent = target_path
            .parent()
            .and_then(FsPath::to_str)
            .ok_or_else(|| AppError::bad("标准映射位置缺少有效父目录"))?;
        let name = target_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| AppError::bad("标准映射位置缺少有效目录名"))?;
        let source_volume = format!("source-{index}");
        let target_volume = format!("target-{index}");
        let source_mount = format!("/sources/{index}");
        let target_mount = format!("/targets/{index}");

        volumes.push(serde_json::json!({
            "name": source_volume,
            "hostPath": { "path": source, "type": "DirectoryOrCreate" }
        }));
        volumes.push(serde_json::json!({
            "name": target_volume,
            "hostPath": { "path": parent, "type": "DirectoryOrCreate" }
        }));
        mounts.push(serde_json::json!({
            "name": source_volume, "mountPath": source_mount, "readOnly": true
        }));
        mounts.push(serde_json::json!({
            "name": target_volume, "mountPath": target_mount
        }));
        let link = format!("{target_mount}/{name}");
        command.push_str(&format!(
            "if [ -L {link} ]; then [ \"$(readlink {link})\" = {source} ] || {{ echo \"软链接目标不一致: {target}\" >&2; exit 1; }}; elif [ -e {link} ]; then echo \"标准位置已存在且不是软链接: {target}\" >&2; exit 1; else ln -s -- {source} {link}; fi\n",
            link = shell_quote(&link),
            source = shell_quote(source),
            target = target,
        ));
    }

    let checksum = mappings
        .iter()
        .flat_map(|(source, target)| source.bytes().chain(target.bytes()))
        .fold(2_166_136_261_u32, |hash, byte| {
            hash.wrapping_mul(16_777_619) ^ u32::from(byte)
        });
    let image = image_with_prefix(&config.image_prefix, "gdal-base:v4.2.35");
    let mut docs = Vec::new();
    for node in nodes {
        let short_node: String = node.chars().take(24).collect();
        let name = format!("cangling-storage-map-{short_node}-{checksum:08x}");
        docs.push(
            serde_yaml::to_value(serde_json::json!({
                "apiVersion": "batch/v1",
                "kind": "Job",
                "metadata": { "name": name, "namespace": config.namespace },
                "spec": {
                    "backoffLimit": 3,
                    "ttlSecondsAfterFinished": 86400,
                    "template": {
                        "metadata": { "labels": { "app": "cangling-storage-map" } },
                        "spec": {
                            "restartPolicy": "OnFailure",
                            "affinity": {
                                "nodeAffinity": {
                                    "requiredDuringSchedulingIgnoredDuringExecution": {
                                        "nodeSelectorTerms": [
                                            { "matchExpressions": [{ "key": "cangling.cn/node-id", "operator": "In", "values": [node] }] },
                                            { "matchExpressions": [{ "key": "kubernetes.io/hostname", "operator": "In", "values": [node] }] }
                                        ]
                                    }
                                }
                            },
                            "containers": [{
                                "name": "storage-map",
                                "image": image,
                                "command": ["/bin/sh", "-c", command],
                                "volumeMounts": mounts
                            }],
                            "volumes": volumes
                        }
                    }
                }
            }))
            .map_err(|error| AppError::internal(format!("生成存储映射 Job 失败：{error}")))?,
        );
    }
    Ok(docs)
}

fn parse_documents() -> Result<Vec<Value>, AppError> {
    serde_yaml::Deserializer::from_str(MANIFEST_TEMPLATE)
        .map(|document| {
            Value::deserialize(document)
                .map_err(|error| AppError::internal(format!("解析内置部署模板失败：{error}")))
        })
        .collect()
}

fn validate_config(config: &CanglingConfig) -> Result<(), AppError> {
    let image_prefix = config.image_prefix.trim_end_matches('/');
    if image_prefix.is_empty()
        || image_prefix.starts_with('/')
        || image_prefix.contains("://")
        || image_prefix.chars().any(char::is_whitespace)
    {
        return Err(AppError::bad(
            "镜像前缀必须是 registry[:port]/namespace 格式，不包含协议或空格",
        ));
    }
    validate_dns_label(&config.namespace, "命名空间")?;
    validate_dns_label(&config.primary_node, "缺省部署节点")?;
    config
        .host_ip
        .parse::<IpAddr>()
        .map_err(|_| AppError::bad("主机地址必须是有效的 IPv4 或 IPv6 地址"))?;
    for (label, path, storage) in [
        (
            "外部共享存储目录",
            &config.shared_root,
            &config.shared_storage,
        ),
        ("本地数据目录", &config.local_root, &config.local_storage),
        ("核心数据目录", &config.core_root, &config.core_storage),
        ("JAR 目录", &config.jar_root, &config.jar_storage),
    ] {
        if !path.starts_with('/') || path.contains("..") {
            return Err(AppError::bad(format!("{label}必须是安全的绝对路径")));
        }
        let source = storage_source(storage, path);
        match storage.storage_type {
            StorageType::Local => {
                if !source.starts_with('/') || source.contains("..") {
                    return Err(AppError::bad(format!(
                        "{label}的 Value 必须是安全的本地绝对路径"
                    )));
                }
            }
            StorageType::Nfs => {
                parse_nfs_source(&source).map_err(|_| {
                    AppError::bad(format!("{label}的 Value 必须使用 server:/export/path 格式"))
                })?;
            }
        }
    }
    validate_variable_map(&config.environment, "环境变量")?;
    validate_variable_map(&config.secrets, "密钥")?;
    let mut ids = HashSet::new();
    for app in &config.applications {
        validate_dns_label(&app.id, "应用名称")?;
        if !ids.insert(app.id.as_str()) {
            return Err(AppError::bad(format!("应用 {} 重复", app.id)));
        }
        if app.image.trim().is_empty()
            || app.image.len() > 255
            || app.image.contains('/')
            || app.image.contains("://")
            || app.image.chars().any(char::is_whitespace)
        {
            return Err(AppError::bad(format!(
                "应用 {} 的镜像只能填写名称和版本，例如 cangling-map:0.1.11",
                app.id
            )));
        }
        if app.workload == WorkloadKind::Deployment {
            if app.group == ApplicationGroup::Single && app.replicas != 1 {
                return Err(AppError::bad(format!(
                    "单实例应用 {} 的副本数必须固定为 1",
                    app.id
                )));
            }
            if app.replicas == 0 || app.replicas > 100 {
                return Err(AppError::bad(format!(
                    "应用 {} 的副本数必须为 1–100",
                    app.id
                )));
            }
            let nodes = deployment_nodes(app, &config.primary_node);
            if nodes.is_empty() {
                return Err(AppError::bad(format!(
                    "应用 {} 至少需要选择一个部署节点",
                    app.id
                )));
            }
            if app.group != ApplicationGroup::Multi && nodes.len() != 1 {
                return Err(AppError::bad(format!(
                    "单实例应用 {} 只能选择一个部署节点",
                    app.id
                )));
            }
            let mut unique_nodes = HashSet::new();
            for node in nodes {
                validate_dns_label(node, "部署节点")?;
                if !unique_nodes.insert(node) {
                    return Err(AppError::bad(format!(
                        "应用 {} 的部署节点 {} 重复",
                        app.id, node
                    )));
                }
            }
        }
        if app.ports.is_empty() || app.ports.iter().any(|port| *port == 0) {
            return Err(AppError::bad(format!(
                "应用 {} 至少需要一个有效端口",
                app.id
            )));
        }
    }
    Ok(())
}

fn deployment_nodes<'a>(app: &'a ApplicationConfig, fallback: &'a str) -> Vec<&'a str> {
    let source = if app.node.trim().is_empty() {
        fallback
    } else {
        app.node.as_str()
    };
    source
        .split(|ch: char| ch == ',' || ch == '，' || ch.is_whitespace())
        .map(str::trim)
        .filter(|node| !node.is_empty())
        .collect()
}

fn validate_variable_map(values: &BTreeMap<String, String>, label: &str) -> Result<(), AppError> {
    if values.len() > 256 {
        return Err(AppError::bad(format!("{label}数量不能超过 256 项")));
    }
    for (name, value) in values {
        let mut chars = name.chars();
        let valid_first = chars
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic());
        if !valid_first || !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
            return Err(AppError::bad(format!("{label}名称 {name} 无效")));
        }
        if value.len() > 16_384 {
            return Err(AppError::bad(format!("{label} {name} 的值过长")));
        }
    }
    Ok(())
}

fn validate_dns_label(value: &str, label: &str) -> Result<(), AppError> {
    let bytes = value.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 63
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    {
        return Err(AppError::bad(format!("{label}不是有效的 Kubernetes 名称")));
    }
    Ok(())
}

fn key(name: &str) -> Value {
    Value::String(name.into())
}

fn mapping_mut(value: &mut Value) -> Result<&mut Mapping, AppError> {
    value
        .as_mapping_mut()
        .ok_or_else(|| AppError::internal("内置部署模板结构无效"))
}

fn child_mut<'a>(value: &'a mut Value, name: &str) -> Result<&'a mut Value, AppError> {
    mapping_mut(value)?
        .get_mut(key(name))
        .ok_or_else(|| AppError::internal(format!("内置部署模板缺少字段 {name}")))
}

fn value_str<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for name in path {
        current = current.as_mapping()?.get(key(name))?;
    }
    current.as_str()
}

fn metadata_name(value: &Value) -> Option<&str> {
    value_str(value, &["metadata", "name"])
}

fn set_mapping_string(map: &mut Mapping, name: &str, value: impl Into<String>) {
    map.insert(key(name), Value::String(value.into()));
}

fn set_mapping_number(map: &mut Mapping, name: &str, value: u64) {
    map.insert(key(name), Value::Number(value.into()));
}

fn patch_namespace(doc: &mut Value, namespace: &str) {
    let is_namespace = value_str(doc, &["kind"]) == Some("Namespace");
    if let Some(metadata) = doc
        .as_mapping_mut()
        .and_then(|map| map.get_mut(key("metadata")))
        .and_then(Value::as_mapping_mut)
    {
        if metadata.get(key("namespace")).is_some() {
            set_mapping_string(metadata, "namespace", namespace);
        } else if metadata.get(key("name")).and_then(Value::as_str) == Some(DEFAULT_NAMESPACE)
            && is_namespace
        {
            set_mapping_string(metadata, "name", namespace);
        }
    }
    replace_named_string(doc, "namespace", DEFAULT_NAMESPACE, namespace);
}

fn patch_environment(
    doc: &mut Value,
    config: &CanglingConfig,
    apps: &BTreeMap<&str, &ApplicationConfig>,
) -> Result<(), AppError> {
    let data = child_mut(doc, "data")?
        .as_mapping_mut()
        .ok_or_else(|| AppError::internal("内置部署模板中的 cangling-env.data 不是对象"))?;
    let mut environment = config.environment.clone();
    let defaults = default_environment();
    let port = |id: &str, index: usize| -> Result<u16, AppError> {
        apps.get(id)
            .and_then(|app| app.ports.get(index))
            .copied()
            .ok_or_else(|| AppError::bad(format!("应用 {id} 缺少端口配置")))
    };
    let broker = port("cangling-broker", 0)?;
    let broker_web = port("cangling-broker", 1)?;
    let tile = port("cangling-tile", 0)?;
    let map = port("cangling-map", 0)?;
    let cis_map = port("cis-map", 0)?;
    let web = port("cangling-nginx", 0)?;
    for (name, value) in [
        ("HOST", config.host_ip.clone()),
        ("HOST_KAFKA", config.host_ip.clone()),
        ("WEB_PORT", web.to_string()),
        ("DATA_PATH", config.shared_root.clone()),
        ("CL_BASE_PATH", format!("{}/cldata", config.shared_root)),
        ("IB_CACHE_PATH", format!("{}/ibcache", config.local_root)),
        ("CL_BROKER_PORT", broker.to_string()),
        ("CL_BROKER_WEBPORT", broker_web.to_string()),
        ("TILE_CACHE_URL", format!("http://cangling-tile:{tile}")),
        ("HOST_GEOSERVER_URL", format!("http://cangling-map:{map}")),
        ("AI_MAP", format!("http://cis-map:{cis_map}/")),
        ("AI_WEB", format!("http://{}:{web}", config.host_ip)),
        ("CL_SERVER_URL", format!("http://{}:{web}/", config.host_ip)),
        (
            "GEOSERVER_FRONT",
            format!("http://{}:{web}/geoserver", config.host_ip),
        ),
    ] {
        if environment.get(name) == defaults.get(name) {
            environment.insert(name.into(), value);
        }
    }
    data.clear();
    for (name, value) in &environment {
        set_mapping_string(data, name, value);
    }
    Ok(())
}

fn patch_secret(doc: &mut Value, config: &CanglingConfig) -> Result<(), AppError> {
    let data = child_mut(doc, "stringData")?
        .as_mapping_mut()
        .ok_or_else(|| AppError::internal("内置部署模板中的 Secret.stringData 不是对象"))?;
    data.clear();
    for (name, value) in &config.secrets {
        set_mapping_string(data, name, value);
    }
    Ok(())
}

fn patch_nginx_config(
    doc: &mut Value,
    apps: &BTreeMap<&str, &ApplicationConfig>,
) -> Result<(), AppError> {
    let data = child_mut(doc, "data")?
        .as_mapping_mut()
        .ok_or_else(|| AppError::internal("内置部署模板中的 cangling-nginx.data 不是对象"))?;
    let nginx = data
        .get_mut(key("nginx.conf"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| AppError::internal("内置部署模板缺少 nginx.conf"))?
        .to_string();
    let web = apps
        .get("cangling-nginx")
        .and_then(|app| app.ports.first())
        .copied()
        .ok_or_else(|| AppError::bad("应用 cangling-nginx 缺少入口端口配置"))?;
    let mut replacements = vec![("listen 7600;".to_string(), format!("listen {web};"))];
    for (id, default_port, index) in [
        ("cangling-broker", 7501, 1usize),
        ("cis-server", 8078, 0usize),
        ("cis-map", 8079, 0usize),
        ("cangling-map", 3000, 0usize),
        ("np4-base", 8080, 0usize),
        ("cangling-tile", 7601, 0usize),
    ] {
        let configured = apps
            .get(id)
            .and_then(|app| app.ports.get(index))
            .copied()
            .ok_or_else(|| AppError::bad(format!("应用 {id} 缺少端口配置")))?;
        replacements.push((format!("{id}:{default_port}"), format!("{id}:{configured}")));
    }
    let mut patched = nginx;
    for (from, to) in replacements {
        patched = patched.replace(&from, &to);
    }
    data.insert(key("nginx.conf"), Value::String(patched));
    Ok(())
}

fn attach_shared_configuration(doc: &mut Value) -> Result<(), AppError> {
    let kind = value_str(doc, &["kind"]).unwrap_or_default();
    if kind != "Deployment" && kind != "DaemonSet" && kind != "StatefulSet" {
        return Ok(());
    }
    let containers = child_mut(
        child_mut(child_mut(child_mut(doc, "spec")?, "template")?, "spec")?,
        "containers",
    )?
    .as_sequence_mut()
    .ok_or_else(|| AppError::internal("内置工作负载的 containers 不是数组"))?;
    for container in containers {
        let container = mapping_mut(container)?;
        let env_from = container
            .entry(key("envFrom"))
            .or_insert_with(|| Value::Sequence(Vec::new()))
            .as_sequence_mut()
            .ok_or_else(|| AppError::internal("内置工作负载的 envFrom 不是数组"))?;
        ensure_env_reference(env_from, "configMapRef", "cangling-env");
        ensure_env_reference(env_from, "secretRef", "cangling-secret");
    }
    Ok(())
}

fn ensure_env_reference(items: &mut Vec<Value>, reference: &str, name: &str) {
    let exists = items.iter().any(|item| {
        item.as_mapping()
            .and_then(|item| item.get(key(reference)))
            .and_then(Value::as_mapping)
            .and_then(|reference| reference.get(key("name")))
            .and_then(Value::as_str)
            == Some(name)
    });
    if exists {
        return;
    }
    let mut reference_value = Mapping::new();
    set_mapping_string(&mut reference_value, "name", name);
    let mut item = Mapping::new();
    item.insert(key(reference), Value::Mapping(reference_value));
    items.push(Value::Mapping(item));
}

fn patch_application_document(
    doc: &mut Value,
    app: &ApplicationConfig,
    config: &CanglingConfig,
) -> Result<(), AppError> {
    let kind = value_str(doc, &["kind"]).unwrap_or_default().to_string();
    if kind == "Service" {
        patch_service_ports(doc, app)?;
        return Ok(());
    }
    if kind == "ConfigMap" || (kind != "Deployment" && kind != "DaemonSet") {
        return Ok(());
    }
    let spec = child_mut(doc, "spec")?;
    if kind == "Deployment" {
        mapping_mut(spec)?.insert(key("replicas"), Value::Number(app.replicas.into()));
    }
    let template = child_mut(spec, "template")?;
    let pod_spec = child_mut(template, "spec")?;
    let pod_map = mapping_mut(pod_spec)?;
    if app.workload == WorkloadKind::Deployment {
        let nodes = deployment_nodes(app, &config.primary_node);
        pod_map.remove(key("nodeSelector"));
        pod_map.remove(key("affinity"));
        let terms = ["cangling.cn/node-id", "kubernetes.io/hostname"]
            .into_iter()
            .map(|label| {
                let mut expression = Mapping::new();
                set_mapping_string(&mut expression, "key", label);
                set_mapping_string(&mut expression, "operator", "In");
                expression.insert(
                    key("values"),
                    Value::Sequence(
                        nodes
                            .iter()
                            .map(|node| Value::String((*node).to_string()))
                            .collect(),
                    ),
                );
                let mut term = Mapping::new();
                term.insert(
                    key("matchExpressions"),
                    Value::Sequence(vec![Value::Mapping(expression)]),
                );
                Value::Mapping(term)
            })
            .collect();
        let mut required = Mapping::new();
        required.insert(key("nodeSelectorTerms"), Value::Sequence(terms));
        let mut node_affinity = Mapping::new();
        node_affinity.insert(
            key("requiredDuringSchedulingIgnoredDuringExecution"),
            Value::Mapping(required),
        );
        let mut affinity = Mapping::new();
        affinity.insert(key("nodeAffinity"), Value::Mapping(node_affinity));
        pod_map.insert(key("affinity"), Value::Mapping(affinity));
    }
    {
        let containers = pod_map
            .get_mut(key("containers"))
            .and_then(Value::as_sequence_mut)
            .ok_or_else(|| AppError::internal(format!("应用 {} 缺少 containers", app.id)))?;
        let container = containers
            .first_mut()
            .ok_or_else(|| AppError::internal(format!("应用 {} 没有容器", app.id)))?;
        let container_map = mapping_mut(container)?;
        set_mapping_string(container_map, "image", &app.image);
        if let Some(ports) = container_map
            .get_mut(key("ports"))
            .and_then(Value::as_sequence_mut)
        {
            patch_port_sequence(ports, &app.ports, "containerPort");
            if app.id == "cangling-nginx" {
                patch_port_sequence(ports, &app.ports, "hostPort");
            }
        }
        if app.id == "cangling-map" {
            patch_named_env(
                container_map,
                "MAP_SERVER_ADDRESS",
                &format!("0.0.0.0:{}", app.ports[0]),
            );
        }
    }
    stage_jar_runtime(pod_map, app, config)?;
    Ok(())
}

fn stage_jar_runtime(
    pod_map: &mut Mapping,
    app: &ApplicationConfig,
    config: &CanglingConfig,
) -> Result<(), AppError> {
    let Some(volumes) = pod_map
        .get_mut(key("volumes"))
        .and_then(Value::as_sequence_mut)
    else {
        return Ok(());
    };
    let Some(index) = volumes.iter().position(|volume| {
        volume
            .as_mapping()
            .and_then(|volume| volume.get(key("name")))
            .and_then(Value::as_str)
            == Some("jar")
    }) else {
        return Ok(());
    };
    let source_path = volumes[index]
        .as_mapping()
        .and_then(|volume| volume.get(key("hostPath")))
        .and_then(Value::as_mapping)
        .and_then(|host_path| host_path.get(key("path")))
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::internal(format!("应用 {} 的 JAR 发布源无效", app.id)))?;
    let filename = source_path
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppError::internal(format!("应用 {} 的 JAR 文件名无效", app.id)))?
        .to_string();

    let mut source_host_path = Mapping::new();
    set_mapping_string(&mut source_host_path, "path", &config.jar_root);
    set_mapping_string(&mut source_host_path, "type", "Directory");
    let mut source_volume = Mapping::new();
    set_mapping_string(&mut source_volume, "name", "jar-source");
    source_volume.insert(key("hostPath"), Value::Mapping(source_host_path));
    volumes[index] = Value::Mapping(source_volume);

    let mut runtime_volume = Mapping::new();
    set_mapping_string(&mut runtime_volume, "name", "jar");
    runtime_volume.insert(key("emptyDir"), Value::Mapping(Mapping::new()));
    volumes.push(Value::Mapping(runtime_volume));

    let containers = pod_map
        .get_mut(key("containers"))
        .and_then(Value::as_sequence_mut)
        .ok_or_else(|| AppError::internal(format!("应用 {} 缺少 containers", app.id)))?;
    let container = containers
        .first_mut()
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| AppError::internal(format!("应用 {} 没有主容器", app.id)))?;
    let mounts = container
        .get_mut(key("volumeMounts"))
        .and_then(Value::as_sequence_mut)
        .ok_or_else(|| AppError::internal(format!("应用 {} 缺少 volumeMounts", app.id)))?;
    let jar_mount = mounts
        .iter_mut()
        .filter_map(Value::as_mapping_mut)
        .find(|mount| mount.get(key("name")).and_then(Value::as_str) == Some("jar"))
        .ok_or_else(|| AppError::internal(format!("应用 {} 缺少 JAR 挂载", app.id)))?;
    set_mapping_string(jar_mount, "subPath", "app.jar");

    let mut source_mount = Mapping::new();
    set_mapping_string(&mut source_mount, "name", "jar-source");
    set_mapping_string(&mut source_mount, "mountPath", "/source");
    source_mount.insert(key("readOnly"), Value::Bool(true));
    let mut runtime_mount = Mapping::new();
    set_mapping_string(&mut runtime_mount, "name", "jar");
    set_mapping_string(&mut runtime_mount, "mountPath", "/runtime");
    let mut init = Mapping::new();
    set_mapping_string(&mut init, "name", "stage-jar");
    set_mapping_string(&mut init, "image", &app.image);
    set_mapping_string(&mut init, "imagePullPolicy", "IfNotPresent");
    init.insert(
        key("command"),
        Value::Sequence(
            [
                "cp".to_string(),
                format!("/source/{filename}"),
                "/runtime/app.jar".to_string(),
            ]
            .into_iter()
            .map(Value::String)
            .collect(),
        ),
    );
    init.insert(
        key("volumeMounts"),
        Value::Sequence(vec![
            Value::Mapping(source_mount),
            Value::Mapping(runtime_mount),
        ]),
    );
    pod_map
        .entry(key("initContainers"))
        .or_insert_with(|| Value::Sequence(Vec::new()))
        .as_sequence_mut()
        .ok_or_else(|| AppError::internal(format!("应用 {} 的 initContainers 无效", app.id)))?
        .push(Value::Mapping(init));
    Ok(())
}

fn patch_storage_volumes(doc: &mut Value, config: &CanglingConfig) -> Result<(), AppError> {
    let kind = value_str(doc, &["kind"]).unwrap_or_default();
    if kind != "Deployment" && kind != "DaemonSet" && kind != "StatefulSet" {
        return Ok(());
    }
    let pod_spec = child_mut(child_mut(child_mut(doc, "spec")?, "template")?, "spec")?;
    let Some(volumes) = pod_spec
        .as_mapping_mut()
        .and_then(|pod| pod.get_mut(key("volumes")))
        .and_then(Value::as_sequence_mut)
    else {
        return Ok(());
    };
    for volume in volumes {
        let Some(volume) = volume.as_mapping_mut() else {
            continue;
        };
        let Some(path) = volume
            .get(key("hostPath"))
            .and_then(Value::as_mapping)
            .and_then(|host_path| host_path.get(key("path")))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        let Some((mapped_root, storage)) = storage_for_path(config, &path) else {
            continue;
        };
        let source_root = storage_source(storage, mapped_root);
        let suffix = path.strip_prefix(mapped_root).unwrap_or_default();
        match storage.storage_type {
            StorageType::Local => {
                let source = format!("{}{suffix}", source_root.trim_end_matches('/'));
                let host_path = volume
                    .get_mut(key("hostPath"))
                    .and_then(Value::as_mapping_mut)
                    .ok_or_else(|| AppError::internal("hostPath 配置无效"))?;
                set_mapping_string(host_path, "path", &source);
            }
            StorageType::Nfs => {
                let (server, export_root) = parse_nfs_source(&source_root)?;
                let export_path = format!("{}{suffix}", export_root.trim_end_matches('/'));
                volume.remove(key("hostPath"));
                let mut nfs = Mapping::new();
                set_mapping_string(&mut nfs, "server", server);
                set_mapping_string(&mut nfs, "path", export_path);
                volume.insert(key("nfs"), Value::Mapping(nfs));
            }
        }
    }
    Ok(())
}

fn storage_source(storage: &StorageConfig, mapped_path: &str) -> String {
    let source = storage.source.trim();
    if !source.is_empty() {
        return source.to_string();
    }
    if storage.storage_type == StorageType::Nfs {
        format!("{}:{mapped_path}", storage.server.trim())
    } else {
        mapped_path.to_string()
    }
}

fn parse_nfs_source(source: &str) -> Result<(&str, &str), AppError> {
    let (server, path) = source
        .trim()
        .split_once(':')
        .ok_or_else(|| AppError::bad("NFS Value 必须使用 server:/export/path 格式"))?;
    if server.is_empty()
        || server.contains("//")
        || server.contains('/')
        || server.chars().any(char::is_whitespace)
        || !path.starts_with('/')
        || path.contains("..")
        || path.chars().any(char::is_whitespace)
    {
        return Err(AppError::bad("NFS Value 必须使用 server:/export/path 格式"));
    }
    Ok((server, path))
}

fn storage_for_path<'a>(
    config: &'a CanglingConfig,
    path: &str,
) -> Option<(&'a str, &'a StorageConfig)> {
    [
        (config.shared_root.as_str(), &config.shared_storage),
        (config.local_root.as_str(), &config.local_storage),
        (config.core_root.as_str(), &config.core_storage),
        (config.jar_root.as_str(), &config.jar_storage),
    ]
    .into_iter()
    .find_map(|(root, storage)| {
        (path == root
            || path
                .strip_prefix(root)
                .is_some_and(|suffix| suffix.starts_with('/')))
        .then_some((root, storage))
    })
}

fn patch_service_ports(doc: &mut Value, app: &ApplicationConfig) -> Result<(), AppError> {
    let spec = child_mut(doc, "spec")?;
    let ports = child_mut(spec, "ports")?
        .as_sequence_mut()
        .ok_or_else(|| AppError::internal(format!("应用 {} 的 Service.ports 无效", app.id)))?;
    patch_port_sequence(ports, &app.ports, "port");
    for (index, port) in ports.iter_mut().enumerate() {
        let Some(configured) = app.ports.get(index).copied() else {
            break;
        };
        if let Some(map) = port.as_mapping_mut() {
            if map.get(key("targetPort")).is_some_and(Value::is_number) {
                set_mapping_number(map, "targetPort", configured.into());
            }
        }
    }
    Ok(())
}

fn patch_port_sequence(ports: &mut [Value], configured: &[u16], field: &str) {
    for (index, port) in ports.iter_mut().enumerate() {
        let Some(value) = configured.get(index).copied() else {
            break;
        };
        if let Some(map) = port.as_mapping_mut() {
            set_mapping_number(map, field, value.into());
        }
    }
}

fn patch_named_env(container: &mut Mapping, name: &str, value: &str) {
    let Some(env) = container
        .get_mut(key("env"))
        .and_then(Value::as_sequence_mut)
    else {
        return;
    };
    for item in env {
        let Some(map) = item.as_mapping_mut() else {
            continue;
        };
        if map.get(key("name")).and_then(Value::as_str) == Some(name) {
            set_mapping_string(map, "value", value);
            return;
        }
    }
}

fn replace_strings(value: &mut Value, replacements: &[(&str, &str)]) {
    match value {
        Value::String(text) => {
            for (from, to) in replacements {
                *text = text.replace(from, to);
            }
        }
        Value::Sequence(items) => {
            for item in items {
                replace_strings(item, replacements);
            }
        }
        Value::Mapping(map) => {
            for item in map.values_mut() {
                replace_strings(item, replacements);
            }
        }
        _ => {}
    }
}

fn replace_named_string(value: &mut Value, field: &str, from: &str, to: &str) {
    match value {
        Value::Sequence(items) => {
            for item in items {
                replace_named_string(item, field, from, to);
            }
        }
        Value::Mapping(map) => {
            if let Some(Value::String(text)) = map.get_mut(key(field)) {
                if text == from {
                    *text = to.into();
                }
            }
            for item in map.values_mut() {
                replace_named_string(item, field, from, to);
            }
        }
        _ => {}
    }
}

fn rebase_template_images(value: &mut Value, prefix: &str) {
    match value {
        Value::Sequence(items) => {
            for item in items {
                rebase_template_images(item, prefix);
            }
        }
        Value::Mapping(map) => {
            if let Some(Value::String(image)) = map.get_mut(key("image")) {
                *image = image_with_prefix(prefix, image);
            }
            for item in map.values_mut() {
                rebase_template_images(item, prefix);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_describe_current_topology() {
        let config = default_config();
        assert_eq!(config.image_prefix, DEFAULT_IMAGE_PREFIX);
        assert_eq!(config.shared_root, "/opt/cl/share/cangling");
        assert_eq!(config.local_root, "/opt/cl/local/cangling");
        assert_eq!(config.core_root, "/opt/cl/core/cangling");
        assert_eq!(config.jar_root, "/opt/cl/jars/cangling");
        assert_eq!(config.shared_storage.storage_type, StorageType::Nfs);
        assert_eq!(config.local_storage.storage_type, StorageType::Local);
        assert_eq!(config.core_storage.storage_type, StorageType::Local);
        assert_eq!(config.jar_storage.storage_type, StorageType::Nfs);
        assert_eq!(config.applications.len(), 9);
        assert_eq!(
            config.environment.get("TILE_CACHE_URL").map(String::as_str),
            Some("http://cangling-tile:7601")
        );
        assert_eq!(
            config.secrets.get("POSTGIS_PWD").map(String::as_str),
            Some("cangling-postgis")
        );
        assert_eq!(
            config
                .secrets
                .get("CANGLING_TILE_ADMIN_PASSWORD")
                .map(String::as_str),
            Some("Ji2*p8Uh")
        );
        assert_eq!(
            config
                .secrets
                .get("CANGLING_TILE_AUTH_TOKEN")
                .map(String::as_str),
            Some("632ad553d15944e900de07df883b3eadefbf4a21")
        );
        assert!(!config.secrets.contains_key("POSTGRES_PASS"));
        assert!(!config.secrets.contains_key("POSTGRES_PWD"));
        assert!(!config.secrets.contains_key("TILE_CACHE_TOKEN"));
        assert!(!config.secrets.contains_key("TILE_CACHE_AUTH_TOKEN"));
        assert!(config
            .applications
            .iter()
            .filter(|app| app.group == ApplicationGroup::Single)
            .all(|app| app.replicas == 1));
        assert!(config
            .applications
            .iter()
            .all(|app| !app.image.contains('/')));
        assert_eq!(
            config
                .applications
                .iter()
                .filter(|app| app.group == ApplicationGroup::Multi)
                .count(),
            3
        );
        assert_eq!(
            config
                .applications
                .iter()
                .filter(|app| app.group == ApplicationGroup::Entry)
                .count(),
            1
        );
        let map = config
            .applications
            .iter()
            .find(|app| app.id == "cangling-map")
            .unwrap();
        assert_eq!(map.group, ApplicationGroup::Multi);
        assert_eq!(map.replicas, 1);
        assert_eq!(map.image, "cangling-map:0.1.11");
        let tile = config
            .applications
            .iter()
            .find(|app| app.id == "cangling-tile")
            .unwrap();
        assert_eq!(tile.image, "cangling-tile:0.1.20");
        let entry = config
            .applications
            .iter()
            .find(|app| app.group == ApplicationGroup::Entry)
            .unwrap();
        assert_eq!(entry.id, "cangling-nginx");
        assert_eq!(entry.storage, "/opt/cl/local/cangling/nginx-log");
        for id in ["cis-map", "np4-base"] {
            let application = config.applications.iter().find(|app| app.id == id).unwrap();
            assert_eq!(application.group, ApplicationGroup::Multi);
            assert_eq!(application.workload, WorkloadKind::Deployment);
            assert_eq!(application.replicas, 1);
        }
        let documents = parse_documents().unwrap();
        for id in ["cis-map", "np4-base"] {
            let deployment = documents
                .iter()
                .find(|document| {
                    metadata_name(document) == Some(id)
                        && value_str(document, &["kind"]) == Some("Deployment")
                })
                .unwrap();
            assert_eq!(
                deployment
                    .as_mapping()
                    .and_then(|document| document.get(key("spec")))
                    .and_then(Value::as_mapping)
                    .and_then(|spec| spec.get(key("replicas")))
                    .and_then(Value::as_u64),
                Some(1)
            );
        }
    }

    #[test]
    fn manifest_applies_configuration() {
        let mut config = default_config();
        config.namespace = "cangling-test".into();
        config.host_ip = "10.0.0.8".into();
        config.image_prefix = "registry.example.com/cangling".into();
        config.shared_root = "/data/shared".into();
        config
            .environment
            .insert("CUSTOM_FEATURE".into(), "enabled".into());
        config
            .secrets
            .insert("CUSTOM_SECRET".into(), "secret-value".into());
        let map = config
            .applications
            .iter_mut()
            .find(|app| app.id == "cangling-map")
            .unwrap();
        map.image = "cangling-map:test".into();
        map.ports = vec![3100];
        let nginx = config
            .applications
            .iter_mut()
            .find(|app| app.id == "cangling-nginx")
            .unwrap();
        nginx.ports = vec![7700];
        let (yaml, warnings) = render_manifest(&config).unwrap();
        assert!(yaml.contains("namespace: cangling-test"));
        assert!(yaml.contains("POSTGIS_USER: cangling"));
        assert!(yaml.contains("value: cangling"));
        assert!(yaml.contains("registry.example.com/cangling/cangling-map:test"));
        assert!(yaml.contains("http://cangling-map:3100"));
        assert!(yaml.contains("WEB_PORT: '7700'"));
        assert!(yaml.contains("listen 7700;"));
        assert!(yaml.contains("hostPort: 7700"));
        assert!(yaml.contains("registry.example.com/cangling/postgis:17-3.5"));
        assert!(!yaml.contains("kind: DaemonSet"));
        assert!(!yaml.contains("geoserver:v2.28.49"));
        assert!(!yaml.contains("geoserver-rollback"));
        assert!(yaml.contains("upstream cangling_map { server cangling-map:3100;"));
        assert!(yaml.contains("CUSTOM_FEATURE: enabled"));
        assert!(yaml.contains("CUSTOM_SECRET: secret-value"));
        assert!(!yaml.contains("docker.1ms.run/kartoza/postgis"));
        assert!(yaml.contains("/data/shared"));
        assert!(yaml.contains("CIS_MONGO_MIGRATION_ENABLED: 'false'"));
        assert!(!yaml.contains("cis-mongo"));
        assert!(!yaml.to_ascii_lowercase().contains("mongodb"));
        assert!(!warnings.is_empty());
    }

    #[test]
    fn manifest_uses_nfs_sources_and_stages_jars_in_empty_dir() {
        let (yaml, _) = render_manifest(&default_config()).unwrap();
        let documents: Vec<Value> = serde_yaml::Deserializer::from_str(&yaml)
            .map(|document| Value::deserialize(document).unwrap())
            .collect();
        let deployment = documents
            .iter()
            .find(|document| {
                metadata_name(document) == Some("cis-server")
                    && value_str(document, &["kind"]) == Some("Deployment")
            })
            .unwrap();
        let pod = deployment
            .as_mapping()
            .and_then(|document| document.get(key("spec")))
            .and_then(Value::as_mapping)
            .and_then(|spec| spec.get(key("template")))
            .and_then(Value::as_mapping)
            .and_then(|template| template.get(key("spec")))
            .and_then(Value::as_mapping)
            .unwrap();
        let volumes = pod
            .get(key("volumes"))
            .and_then(Value::as_sequence)
            .unwrap();
        let jar_source = volumes
            .iter()
            .filter_map(Value::as_mapping)
            .find(|volume| volume.get(key("name")).and_then(Value::as_str) == Some("jar-source"))
            .unwrap();
        assert_eq!(
            jar_source
                .get(key("nfs"))
                .and_then(Value::as_mapping)
                .and_then(|nfs| nfs.get(key("path")))
                .and_then(Value::as_str),
            Some("/opt/cl/jars/cangling")
        );
        let jar_runtime = volumes
            .iter()
            .filter_map(Value::as_mapping)
            .find(|volume| volume.get(key("name")).and_then(Value::as_str) == Some("jar"))
            .unwrap();
        assert!(jar_runtime.get(key("emptyDir")).is_some());
        let init = pod
            .get(key("initContainers"))
            .and_then(Value::as_sequence)
            .and_then(|containers| containers.first())
            .and_then(Value::as_mapping)
            .unwrap();
        assert_eq!(
            init.get(key("command"))
                .and_then(Value::as_sequence)
                .and_then(|command| command.get(1))
                .and_then(Value::as_str),
            Some("/source/cis-server-1.0.0.jar")
        );
        let container = pod
            .get(key("containers"))
            .and_then(Value::as_sequence)
            .and_then(|containers| containers.first())
            .and_then(Value::as_mapping)
            .unwrap();
        let jar_mount = container
            .get(key("volumeMounts"))
            .and_then(Value::as_sequence)
            .unwrap()
            .iter()
            .filter_map(Value::as_mapping)
            .find(|mount| mount.get(key("name")).and_then(Value::as_str) == Some("jar"))
            .unwrap();
        assert_eq!(
            jar_mount.get(key("subPath")).and_then(Value::as_str),
            Some("app.jar")
        );
    }

    #[test]
    fn local_source_uses_actual_host_path_and_generates_safe_mapping_job() {
        let mut config = default_config();
        config.local_storage.source = "/mnt/nvme/cangling-local".into();
        let (yaml, warnings) = render_manifest(&config).unwrap();
        let documents: Vec<Value> = serde_yaml::Deserializer::from_str(&yaml)
            .map(|document| Value::deserialize(document).unwrap())
            .collect();
        let mapping_job = documents
            .iter()
            .find(|document| value_str(document, &["kind"]) == Some("Job"))
            .unwrap();
        let rendered_job = serde_yaml::to_string(mapping_job).unwrap();
        assert!(rendered_job.contains("/mnt/nvme/cangling-local"));
        assert!(rendered_job.contains("/opt/cl/local"));
        assert!(!rendered_job.contains("privileged"));
        assert!(!rendered_job.contains("path: /\n"));
        assert!(warnings.iter().any(|warning| warning.contains("软链接")));

        let map = documents
            .iter()
            .find(|document| {
                metadata_name(document) == Some("cangling-map")
                    && value_str(document, &["kind"]) == Some("Deployment")
            })
            .unwrap();
        let rendered_map = serde_yaml::to_string(map).unwrap();
        assert!(rendered_map.contains("path: /mnt/nvme/cangling-local/cangling-map"));
        assert!(rendered_map.contains("mountPath: /data"));
    }

    #[test]
    fn every_workload_mounts_shared_environment_and_secrets() {
        let (yaml, _) = render_manifest(&default_config()).unwrap();
        let documents: Vec<Value> = serde_yaml::Deserializer::from_str(&yaml)
            .map(|document| Value::deserialize(document).unwrap())
            .collect();
        let mut workload_count = 0;
        for document in &documents {
            let kind = value_str(document, &["kind"]).unwrap_or_default();
            if kind != "Deployment" && kind != "DaemonSet" && kind != "StatefulSet" {
                continue;
            }
            workload_count += 1;
            let containers = document
                .as_mapping()
                .and_then(|document| document.get(key("spec")))
                .and_then(Value::as_mapping)
                .and_then(|spec| spec.get(key("template")))
                .and_then(Value::as_mapping)
                .and_then(|template| template.get(key("spec")))
                .and_then(Value::as_mapping)
                .and_then(|spec| spec.get(key("containers")))
                .and_then(Value::as_sequence)
                .unwrap();
            for container in containers {
                let env_from = container
                    .as_mapping()
                    .and_then(|container| container.get(key("envFrom")))
                    .and_then(Value::as_sequence)
                    .unwrap();
                for (reference, name) in [
                    ("configMapRef", "cangling-env"),
                    ("secretRef", "cangling-secret"),
                ] {
                    assert!(env_from.iter().any(|item| {
                        item.as_mapping()
                            .and_then(|item| item.get(key(reference)))
                            .and_then(Value::as_mapping)
                            .and_then(|reference| reference.get(key("name")))
                            .and_then(Value::as_str)
                            == Some(name)
                    }));
                }
            }
        }
        assert!(workload_count >= 9);
    }

    #[test]
    fn invalid_local_path_is_rejected() {
        let mut config = default_config();
        config.local_root = "../tmp".into();
        assert!(render_manifest(&config).is_err());
    }

    #[test]
    fn invalid_image_prefix_is_rejected() {
        let mut config = default_config();
        config.image_prefix = "https://hub.cangling.cn/cangling".into();
        assert!(render_manifest(&config).is_err());
    }

    #[test]
    fn image_name_must_not_repeat_the_prefix() {
        let mut config = default_config();
        config.applications[0].image = "hub.cangling.cn/cangling/postgis:17-3.5".into();
        assert!(render_manifest(&config).is_err());
    }

    #[test]
    fn single_instance_replica_count_is_fixed() {
        let mut config = default_config();
        let application = config
            .applications
            .iter_mut()
            .find(|app| app.group == ApplicationGroup::Single)
            .unwrap();
        application.replicas = 2;
        assert!(render_manifest(&config).is_err());
    }

    #[test]
    fn multi_instance_nodes_generate_required_node_affinity() {
        let mut config = default_config();
        let application = config
            .applications
            .iter_mut()
            .find(|app| app.id == "cangling-map")
            .unwrap();
        application.node = "n01, n02".into();
        let (yaml, _) = render_manifest(&config).unwrap();
        let documents: Vec<Value> = serde_yaml::Deserializer::from_str(&yaml)
            .map(|document| Value::deserialize(document).unwrap())
            .collect();
        let deployment = documents
            .iter()
            .find(|document| {
                metadata_name(document) == Some("cangling-map")
                    && value_str(document, &["kind"]) == Some("Deployment")
            })
            .unwrap();
        let pod_spec = deployment
            .as_mapping()
            .and_then(|document| document.get(key("spec")))
            .and_then(Value::as_mapping)
            .and_then(|spec| spec.get(key("template")))
            .and_then(Value::as_mapping)
            .and_then(|template| template.get(key("spec")))
            .and_then(Value::as_mapping)
            .unwrap();
        assert!(pod_spec.get(key("nodeSelector")).is_none());
        let terms = pod_spec
            .get(key("affinity"))
            .and_then(Value::as_mapping)
            .and_then(|affinity| affinity.get(key("nodeAffinity")))
            .and_then(Value::as_mapping)
            .and_then(|affinity| {
                affinity.get(key("requiredDuringSchedulingIgnoredDuringExecution"))
            })
            .and_then(Value::as_mapping)
            .and_then(|required| required.get(key("nodeSelectorTerms")))
            .and_then(Value::as_sequence)
            .unwrap();
        assert_eq!(terms.len(), 2);
        let keys: Vec<&str> = terms
            .iter()
            .filter_map(Value::as_mapping)
            .filter_map(|term| term.get(key("matchExpressions")))
            .filter_map(Value::as_sequence)
            .filter_map(|expressions| expressions.first())
            .filter_map(Value::as_mapping)
            .filter_map(|expression| expression.get(key("key")))
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(keys, vec!["cangling.cn/node-id", "kubernetes.io/hostname"]);
        let values = terms
            .first()
            .and_then(Value::as_mapping)
            .and_then(|term| term.get(key("matchExpressions")))
            .and_then(Value::as_sequence)
            .and_then(|expressions| expressions.first())
            .and_then(Value::as_mapping)
            .and_then(|expression| expression.get(key("values")))
            .and_then(Value::as_sequence)
            .unwrap();
        assert_eq!(
            values,
            &vec![Value::String("n01".into()), Value::String("n02".into())]
        );
    }

    #[test]
    fn single_instance_rejects_multiple_nodes() {
        let mut config = default_config();
        let application = config
            .applications
            .iter_mut()
            .find(|app| app.group == ApplicationGroup::Single)
            .unwrap();
        application.node = "n01, n02".into();
        assert!(render_manifest(&config).is_err());
    }
}
