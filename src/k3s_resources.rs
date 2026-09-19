//! k3s resource browser and a deliberately small set of operational actions.
//! Commands are built from validated arguments and never passed through a shell.

use crate::error::AppError;
use axum::extract::{Path, Query};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path as FsPath, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const LOG_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LOG_TAIL: u32 = 5000;
const K3S_BINS: &[&str] = &["k3s", "/usr/local/bin/k3s", "/usr/bin/k3s", "/opt/bin/k3s"];

#[derive(Clone, Copy)]
struct ResourceKind {
    id: &'static str,
    kubectl: &'static str,
    namespaced: bool,
}

const RESOURCE_KINDS: &[ResourceKind] = &[
    ResourceKind {
        id: "namespaces",
        kubectl: "namespaces",
        namespaced: false,
    },
    ResourceKind {
        id: "nodes",
        kubectl: "nodes",
        namespaced: false,
    },
    ResourceKind {
        id: "pods",
        kubectl: "pods",
        namespaced: true,
    },
    ResourceKind {
        id: "deployments",
        kubectl: "deployments.apps",
        namespaced: true,
    },
    ResourceKind {
        id: "statefulsets",
        kubectl: "statefulsets.apps",
        namespaced: true,
    },
    ResourceKind {
        id: "daemonsets",
        kubectl: "daemonsets.apps",
        namespaced: true,
    },
    ResourceKind {
        id: "jobs",
        kubectl: "jobs.batch",
        namespaced: true,
    },
    ResourceKind {
        id: "cronjobs",
        kubectl: "cronjobs.batch",
        namespaced: true,
    },
    ResourceKind {
        id: "services",
        kubectl: "services",
        namespaced: true,
    },
    ResourceKind {
        id: "ingresses",
        kubectl: "ingresses.networking.k8s.io",
        namespaced: true,
    },
    ResourceKind {
        id: "configmaps",
        kubectl: "configmaps",
        namespaced: true,
    },
    ResourceKind {
        id: "persistentvolumeclaims",
        kubectl: "persistentvolumeclaims",
        namespaced: true,
    },
    ResourceKind {
        id: "persistentvolumes",
        kubectl: "persistentvolumes",
        namespaced: false,
    },
    ResourceKind {
        id: "storageclasses",
        kubectl: "storageclasses.storage.k8s.io",
        namespaced: false,
    },
];

#[derive(Debug, Deserialize)]
pub struct ResourceQuery {
    kind: String,
    namespace: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    container: Option<String>,
    tail: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct ScaleBody {
    replicas: u32,
}

#[derive(Debug, Serialize)]
pub struct TextResult {
    text: String,
}

pub fn routes() -> Router<crate::state::AppState> {
    Router::new()
        .route("/api/k3s/status", get(cluster_status))
        .route("/api/k3s/namespaces", get(namespaces))
        .route("/api/k3s/resources", get(resources))
        .route(
            "/api/k3s/resources/{kind}/{namespace}/{name}",
            get(resource_detail),
        )
        .route(
            "/api/k3s/resources/{kind}/{namespace}/{name}/yaml",
            get(resource_yaml),
        )
        .route(
            "/api/k3s/resources/{kind}/{namespace}/{name}/events",
            get(resource_events),
        )
        .route("/api/k3s/pods/{namespace}/{name}/logs", get(pod_logs))
        .route(
            "/api/k3s/workloads/{kind}/{namespace}/{name}/restart",
            post(workload_restart),
        )
        .route(
            "/api/k3s/workloads/{kind}/{namespace}/{name}/scale",
            put(workload_scale),
        )
        .route("/api/k3s/pods/{namespace}/{name}", delete(delete_pod))
}

fn resource_kind(id: &str) -> Result<ResourceKind, AppError> {
    RESOURCE_KINDS
        .iter()
        .copied()
        .find(|kind| kind.id == id)
        .ok_or_else(|| AppError::bad("不支持的 Kubernetes 资源类型"))
}

fn validate_name(value: &str, label: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 253
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
    {
        return Err(AppError::bad(format!("无效的{label}")));
    }
    Ok(())
}

fn namespace_args(kind: ResourceKind, namespace: &str) -> Result<Vec<String>, AppError> {
    if !kind.namespaced {
        return Ok(Vec::new());
    }
    if namespace == "_all" {
        return Ok(vec!["--all-namespaces".into()]);
    }
    validate_name(namespace, "命名空间")?;
    Ok(vec!["--namespace".into(), namespace.into()])
}

fn k3s_binary() -> Option<PathBuf> {
    K3S_BINS.iter().find_map(|candidate| {
        if candidate.contains('/') {
            FsPath::new(candidate)
                .is_file()
                .then(|| PathBuf::from(candidate))
        } else {
            std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|dir| dir.join(candidate))
                    .find(|path| path.is_file())
            })
        }
    })
}

async fn kubectl(args: Vec<String>, limit: Duration) -> Result<String, AppError> {
    let binary = k3s_binary().ok_or_else(|| AppError::internal("当前主机未安装 k3s"))?;
    let mut command = Command::new(binary);
    command
        .arg("kubectl")
        .args(&args)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = timeout(limit, command.output())
        .await
        .map_err(|_| AppError::internal("kubectl 执行超时"))?
        .map_err(|error| AppError::internal(format!("执行 kubectl 失败：{error}")))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(AppError::bad(if message.is_empty() {
            "kubectl 执行失败".into()
        } else {
            message
        }));
    }
    String::from_utf8(output.stdout).map_err(|_| AppError::internal("kubectl 返回了非 UTF-8 数据"))
}

async fn kubectl_json(args: Vec<String>) -> Result<Value, AppError> {
    let text = kubectl(args, COMMAND_TIMEOUT).await?;
    serde_json::from_str(&text)
        .map_err(|error| AppError::internal(format!("解析 kubectl 输出失败：{error}")))
}

async fn cluster_status() -> Result<Json<Value>, AppError> {
    let version = kubectl_json(vec!["version".into(), "-o".into(), "json".into()]).await?;
    let nodes = kubectl_json(vec![
        "get".into(),
        "nodes".into(),
        "-o".into(),
        "json".into(),
    ])
    .await?;
    let pods = kubectl_json(vec![
        "get".into(),
        "pods".into(),
        "--all-namespaces".into(),
        "-o".into(),
        "json".into(),
    ])
    .await?;
    let node_items = nodes["items"].as_array().cloned().unwrap_or_default();
    let pod_items = pods["items"].as_array().cloned().unwrap_or_default();
    let ready_nodes = node_items
        .iter()
        .filter(|node| {
            node["status"]["conditions"]
                .as_array()
                .map(|conditions| {
                    conditions.iter().any(|condition| {
                        condition["type"] == "Ready" && condition["status"] == "True"
                    })
                })
                .unwrap_or(false)
        })
        .count();
    let unhealthy_pods = pod_items
        .iter()
        .filter(|pod| {
            !matches!(
                pod["status"]["phase"].as_str(),
                Some("Running" | "Succeeded")
            )
        })
        .count();
    Ok(Json(json!({
        "available": true,
        "version": version,
        "nodes": node_items.len(),
        "readyNodes": ready_nodes,
        "pods": pod_items.len(),
        "unhealthyPods": unhealthy_pods
    })))
}

async fn namespaces() -> Result<Json<Value>, AppError> {
    Ok(Json(
        kubectl_json(vec![
            "get".into(),
            "namespaces".into(),
            "-o".into(),
            "json".into(),
        ])
        .await?,
    ))
}

async fn resources(Query(query): Query<ResourceQuery>) -> Result<Json<Value>, AppError> {
    let kind = resource_kind(&query.kind)?;
    let namespace = query.namespace.as_deref().unwrap_or("_all");
    let mut args = vec!["get".into(), kind.kubectl.into()];
    args.extend(namespace_args(kind, namespace)?);
    args.extend(["-o".into(), "json".into()]);
    Ok(Json(kubectl_json(args).await?))
}

async fn resource_detail(
    Path((kind, namespace, name)): Path<(String, String, String)>,
) -> Result<Json<Value>, AppError> {
    let kind = resource_kind(&kind)?;
    validate_name(&name, "资源名称")?;
    let mut args = vec!["get".into(), kind.kubectl.into(), name];
    args.extend(namespace_args(kind, &namespace)?);
    args.extend(["-o".into(), "json".into()]);
    Ok(Json(kubectl_json(args).await?))
}

async fn resource_yaml(
    Path((kind, namespace, name)): Path<(String, String, String)>,
) -> Result<Json<TextResult>, AppError> {
    let kind = resource_kind(&kind)?;
    validate_name(&name, "资源名称")?;
    let mut args = vec!["get".into(), kind.kubectl.into(), name];
    args.extend(namespace_args(kind, &namespace)?);
    args.extend(["-o".into(), "yaml".into()]);
    Ok(Json(TextResult {
        text: kubectl(args, COMMAND_TIMEOUT).await?,
    }))
}

async fn resource_events(
    Path((kind, namespace, name)): Path<(String, String, String)>,
) -> Result<Json<Value>, AppError> {
    let kind = resource_kind(&kind)?;
    validate_name(&name, "资源名称")?;
    let mut args = vec!["get".into(), "events".into()];
    args.extend(namespace_args(kind, &namespace)?);
    args.extend([
        "--field-selector".into(),
        format!("involvedObject.name={name}"),
        "-o".into(),
        "json".into(),
    ]);
    Ok(Json(kubectl_json(args).await?))
}

async fn pod_logs(
    Path((namespace, name)): Path<(String, String)>,
    Query(query): Query<LogsQuery>,
) -> Result<Json<TextResult>, AppError> {
    validate_name(&namespace, "命名空间")?;
    validate_name(&name, "Pod 名称")?;
    let tail = query.tail.unwrap_or(500).min(MAX_LOG_TAIL);
    let mut args = vec![
        "logs".into(),
        name,
        "--namespace".into(),
        namespace,
        "--tail".into(),
        tail.to_string(),
        "--timestamps".into(),
    ];
    if let Some(container) = query.container.filter(|value| !value.is_empty()) {
        validate_name(&container, "容器名称")?;
        args.extend(["--container".into(), container]);
    }
    Ok(Json(TextResult {
        text: kubectl(args, LOG_TIMEOUT).await?,
    }))
}

fn workload_kind(id: &str, scalable: bool) -> Result<ResourceKind, AppError> {
    let allowed = if scalable {
        ["deployments", "statefulsets"].as_slice()
    } else {
        ["deployments", "statefulsets", "daemonsets"].as_slice()
    };
    if !allowed.contains(&id) {
        return Err(AppError::bad("该工作负载不支持此操作"));
    }
    resource_kind(id)
}

async fn workload_restart(
    Path((kind, namespace, name)): Path<(String, String, String)>,
) -> Result<Json<Value>, AppError> {
    let kind = workload_kind(&kind, false)?;
    validate_name(&namespace, "命名空间")?;
    validate_name(&name, "资源名称")?;
    let text = kubectl(
        vec![
            "rollout".into(),
            "restart".into(),
            format!("{}/{}", kind.kubectl, name),
            "--namespace".into(),
            namespace,
        ],
        COMMAND_TIMEOUT,
    )
    .await?;
    Ok(Json(json!({ "ok": true, "message": text.trim() })))
}

async fn workload_scale(
    Path((kind, namespace, name)): Path<(String, String, String)>,
    Json(body): Json<ScaleBody>,
) -> Result<Json<Value>, AppError> {
    let kind = workload_kind(&kind, true)?;
    validate_name(&namespace, "命名空间")?;
    validate_name(&name, "资源名称")?;
    if body.replicas > 1000 {
        return Err(AppError::bad("副本数不能超过 1000"));
    }
    let text = kubectl(
        vec![
            "scale".into(),
            format!("{}/{}", kind.kubectl, name),
            format!("--replicas={}", body.replicas),
            "--namespace".into(),
            namespace,
        ],
        COMMAND_TIMEOUT,
    )
    .await?;
    Ok(Json(json!({ "ok": true, "message": text.trim() })))
}

async fn delete_pod(
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    validate_name(&namespace, "命名空间")?;
    validate_name(&name, "Pod 名称")?;
    let text = kubectl(
        vec![
            "delete".into(),
            "pod".into(),
            name,
            "--namespace".into(),
            namespace,
            "--wait=false".into(),
        ],
        COMMAND_TIMEOUT,
    )
    .await?;
    Ok(Json(json!({ "ok": true, "message": text.trim() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_allowlist_distinguishes_scope() {
        assert!(resource_kind("pods").unwrap().namespaced);
        assert!(!resource_kind("nodes").unwrap().namespaced);
        assert!(resource_kind("secrets").is_err());
    }

    #[test]
    fn names_reject_shell_and_path_characters() {
        assert!(validate_name("cis-map-0", "名称").is_ok());
        assert!(validate_name("../pod", "名称").is_err());
        assert!(validate_name("pod;id", "名称").is_err());
        assert!(validate_name("", "名称").is_err());
    }

    #[test]
    fn namespace_arguments_follow_resource_scope() {
        let pods = resource_kind("pods").unwrap();
        assert_eq!(
            namespace_args(pods, "_all").unwrap(),
            vec!["--all-namespaces"]
        );
        assert_eq!(
            namespace_args(pods, "cis").unwrap(),
            vec!["--namespace", "cis"]
        );
        let nodes = resource_kind("nodes").unwrap();
        assert!(namespace_args(nodes, "ignored").unwrap().is_empty());
    }
}
