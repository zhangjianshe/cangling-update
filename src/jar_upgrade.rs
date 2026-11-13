use crate::error::AppError;
use crate::state::AppState;
use axum::extract::{Multipart, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use uuid::Uuid;

const DEFAULT_DEPLOY_DIR: &str = "/opt/cangling/jar-deploy";
// 新疆现场一次性迁移凭据。任务完成后必须删除并发布清理版本。
const XINJIANG_CIFS_PASSWORD: &str = "admin123456";
const XINJIANG_REMOTE_NODES: [(&str, &str); 3] = [
    ("worker1", "10.166.11.21"),
    ("worker2", "10.166.11.22"),
    ("gpu1", "10.166.11.50"),
];
const MAX_JAR_BYTES: u64 = 1024 * 1024 * 1024;
const REQUIRED_JARS: [&str; 4] = [
    "cis-server-1.0.0.jar",
    "cis-map-1.0.0.jar",
    "cis-k8s-1.0.0.jar",
    "np4-base-server-1.0.0.jar",
];
static UPGRADING: AtomicBool = AtomicBool::new(false);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jar-upgrade/status", get(status))
        .route("/api/jar-upgrade/check", post(check_environment))
        .route("/api/jar-upgrade/upload", post(upload))
        .route("/api/jar-upgrade/run", post(run))
        .route("/api/jar-upgrade/repair-storage", post(repair_storage))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairStorageRequest {
    pub server: String,
    pub share: String,
    pub target_dir: String,
    pub username: String,
    #[serde(default)]
    pub options: String,
    #[serde(default)]
    pub check_only: bool,
}

#[derive(Debug, Clone)]
struct ExistingMount {
    source: String,
    fstype: String,
    options: String,
}

fn deploy_dir() -> PathBuf {
    std::env::var_os("CANGLING_JAR_DEPLOY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DEPLOY_DIR))
}

pub(crate) fn incoming_dir() -> PathBuf {
    deploy_dir().join("incoming")
}

fn checked_name(name: &str) -> Result<&'static str, AppError> {
    REQUIRED_JARS
        .iter()
        .copied()
        .find(|expected| *expected == name)
        .ok_or_else(|| AppError::bad(format!("不允许的 JAR 文件名：{name}")))
}

#[derive(Debug, Serialize)]
struct JarFileStatus {
    name: &'static str,
    present: bool,
    bytes: u64,
    modified: Option<u64>,
}

#[derive(Debug, Serialize)]
struct JarUpgradeStatus {
    incoming_dir: String,
    script: String,
    ready: bool,
    eligible_targets: usize,
    skipped_nodes: usize,
    upgrading: bool,
    active_job: Option<crate::progress::JobProgress>,
    files: Vec<JarFileStatus>,
    checked_at: Option<String>,
    nodes: Vec<JarNodeStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JarNodeStatus {
    name: String,
    address: String,
    ok: bool,
    message: String,
    targets: Vec<JarTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JarTarget {
    jar: String,
    container: String,
    path: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct JarEnvironmentReport {
    checked_at: String,
    nodes: Vec<JarNodeStatus>,
}

async fn load_status() -> Result<JarUpgradeStatus, AppError> {
    load_status_at(&deploy_dir()).await
}

async fn load_status_at(root: &Path) -> Result<JarUpgradeStatus, AppError> {
    let incoming = root.join("incoming");
    let script = root.join("upgrade-jars.sh");
    let mut files = Vec::with_capacity(REQUIRED_JARS.len());
    for name in REQUIRED_JARS {
        let metadata = tokio::fs::metadata(incoming.join(name)).await.ok();
        files.push(JarFileStatus {
            name,
            present: metadata
                .as_ref()
                .is_some_and(|item| item.is_file() && item.len() > 0),
            bytes: metadata.as_ref().map_or(0, |item| item.len()),
            modified: metadata
                .and_then(|item| item.modified().ok())
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_secs()),
        });
    }
    let report = tokio::fs::read(root.join("environment.json"))
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<JarEnvironmentReport>(&bytes).ok())
        .unwrap_or_default();
    let eligible_targets = report
        .nodes
        .iter()
        .filter(|node| node.ok)
        .flat_map(|node| &node.targets)
        .filter(|target| {
            files
                .iter()
                .any(|file| file.present && file.name == target.jar)
        })
        .count();
    let skipped_nodes = report.nodes.iter().filter(|node| !node.ok).count();
    let ready = eligible_targets > 0
        && tokio::fs::metadata(&script)
            .await
            .is_ok_and(|item| item.is_file());
    Ok(JarUpgradeStatus {
        incoming_dir: incoming.display().to_string(),
        script: script.display().to_string(),
        ready,
        eligible_targets,
        skipped_nodes,
        upgrading: UPGRADING.load(Ordering::SeqCst),
        active_job: None,
        files,
        checked_at: (!report.checked_at.is_empty()).then_some(report.checked_at),
        nodes: report.nodes,
    })
}

async fn status(State(state): State<AppState>) -> Result<Json<JarUpgradeStatus>, AppError> {
    let mut status = load_status().await?;
    let id = state
        .active_jar_upgrade_job
        .lock()
        .map_err(|_| AppError::internal("jar job lock"))?
        .clone();
    status.active_job = id.and_then(|id| state.jobs.get(&id));
    Ok(Json(status))
}

async fn check_environment(
    State(state): State<AppState>,
) -> Result<Json<JarUpgradeStatus>, AppError> {
    if UPGRADING.load(Ordering::SeqCst) {
        return Err(AppError::conflict("JAR 升级正在执行，暂时不能检查环境"));
    }
    let hosts = {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let mut stmt = conn.prepare(
            "SELECT name,host,ssh_port,username,kind FROM managed_hosts
             ORDER BY CASE kind WHEN 'local' THEN 0 WHEN 'cluster' THEN 1 ELSE 2 END,name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?.clamp(1, u16::MAX as i64) as u16,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let root = deploy_dir();
    let inspect_root = root.clone();
    let (report, script) =
        tokio::task::spawn_blocking(move || inspect_environment(hosts, &inspect_root))
            .await
            .map_err(|error| AppError::internal(format!("环境检查异常退出：{error}")))??;
    tokio::fs::create_dir_all(&root).await?;
    let report_bytes = serde_json::to_vec_pretty(&report)
        .map_err(|error| AppError::internal(format!("无法保存检查结果：{error}")))?;
    tokio::fs::write(root.join("environment.json"), report_bytes).await?;
    tokio::fs::write(root.join("upgrade-jars.sh"), script).await?;
    tokio::fs::set_permissions(
        root.join("upgrade-jars.sh"),
        std::fs::Permissions::from_mode(0o750),
    )
    .await?;
    load_status().await.map(Json)
}

#[derive(Debug)]
struct InspectedHost {
    status: JarNodeStatus,
    kind: String,
    host: String,
    port: u16,
    username: String,
}

fn inspect_environment(
    mut hosts: Vec<(String, String, u16, String, String)>,
    root: &Path,
) -> Result<(JarEnvironmentReport, String), AppError> {
    if !hosts.iter().any(|item| item.4 == "local") {
        hosts.insert(
            0,
            (
                "本机".into(),
                String::new(),
                22,
                "root".into(),
                "local".into(),
            ),
        );
    }
    let inspected = hosts
        .into_iter()
        .map(|(name, host, port, username, kind)| {
            inspect_jar_host(name, host, port, username, kind)
        })
        .collect::<Vec<_>>();
    let report = JarEnvironmentReport {
        checked_at: chrono::Utc::now().to_rfc3339(),
        nodes: inspected.iter().map(|item| item.status.clone()).collect(),
    };
    let script = generate_upgrade_script(root, &inspected)?;
    Ok((report, script))
}

fn inspect_jar_host(
    name: String,
    host: String,
    port: u16,
    username: String,
    kind: String,
) -> InspectedHost {
    let address = if kind == "local" {
        "本机".to_string()
    } else {
        format!("{username}@{host}:{port}")
    };
    let output = if kind == "local" {
        let mut targets = discover_jar_targets(Command::new("docker")).unwrap_or_default();
        targets.extend(discover_k3s_jar_targets().unwrap_or_default());
        targets.sort_by(|a, b| {
            a.jar
                .cmp(&b.jar)
                .then_with(|| a.container.cmp(&b.container))
        });
        targets.dedup_by(|a, b| a.jar == b.jar && a.container == b.container && a.path == b.path);
        Ok(targets)
    } else {
        discover_remote_jar_targets(&host, port, &username)
    };
    let status = match output {
        Ok(targets) if targets.is_empty() => JarNodeStatus {
            name,
            address,
            ok: true,
            message: "未发现挂载规定 JAR 的运行容器".into(),
            targets,
        },
        Ok(targets) => JarNodeStatus {
            name,
            address,
            ok: true,
            message: format!("发现 {} 个待升级容器", targets.len()),
            targets,
        },
        Err(error) => JarNodeStatus {
            name,
            address,
            ok: false,
            message: error.to_string(),
            targets: Vec::new(),
        },
    };
    InspectedHost {
        status,
        kind,
        host,
        port,
        username,
    }
}

fn discover_k3s_jar_targets() -> Result<Vec<JarTarget>, AppError> {
    let k3s = ["/usr/local/bin/k3s", "/usr/bin/k3s", "/opt/bin/k3s"]
        .iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .ok_or_else(|| AppError::internal("未安装 k3s"))?;
    let output = Command::new(k3s)
        .args([
            "kubectl",
            "-n",
            "cangling",
            "get",
            "deployments",
            "-o",
            "json",
        ])
        .output()
        .map_err(|error| AppError::internal(format!("无法检查 K3s Deployment：{error}")))?;
    if !output.status.success() {
        return Err(AppError::internal(output_tail(
            &output.stderr,
            "无法读取 K3s Deployment",
        )));
    }
    let document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| AppError::internal(format!("解析 K3s Deployment 失败：{error}")))?;
    let mut targets = Vec::new();
    for deployment in document
        .get("items")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = deployment
            .pointer("/metadata/name")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        for volume in deployment
            .pointer("/spec/template/spec/volumes")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(source) = volume
                .pointer("/hostPath/path")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let Some(jar) = Path::new(source)
                .file_name()
                .and_then(|value| value.to_str())
            else {
                continue;
            };
            if REQUIRED_JARS.contains(&jar) {
                targets.push(JarTarget {
                    jar: jar.into(),
                    container: format!("k3s/{name}"),
                    path: source.into(),
                });
            }
        }
    }
    Ok(targets)
}

fn discover_remote_jar_targets(
    host: &str,
    port: u16,
    username: &str,
) -> Result<Vec<JarTarget>, AppError> {
    const SCRIPT: &str = r#"set -eu
for c in $(docker ps --format '{{.Names}}'); do
  source=$(docker inspect -f '{{range .Mounts}}{{if eq .Destination "/app/app.jar"}}{{.Source}}{{end}}{{end}}' "$c")
  [ -n "$source" ] && printf '%s|%s\n' "$c" "$source"
done
"#;
    let destination = format!("{username}@{host}");
    let mut child = Command::new("ssh")
        .args([
            "-p",
            &port.to_string(),
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "StrictHostKeyChecking=accept-new",
            &destination,
            "sh",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| AppError::internal(format!("无法连接 {destination}：{error}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| AppError::internal("无法发送远程检查脚本"))?
        .write_all(SCRIPT.as_bytes())
        .map_err(|error| AppError::internal(format!("发送远程检查脚本失败：{error}")))?;
    let output = child
        .wait_with_output()
        .map_err(|error| AppError::internal(format!("等待 {destination} 检查失败：{error}")))?;
    if !output.status.success() {
        return Err(AppError::internal(output_tail(
            &output.stderr,
            "远程 Docker 检查失败",
        )));
    }
    let mut targets = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((container, source)) = line.split_once('|') else {
            continue;
        };
        let Some(jar) = Path::new(source).file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if REQUIRED_JARS.contains(&jar) {
            targets.push(JarTarget {
                jar: jar.into(),
                container: container.into(),
                path: source.into(),
            });
        }
    }
    targets.sort_by(|a, b| {
        a.jar
            .cmp(&b.jar)
            .then_with(|| a.container.cmp(&b.container))
    });
    targets.dedup_by(|a, b| a.jar == b.jar && a.container == b.container && a.path == b.path);
    Ok(targets)
}

fn discover_jar_targets(mut command: Command) -> Result<Vec<JarTarget>, AppError> {
    command
        .args(["ps", "--format", "{{.Names}}"])
        .stdout(Stdio::piped());
    let output = command
        .output()
        .map_err(|error| AppError::internal(format!("无法执行 Docker 检查：{error}")))?;
    if !output.status.success() {
        return Err(AppError::internal(output_tail(
            &output.stderr,
            "无法读取运行容器",
        )));
    }
    let names = String::from_utf8_lossy(&output.stdout);
    let mut targets = Vec::new();
    for container in names.lines().map(str::trim).filter(|name| !name.is_empty()) {
        let output = Command::new("docker")
            .args(["inspect", container])
            .output()
            .map_err(|error| AppError::internal(format!("无法检查容器 {container}：{error}")))?;
        if !output.status.success() {
            continue;
        }
        let Ok(documents) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout) else {
            continue;
        };
        let Some(mounts) = documents
            .first()
            .and_then(|document| document.get("Mounts"))
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for mount in mounts {
            let Some(source) = mount.get("Source").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(jar) = Path::new(source).file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if REQUIRED_JARS.contains(&jar) {
                targets.push(JarTarget {
                    jar: jar.into(),
                    container: container.into(),
                    path: source.into(),
                });
            }
        }
    }
    targets.sort_by(|a, b| {
        a.jar
            .cmp(&b.jar)
            .then_with(|| a.container.cmp(&b.container))
    });
    targets.dedup_by(|a, b| a.jar == b.jar && a.container == b.container && a.path == b.path);
    Ok(targets)
}

fn generate_upgrade_script(root: &Path, hosts: &[InspectedHost]) -> Result<String, AppError> {
    let mut script = String::from("#!/bin/sh\nset -eu\nRUN_ID=$(date +%Y%m%d-%H%M%S)\nINCOMING=");
    script.push_str(&shell_quote(&root.join("incoming").display().to_string()));
    script.push_str("\nUPDATED=0\n");
    let skipped = hosts
        .iter()
        .filter(|node| !node.status.ok)
        .collect::<Vec<_>>();
    if !skipped.is_empty() {
        let names = skipped
            .iter()
            .map(|node| node.status.name.as_str())
            .collect::<Vec<_>>()
            .join("、");
        script.push_str(&format!(
            "printf '%s\\n' {} >&2\n",
            shell_quote(&format!("跳过检查失败节点（不会升级）：{names}"))
        ));
    }
    for node in hosts {
        if !node.status.ok {
            continue;
        }
        for target in &node.status.targets {
            let incoming = format!("$INCOMING/{}", target.jar);
            let label = shell_quote(&format!("{} / {}", node.status.name, target.container));
            script.push_str(&format!(
                "if [ -f {incoming} ]; then printf 'CK_JAR_PROGRESS|start|%s\\n' {label}; fi\n"
            ));
            if node.kind == "local" {
                let backup_dir = root.join("backups");
                let backup = format!(
                    "{}/\"$RUN_ID\"/{}/{}",
                    shell_quote(&backup_dir.display().to_string()),
                    safe_name(&node.status.name),
                    shell_quote(&target.jar)
                );
                if let Some(deployment) = target.container.strip_prefix("k3s/") {
                    script.push_str(&format!(
                        "if [ -f {incoming} ]; then\n  mkdir -p {} $(dirname {})\n  [ ! -f {} ] || cp -a {} {}\n  install -m 0644 {incoming} {}\n  if ! (/usr/local/bin/k3s kubectl -n cangling rollout restart deployment/{} >/dev/null && /usr/local/bin/k3s kubectl -n cangling rollout status deployment/{} --timeout=180s); then\n    [ ! -f {} ] || cp -a {} {}\n    /usr/local/bin/k3s kubectl -n cangling rollout restart deployment/{} >/dev/null || true\n    echo 'K3s JAR 升级失败，已恢复 {}' >&2\n    exit 1\n  fi\n  UPDATED=$((UPDATED+1))\nfi\n",
                        format!("{}/\"$RUN_ID\"/{}", shell_quote(&backup_dir.display().to_string()), safe_name(&node.status.name)),
                        shell_quote(&target.path),
                        shell_quote(&target.path), shell_quote(&target.path), &backup,
                        shell_quote(&target.path),
                        shell_quote(deployment), shell_quote(deployment),
                        &backup, &backup, shell_quote(&target.path), shell_quote(deployment), shell_quote(deployment),
                    ));
                    script.push_str(&format!("if [ -f {incoming} ]; then printf 'CK_JAR_PROGRESS|done|%s\\n' {label}; fi\n"));
                    continue;
                }
                script.push_str(&format!(
                    "if [ -f {incoming} ]; then\n  mkdir -p {}\n  cp -a {} {}\n  if ! (install -m 0644 {incoming} {} && docker restart {} >/dev/null && test \"$(docker inspect -f '{{{{.State.Running}}}}' {})\" = true); then\n    cp -a {} {}\n    docker restart {} >/dev/null || true\n    echo '升级失败，已恢复 {}' >&2\n    exit 1\n  fi\n  UPDATED=$((UPDATED+1))\nfi\n",
                    format!("{}/\"$RUN_ID\"/{}", shell_quote(&backup_dir.display().to_string()), safe_name(&node.status.name)),
                    shell_quote(&target.path),
                    &backup,
                    shell_quote(&target.path),
                    shell_quote(&target.container),
                    shell_quote(&target.container),
                    &backup,
                    shell_quote(&target.path),
                    shell_quote(&target.container),
                    shell_quote(&target.container),
                ));
            } else {
                let destination = format!("{}@{}", node.username, node.host);
                let remote_backup_dir = format!(
                    "/opt/cangling/jar-backups/\"$RUN_ID\"/{}",
                    safe_name(&node.status.name)
                );
                let remote_backup = format!("{}/{}", remote_backup_dir, shell_quote(&target.jar));
                let remote_temp = format!("{}.cangling-update-$RUN_ID", target.path);
                let ssh = format!(
                    "ssh -p {} -o BatchMode=yes -o StrictHostKeyChecking=accept-new {}",
                    node.port,
                    shell_quote(&destination)
                );
                let scp = format!(
                    "scp -P {} -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
                    node.port
                );
                let remote_command = format!(
                    "set -eu; RUN_ID=$(date +%Y%m%d-%H%M%S); mkdir -p {}; cp -a {} {}; if ! (install -m 0644 {} {} && rm -f {} && docker restart {} >/dev/null && test \\\"$(docker inspect -f '{{{{.State.Running}}}}' {})\\\" = true); then cp -a {} {}; docker restart {} >/dev/null || true; exit 1; fi",
                    remote_backup_dir,
                    shell_quote(&target.path), shell_quote(&remote_backup),
                    shell_quote(&remote_temp), shell_quote(&target.path), shell_quote(&remote_temp),
                    shell_quote(&target.container), shell_quote(&target.container),
                    shell_quote(&remote_backup), shell_quote(&target.path), shell_quote(&target.container),
                );
                script.push_str(&format!(
                    "if [ -f {incoming} ]; then\n  {scp} {incoming} {}\n  {ssh} {}\n  UPDATED=$((UPDATED+1))\nfi\n",
                    shell_quote(&format!("{destination}:{remote_temp}")),
                    shell_quote(&remote_command),
                ));
            }
            script.push_str(&format!(
                "if [ -f {incoming} ]; then printf 'CK_JAR_PROGRESS|done|%s\\n' {label}; fi\n"
            ));
        }
    }
    script.push_str("[ \"$UPDATED\" -gt 0 ] || { echo '没有发现与本次上传匹配的运行容器' >&2; exit 1; }\necho \"JAR 升级完成，共更新 $UPDATED 个容器\"\n");
    if !skipped.is_empty() {
        script.push_str(&format!(
            "echo '已跳过 {} 个检查失败节点，未对其执行升级'\n",
            skipped.len()
        ));
    }
    Ok(script)
}

fn safe_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn upload(mut multipart: Multipart) -> Result<Json<JarUpgradeStatus>, AppError> {
    if UPGRADING.load(Ordering::SeqCst) {
        return Err(AppError::conflict("JAR 升级正在执行，暂时不能覆盖上传目录"));
    }
    let incoming = incoming_dir();
    tokio::fs::create_dir_all(&incoming).await?;
    let mut received = Vec::new();
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| AppError::bad(format!("无效的上传数据：{error}")))?
    {
        if field.name() != Some("files") {
            continue;
        }
        let raw_name = field
            .file_name()
            .ok_or_else(|| AppError::bad("上传文件缺少文件名"))?;
        let name = checked_name(raw_name)?;
        let temp = incoming.join(format!(".{name}.{}.part", Uuid::new_v4()));
        let target = incoming.join(name);
        let write_result: Result<(), AppError> = async {
            let mut size = 0u64;
            let mut file = tokio::fs::File::create(&temp).await?;
            while let Some(chunk) = field
                .chunk()
                .await
                .map_err(|error| AppError::bad(format!("读取 {name} 失败：{error}")))?
            {
                size = size
                    .checked_add(chunk.len() as u64)
                    .ok_or_else(|| AppError::bad("上传文件大小溢出"))?;
                if size > MAX_JAR_BYTES {
                    return Err(AppError::bad(format!("{name} 不能超过 1 GiB")));
                }
                file.write_all(&chunk).await?;
            }
            if size == 0 {
                return Err(AppError::bad(format!("{name} 是空文件")));
            }
            file.flush().await?;
            file.sync_all().await?;
            tokio::fs::rename(&temp, &target).await?;
            Ok(())
        }
        .await;
        if let Err(error) = write_result {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(error);
        }
        received.push(name);
    }
    if received.is_empty() {
        return Err(AppError::bad("请至少选择一个规定名称的 JAR 文件"));
    }
    // An upload request defines one upgrade batch. Remove supported JARs left
    // over from an earlier batch so they cannot be upgraded accidentally.
    for name in REQUIRED_JARS {
        if !received.contains(&name) {
            match tokio::fs::remove_file(incoming.join(name)).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    load_status().await.map(Json)
}

async fn run(
    State(state): State<AppState>,
) -> Result<Json<crate::progress::JobProgress>, AppError> {
    let current = load_status().await?;
    if !current.ready {
        return Err(AppError::bad(
            "请先上传 JAR 并检查环境：至少需要一个检查通过且匹配本批次 JAR 的目标，以及已生成的升级脚本",
        ));
    }
    if UPGRADING.swap(true, Ordering::SeqCst) {
        return Err(AppError::conflict("已有 JAR 升级任务正在执行"));
    }
    let job = state.jobs.create();
    let job_id = job.id.clone();
    let jobs = state.jobs.clone();
    let active = state.active_jar_upgrade_job.clone();
    match active.lock() {
        Ok(mut slot) => *slot = Some(job_id.clone()),
        Err(_) => {
            UPGRADING.store(false, Ordering::SeqCst);
            return Err(AppError::internal("jar job lock"));
        }
    }
    let script = deploy_dir().join("upgrade-jars.sh");
    tokio::spawn(async move {
        // Older generated scripts still run; show an indeterminate indicator
        // until a fresh environment check generates progress markers.
        let progress_total = if tokio::fs::read_to_string(&script)
            .await
            .is_ok_and(|text| text.contains("CK_JAR_PROGRESS|"))
        {
            current.eligible_targets as u64
        } else {
            0
        };
        jobs.set(
            &job_id,
            "upgrade",
            &format!(
                "正在升级 {} 个匹配目标，跳过 {} 个检查失败节点…",
                current.eligible_targets, current.skipped_nodes
            ),
            0,
            progress_total,
        );
        let result = execute_upgrade_script(&script, &jobs, &job_id, progress_total).await;
        match result {
            Ok((status, stdout, _)) if status.success() => {
                let message = output_tail(&stdout, "JAR 升级完成");
                jobs.finish_ok(&job_id, &message);
            }
            Ok((status, stdout, stderr)) => {
                let fallback = format!("升级脚本退出码：{status}");
                let message = output_tail(&stderr, &output_tail(&stdout, &fallback));
                jobs.finish_err(&job_id, &message);
            }
            Err(error) => {
                jobs.finish_err(&job_id, &format!("无法执行 {}：{error}", script.display()))
            }
        }
        if let Ok(mut slot) = active.lock() {
            if slot.as_deref() == Some(&job_id) {
                *slot = None;
            }
        }
        UPGRADING.store(false, Ordering::SeqCst);
    });
    Ok(Json(job))
}

fn record_upgrade_progress(
    line: &str,
    jobs: &crate::progress::JobHub,
    id: &str,
    completed: &mut u64,
    total: u64,
) -> bool {
    let Some(marker) = line.strip_prefix("CK_JAR_PROGRESS|") else {
        return false;
    };
    let Some((phase, label)) = marker.split_once('|') else {
        return false;
    };
    if !matches!(phase, "start" | "done") {
        return false;
    }
    if phase == "done" {
        *completed = completed.saturating_add(1).min(total);
    }
    let label = label.chars().take(300).collect::<String>();
    let message = if phase == "start" {
        format!("正在升级 {label}（已完成 {completed}/{total}）")
    } else {
        format!("已完成 {completed}/{total}：{label}")
    };
    jobs.set(id, "upgrade", &message, *completed, total);
    true
}
async fn capture_upgrade_output<R: AsyncRead + Unpin>(
    stream: R,
    mut progress: impl FnMut(&str) -> bool,
) -> std::io::Result<Vec<u8>> {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    let mut tail = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).await? == 0 {
            break;
        }
        if progress(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n'])) {
            continue;
        }
        tail.extend_from_slice(&line);
        if tail.len() > 16384 {
            tail.drain(..tail.len() - 16384);
        }
    }
    Ok(tail)
}
async fn execute_upgrade_script(
    script: &Path,
    jobs: &crate::progress::JobHub,
    id: &str,
    total: u64,
) -> std::io::Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    let mut child = tokio::process::Command::new(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let mut completed = 0;
    let (out, err, status) = tokio::join!(
        capture_upgrade_output(stdout, |line| record_upgrade_progress(
            line,
            jobs,
            id,
            &mut completed,
            total
        )),
        capture_upgrade_output(stderr, |_| false),
        child.wait()
    );
    Ok((status?, out?, err?))
}

async fn repair_storage(
    State(state): State<AppState>,
    Json(body): Json<RepairStorageRequest>,
) -> Result<Json<crate::progress::JobProgress>, AppError> {
    validate_repair_request(&body)?;
    if UPGRADING.swap(true, Ordering::SeqCst) {
        return Err(AppError::conflict("已有 JAR 升级或存储修复任务正在执行"));
    }
    let preflight = tokio::task::spawn_blocking({
        let local_check = body.clone();
        move || {
            preflight_storage_local(&local_check)?;
            for (name, addr) in XINJIANG_REMOTE_NODES {
                remote_storage_action(addr, true).map_err(|error| {
                    AppError::bad(format!(
                        "节点 {name}（{addr}）预检失败，未执行任何修改：{error}"
                    ))
                })?;
            }
            Ok::<(), AppError>(())
        }
    })
    .await
    .map_err(|error| AppError::internal(format!("存储预检异常退出：{error}")))?;
    if let Err(error) = preflight {
        UPGRADING.store(false, Ordering::SeqCst);
        return Err(error);
    }
    let job = state.jobs.create();
    let job_id = job.id.clone();
    let jobs = state.jobs.clone();
    tokio::spawn(async move {
        jobs.set(&job_id, "storage-repair", "正在检查集群共享存储…", 0, 0);
        let mut lines = Vec::new();
        let local_body = body.clone();
        let local = tokio::task::spawn_blocking(move || repair_storage_local(&local_body)).await;
        match local {
            Ok(Ok(message)) => lines.push(format!("[本机] {message}")),
            Ok(Err(error)) => {
                UPGRADING.store(false, Ordering::SeqCst);
                jobs.finish_err(&job_id, &format!("[本机] 修复失败：{error}"));
                return;
            }
            Err(error) => {
                UPGRADING.store(false, Ordering::SeqCst);
                jobs.finish_err(&job_id, &format!("[本机] 修复任务异常退出：{error}"));
                return;
            }
        }

        let mut failed = false;
        for (name, addr) in XINJIANG_REMOTE_NODES {
            jobs.set(
                &job_id,
                "storage-repair",
                &format!("正在修复节点 {name}…"),
                0,
                0,
            );
            match tokio::task::spawn_blocking(move || remote_storage_action(addr, false)).await {
                Ok(Ok(message)) => lines.push(format!("[{name}] {message}")),
                Ok(Err(error)) => {
                    failed = true;
                    lines.push(format!("[{name}] 失败：{error}"));
                }
                Err(error) => {
                    failed = true;
                    lines.push(format!("[{name}] 修复任务异常退出：{error}"));
                }
            }
        }
        UPGRADING.store(false, Ordering::SeqCst);
        let message = lines.join("\n");
        if failed {
            jobs.finish_err(&job_id, &message);
        } else {
            jobs.finish_ok(&job_id, &message);
        }
    });
    Ok(Json(job))
}

fn remote_storage_action(host: &str, check_only: bool) -> Result<String, AppError> {
    const SCRIPT: &str = r#"set -Eeuo pipefail
TARGET=/nfs1
EXPECTED_NFS=10.166.48.1:/yw
EXPECTED_CIFS=//10.166.48.1/yw
CRED=/etc/cangling-update/storage-nfs1.credentials
HEALTH=/usr/local/sbin/cangling-cifs-nfs1-health.sh
CRON=/etc/cron.d/cangling-cifs-nfs1-health
MODE=${1:-check}

command -v mount.cifs >/dev/null || { echo '未安装 mount.cifs（cifs-utils）' >&2; exit 20; }
SOURCE=$(findmnt -n -o SOURCE --target "$TARGET" 2>/dev/null || true)
TYPE=$(findmnt -n -o FSTYPE --target "$TARGET" 2>/dev/null || true)
if [ -z "$SOURCE" ]; then
  SOURCE=$(awk '$2=="/nfs1" && $1 !~ /^#/ {print $1; exit}' /etc/fstab)
  TYPE=$(awk '$2=="/nfs1" && $1 !~ /^#/ {print $3; exit}' /etc/fstab)
fi
if ! { [ "$TYPE" = nfs ] || [ "$TYPE" = nfs4 ] || { [ "$TYPE" = cifs ] && [ "$SOURCE" = "$EXPECTED_CIFS" ]; }; }; then
  echo "拒绝修改：$TARGET 当前为 ${TYPE:-未知} ${SOURCE:-未配置}" >&2; exit 21
fi
if [ "$TYPE" != cifs ] && [ "$SOURCE" != "$EXPECTED_NFS" ]; then
  echo "拒绝修改：报告记录的 NFS 是 $EXPECTED_NFS，实际为 $SOURCE" >&2; exit 22
fi
if [ "$MODE" = check ]; then
  echo "SSH 与挂载预检通过：$SOURCE -> $EXPECTED_CIFS"; exit 0
fi

AFFECTED=''
if command -v docker >/dev/null 2>&1; then
  for id in $(docker ps -aq); do
    if docker inspect --format '{{range .Mounts}}{{println .Source}}{{end}}' "$id" 2>/dev/null | awk '$0=="/nfs1" || index($0,"/nfs1/")==1 {found=1} END{exit !found}'; then
      AFFECTED="$AFFECTED $id"
    fi
  done
fi

cp -an /etc/fstab /etc/fstab.cangling-update-storage.bak
mkdir -p /etc/cangling-update
chmod 700 /etc/cangling-update
cat >"$CRED" <<'CREDENTIALS'
username=nyc
password=admin123456
CREDENTIALS
chmod 600 "$CRED"
OPTS="credentials=$CRED,rw,_netdev,nofail"
tmp=$(mktemp /etc/fstab.cangling.XXXXXX)
awk -v src="$EXPECTED_CIFS" -v m="$TARGET" -v opts="$OPTS" '
  $2==m && $1 !~ /^#/ {if (!done) print src, m, "cifs", opts, "0 0"; done=1; next}
  {print}
  END {if (!done) print src, m, "cifs", opts, "0 0"}
' /etc/fstab >"$tmp"
chmod --reference=/etc/fstab "$tmp"
mv "$tmp" /etc/fstab

rollback() {
  umount "$TARGET" >/dev/null 2>&1 || true
  cp /etc/fstab.cangling-update-storage.bak /etc/fstab
  mount "$TARGET" >/dev/null 2>&1 || true
}
trap 'rollback' ERR
if [ "$TYPE" != cifs ]; then umount "$TARGET"; fi
mountpoint -q "$TARGET" || mount "$TARGET"
test "$(findmnt -n -o FSTYPE --target "$TARGET")" = cifs
test "$(findmnt -n -o SOURCE --target "$TARGET")" = "$EXPECTED_CIFS"
probe="$TARGET/.cangling-update-write-test-$$"
printf ok >"$probe" && sync "$probe" && rm -f "$probe"

cat >"$HEALTH" <<'HEALTHCHECK'
#!/bin/sh
set -eu
TARGET=/nfs1
EXPECTED=//10.166.48.1/yw
TYPE=$(findmnt -n -o FSTYPE --target "$TARGET" 2>/dev/null || true)
SOURCE=$(findmnt -n -o SOURCE --target "$TARGET" 2>/dev/null || true)
if [ "$TYPE" = cifs ] && [ "$SOURCE" = "$EXPECTED" ]; then exit 0; fi
mountpoint -q "$TARGET" && umount "$TARGET"
mount "$TARGET"
HEALTHCHECK
chmod 750 "$HEALTH"
cat >"$CRON" <<'CRONTASK'
SHELL=/bin/sh
PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
*/30 * * * * root /usr/local/sbin/cangling-cifs-nfs1-health.sh >/dev/null 2>&1
CRONTASK
chmod 644 "$CRON"

for file in /etc/crontab /var/spool/cron/root /var/spool/cron/crontabs/root /etc/cron.d/*; do
  [ -f "$file" ] || continue
  [ "$file" = "$CRON" ] && continue
  case "$file" in *.bak|*.cangling-update-*) continue;; esac
  if grep -Eiv '^[[:space:]]*#' "$file" | grep -Eqi '/nfs1|check_nfs|nfs_check|mount.*nfs'; then
    cp -an "$file" "$file.cangling-update-nfs.bak"
    sed -i -E '/^[[:space:]]*#/!{/\/nfs1|check_nfs|nfs_check|mount.*nfs/I s|^|# cangling-update disabled legacy NFS task: |}' "$file"
  fi
done

trap - ERR
if [ -n "$AFFECTED" ]; then docker restart $AFFECTED >/dev/null; fi
count=$(printf '%s\n' "$AFFECTED" | wc -w)
echo "已切换为 $EXPECTED_CIFS，写入验证通过，已更新半小时检查任务，重启 $count 个关联容器"
"#;

    let destination = format!("root@{host}");
    let mut child = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "StrictHostKeyChecking=accept-new",
            &destination,
            "bash",
            "-s",
            "--",
            if check_only { "check" } else { "apply" },
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| AppError::internal(format!("无法连接 {host}：{error}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| AppError::internal("无法写入远程修复脚本"))?
        .write_all(SCRIPT.as_bytes())
        .map_err(|error| AppError::internal(format!("发送远程修复脚本失败：{error}")))?;
    let output = child
        .wait_with_output()
        .map_err(|error| AppError::internal(format!("等待 {host} 执行失败：{error}")))?;
    if !output.status.success() {
        return Err(AppError::internal(output_tail(
            &output.stderr,
            "远程修复脚本执行失败",
        )));
    }
    Ok(output_tail(&output.stdout, "远程操作完成"))
}

fn preflight_storage_local(body: &RepairStorageRequest) -> Result<String, AppError> {
    let target = body.target_dir.trim();
    let existing = find_existing_mount(target)
        .ok_or_else(|| AppError::bad(format!("未找到挂载点 {target} 的原 NFS 配置")))?;
    if existing.fstype != "nfs" && existing.fstype != "nfs4" && existing.fstype != "cifs" {
        return Err(AppError::bad(format!(
            "{target} 当前文件系统为 {}，拒绝自动替换",
            existing.fstype
        )));
    }
    let share = if body.share.trim().is_empty() {
        derive_share_name(&existing.source)?
    } else {
        body.share.trim().to_string()
    };
    if !Path::new("/sbin/mount.cifs").exists()
        && !Path::new("/usr/sbin/mount.cifs").exists()
        && !Path::new("/usr/bin/mount.cifs").exists()
    {
        return Err(AppError::bad("未安装 mount.cifs（请先安装 cifs-utils）"));
    }
    Ok(format!(
        "已识别 {} {}，将切换为 //{}/{}",
        existing.fstype,
        existing.source,
        body.server.trim(),
        share
    ))
}

fn validate_repair_request(body: &RepairStorageRequest) -> Result<(), AppError> {
    let valid_atom = |value: &str| {
        !value.trim().is_empty() && !value.contains(['\n', '\r', '\0']) && value.len() <= 255
    };
    if !valid_atom(&body.server) || body.server.contains('/') {
        return Err(AppError::bad("CIFS 服务器地址无效"));
    }
    if (!body.share.trim().is_empty() && !valid_atom(&body.share))
        || body.share.contains('/')
        || body.share == "."
        || body.share == ".."
    {
        return Err(AppError::bad("CIFS 共享名称无效"));
    }
    if !valid_atom(&body.username) {
        return Err(AppError::bad("CIFS 用户名无效"));
    }
    if body.server.trim() != "10.166.48.1"
        || (!body.share.trim().is_empty() && body.share.trim() != "yw")
        || body.target_dir.trim() != "/nfs1"
        || body.username.trim() != "nyc"
        || !body.options.trim().is_empty()
    {
        return Err(AppError::bad(
            "新疆一次性修复仅允许 10.166.48.1:/yw 对应的 CIFS 共享 //10.166.48.1/yw 挂载到 /nfs1",
        ));
    }
    let target = Path::new(body.target_dir.trim());
    if !target.is_absolute()
        || body.target_dir.trim() == "/"
        || body.target_dir.split('/').any(|part| part == "..")
        || !body
            .target_dir
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '-' | '_' | '.'))
    {
        return Err(AppError::bad("挂载点必须是安全的绝对路径，且不能为根目录"));
    }
    Ok(())
}

fn repair_storage_local(body: &RepairStorageRequest) -> Result<String, AppError> {
    let target = body.target_dir.trim();
    let existing = find_existing_mount(target)
        .ok_or_else(|| AppError::bad(format!("未找到挂载点 {target} 的原 NFS 配置")))?;
    let share = if body.share.trim().is_empty() {
        derive_share_name(&existing.source)?
    } else {
        body.share.trim().to_string()
    };
    if existing.fstype != "cifs" && existing.source != "10.166.48.1:/yw" {
        return Err(AppError::bad(format!(
            "报告记录的 /nfs1 来源是 10.166.48.1:/yw，实际为 {}，拒绝修改",
            existing.source
        )));
    }
    if existing.fstype != "nfs" && existing.fstype != "nfs4" {
        let expected = format!("//{}/{}", body.server.trim(), share);
        if existing.fstype == "cifs" && existing.source.trim_end_matches('/') == expected {
            verify_writable(target)?;
            let schedule = install_cifs_health_check(target, &expected)?;
            let restarted = restart_affected_containers(target)?;
            return Ok(format!(
                "已经是目标 CIFS 挂载，写入验证通过；{schedule}；重启 {restarted} 个关联容器"
            ));
        }
        return Err(AppError::bad(format!(
            "{target} 当前是 {}（{}），不是可替换的 NFS 挂载",
            existing.fstype, existing.source
        )));
    }

    crate::storage::unmount_dir(target)?;
    let mount_result = crate::storage::mount_from_spec(
        "cifs",
        &body.server,
        &share,
        target,
        &body.username,
        XINJIANG_CIFS_PASSWORD,
        &body.options,
    );
    if let Err(error) = mount_result {
        let rollback = restore_nfs(target, &existing);
        return Err(AppError::internal(format!(
            "CIFS 挂载失败：{error}；原 NFS 恢复{}",
            if rollback.is_ok() { "成功" } else { "失败" }
        )));
    }
    if let Err(error) = verify_writable(target) {
        let _ = crate::storage::unmount_dir(target);
        let rollback = restore_nfs(target, &existing);
        return Err(AppError::internal(format!(
            "CIFS 已挂载但写入验证失败：{error}；原 NFS 恢复{}",
            if rollback.is_ok() { "成功" } else { "失败" }
        )));
    }
    let cifs_source = format!("//{}/{}", body.server.trim(), share);
    let schedule = match install_cifs_health_check(target, &cifs_source) {
        Ok(message) => message,
        Err(error) => {
            let _ = crate::storage::unmount_dir(target);
            let rollback = restore_nfs(target, &existing);
            return Err(AppError::internal(format!(
                "CIFS 已挂载但定时检查任务更新失败：{error}；原 NFS 恢复{}",
                if rollback.is_ok() { "成功" } else { "失败" }
            )));
        }
    };
    let restarted = restart_affected_containers(target)?;
    Ok(format!(
        "原 NFS {}（导出目录已识别）→ {}，写入验证通过；{}；重启 {restarted} 个关联容器",
        existing.source, cifs_source, schedule
    ))
}

fn derive_share_name(source: &str) -> Result<String, AppError> {
    let export = source
        .split_once(':')
        .map(|(_, export)| export)
        .unwrap_or(source)
        .trim_end_matches('/');
    let share = export.rsplit('/').next().unwrap_or_default();
    if share.is_empty()
        || share == "."
        || share == ".."
        || !share
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(AppError::bad(format!(
            "无法从原 NFS 导出目录 {export} 推导 CIFS 共享名称，请手工填写"
        )));
    }
    Ok(share.to_string())
}

fn install_cifs_health_check(target: &str, expected_source: &str) -> Result<String, AppError> {
    use std::os::unix::fs::PermissionsExt;

    let suffix = target.trim_matches('/').replace('/', "-");
    let suffix = if suffix.is_empty() { "mount" } else { &suffix };
    let script = PathBuf::from(format!("/usr/local/sbin/cangling-cifs-{suffix}-health.sh"));
    let cron = PathBuf::from(format!("/etc/cron.d/cangling-cifs-{suffix}-health"));
    let script_body = format!(
        "#!/bin/sh\nset -eu\nTARGET='{}'\nEXPECTED='{}'\nTYPE=$(findmnt -n -o FSTYPE --target \"$TARGET\" 2>/dev/null || true)\nSOURCE=$(findmnt -n -o SOURCE --target \"$TARGET\" 2>/dev/null || true)\nif [ \"$TYPE\" = cifs ] && [ \"$SOURCE\" = \"$EXPECTED\" ]; then exit 0; fi\nif mountpoint -q \"$TARGET\"; then umount \"$TARGET\"; fi\nmount \"$TARGET\"\n",
        target, expected_source
    );
    std::fs::write(&script, script_body)
        .map_err(|error| AppError::internal(format!("写入 CIFS 健康检查脚本失败：{error}")))?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o750))?;

    let cron_body = format!(
        "SHELL=/bin/sh\nPATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n*/30 * * * * root {} >/dev/null 2>&1\n",
        script.display()
    );
    std::fs::write(&cron, cron_body)
        .map_err(|error| AppError::internal(format!("写入 CIFS 定时检查任务失败：{error}")))?;
    std::fs::set_permissions(&cron, std::fs::Permissions::from_mode(0o644))?;

    let disabled = disable_legacy_nfs_cron(target, &cron)?;
    Ok(format!(
        "已安装每 30 分钟 CIFS 健康检查{}",
        if disabled == 0 {
            String::new()
        } else {
            format!("，并禁用 {disabled} 条旧 NFS 定时任务")
        }
    ))
}

fn disable_legacy_nfs_cron(target: &str, own_cron: &Path) -> Result<usize, AppError> {
    let mut candidates = vec![
        PathBuf::from("/etc/crontab"),
        PathBuf::from("/var/spool/cron/root"),
        PathBuf::from("/var/spool/cron/crontabs/root"),
    ];
    if let Ok(entries) = std::fs::read_dir("/etc/cron.d") {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if path.is_file()
                && path != own_cron
                && !name.ends_with(".bak")
                && !name.contains(".cangling-update-")
            {
                candidates.push(path);
            }
        }
    }

    let mut disabled = 0usize;
    for path in candidates {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut changed = false;
        let mut output = String::with_capacity(contents.len() + 128);
        for line in contents.lines() {
            let trimmed = line.trim_start();
            let lower = trimmed.to_ascii_lowercase();
            let legacy = !trimmed.starts_with('#')
                && (lower.contains(target)
                    || lower.contains("check_nfs")
                    || lower.contains("nfs_check")
                    || (lower.contains("mount") && lower.contains("nfs")));
            if legacy {
                output.push_str("# cangling-update disabled legacy NFS task: ");
                output.push_str(line);
                disabled += 1;
                changed = true;
            } else {
                output.push_str(line);
            }
            output.push('\n');
        }
        if !changed {
            continue;
        }
        let backup = PathBuf::from(format!("{}.cangling-update-nfs.bak", path.display()));
        if !backup.exists() {
            std::fs::copy(&path, &backup).map_err(|error| {
                AppError::internal(format!(
                    "备份旧 NFS 定时任务 {} 失败：{error}",
                    path.display()
                ))
            })?;
        }
        let metadata = std::fs::metadata(&path)?;
        std::fs::write(&path, output).map_err(|error| {
            AppError::internal(format!(
                "修改旧 NFS 定时任务 {} 失败：{error}",
                path.display()
            ))
        })?;
        std::fs::set_permissions(&path, metadata.permissions())?;
    }
    Ok(disabled)
}

fn find_existing_mount(target: &str) -> Option<ExistingMount> {
    for path in ["/proc/mounts", "/etc/fstab"] {
        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() >= 4 && unescape_mount(fields[1]) == target {
                return Some(ExistingMount {
                    source: unescape_mount(fields[0]),
                    fstype: fields[2].to_string(),
                    options: fields[3].to_string(),
                });
            }
        }
    }
    None
}

fn unescape_mount(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn restore_nfs(target: &str, previous: &ExistingMount) -> Result<String, AppError> {
    let (server, share) = previous
        .source
        .split_once(':')
        .ok_or_else(|| AppError::internal("无法解析原 NFS 地址"))?;
    crate::storage::mount_from_spec("nfs", server, share, target, "", "", &previous.options)
}

fn verify_writable(target: &str) -> Result<(), AppError> {
    let probe = Path::new(target).join(format!(".cangling-update-write-test-{}", Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)?;
        file.write_all(b"ok")?;
        file.sync_all()?;
        std::fs::remove_file(&probe)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&probe);
    }
    result.map_err(|error| AppError::internal(format!("挂载点不可写：{error}")))
}

fn restart_affected_containers(target: &str) -> Result<usize, AppError> {
    let output = Command::new("docker")
        .args(["ps", "-aq"])
        .output()
        .map_err(|error| AppError::internal(format!("读取容器列表失败：{error}")))?;
    if !output.status.success() {
        return Err(AppError::internal("读取容器列表失败"));
    }
    let mut affected = Vec::new();
    for id in String::from_utf8_lossy(&output.stdout).lines() {
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        let inspect = Command::new("docker")
            .args([
                "inspect",
                "--format",
                "{{range .Mounts}}{{println .Source}}{{end}}",
                id,
            ])
            .output()
            .map_err(|error| AppError::internal(format!("检查容器 {id} 失败：{error}")))?;
        let prefix = format!("{}/", target.trim_end_matches('/'));
        if inspect.status.success()
            && String::from_utf8_lossy(&inspect.stdout)
                .lines()
                .any(|source| source == target || source.starts_with(&prefix))
        {
            affected.push(id.to_string());
        }
    }
    if !affected.is_empty() {
        let status = Command::new("docker")
            .arg("restart")
            .args(&affected)
            .status()
            .map_err(|error| AppError::internal(format!("重启关联容器失败：{error}")))?;
        if !status.success() {
            return Err(AppError::internal("重启关联容器失败"));
        }
    }
    Ok(affected.len())
}

fn output_tail(bytes: &[u8], fallback: &str) -> String {
    const MAX: usize = 4000;
    let start = bytes.len().saturating_sub(MAX);
    let value = String::from_utf8_lossy(&bytes[start..]).trim().to_string();
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_expected_jar_names_are_allowed() {
        for name in REQUIRED_JARS {
            assert_eq!(checked_name(name).unwrap(), name);
        }
        assert!(checked_name("other.jar").is_err());
        assert!(checked_name("../cis-map-1.0.0.jar").is_err());
    }

    #[test]
    fn output_is_bounded() {
        let bytes = vec![b'x'; 5000];
        assert_eq!(output_tail(&bytes, "empty").len(), 4000);
        assert_eq!(output_tail(b"", "empty"), "empty");
    }

    #[test]
    fn validates_storage_repair_request() {
        let valid = RepairStorageRequest {
            server: "10.166.48.1".into(),
            share: "yw".into(),
            target_dir: "/nfs1".into(),
            username: "nyc".into(),
            options: String::new(),
            check_only: false,
        };
        assert!(validate_repair_request(&valid).is_ok());
        let mut invalid = valid.clone();
        invalid.target_dir = "/".into();
        assert!(validate_repair_request(&invalid).is_err());
        invalid = valid.clone();
        invalid.share = "../nfs1".into();
        assert!(validate_repair_request(&invalid).is_err());
    }

    #[test]
    fn decodes_mount_paths() {
        assert_eq!(unescape_mount("/mnt/shared\\040data"), "/mnt/shared data");
    }

    #[test]
    fn derives_cifs_share_from_nfs_export() {
        assert_eq!(derive_share_name("10.166.48.1:/nfs1").unwrap(), "nfs1");
        assert_eq!(
            derive_share_name("10.166.48.1:/exports/cangling").unwrap(),
            "cangling"
        );
        assert!(derive_share_name("10.166.48.1:/").is_err());
    }

    #[test]
    fn generated_script_uses_discovered_container_and_mount() {
        let hosts = vec![InspectedHost {
            status: JarNodeStatus {
                name: "master".into(),
                address: "本机".into(),
                ok: true,
                message: String::new(),
                targets: vec![JarTarget {
                    jar: "cis-server-1.0.0.jar".into(),
                    container: "cangling-core-cis-server-1".into(),
                    path: "/opt/np4/jars/cis-server-1.0.0.jar".into(),
                }],
            },
            kind: "local".into(),
            host: String::new(),
            port: 22,
            username: "root".into(),
        }];
        let script =
            generate_upgrade_script(Path::new("/opt/cangling/jar-deploy"), &hosts).expect("script");
        assert!(script.contains("cangling-core-cis-server-1"));
        assert!(script.contains("/opt/np4/jars/cis-server-1.0.0.jar"));
        assert!(script.contains("JAR 升级完成"));
    }
}

#[cfg(test)]
mod partial_upgrade_tests {
    use super::*;

    fn node(ok: bool, jar: &str, path: &Path) -> InspectedHost {
        InspectedHost {
            status: JarNodeStatus {
                name: if ok {
                    "business"
                } else {
                    "GPU 'not installed'"
                }
                .into(),
                address: "test".into(),
                ok,
                message: if ok {
                    "ready"
                } else {
                    "SSH unavailable; updater not installed"
                }
                .into(),
                targets: vec![JarTarget {
                    jar: jar.into(),
                    container: if ok { "app" } else { "failed-app" }.into(),
                    path: path.display().to_string(),
                }],
            },
            kind: "local".into(),
            host: String::new(),
            port: 22,
            username: "root".into(),
        }
    }
    fn root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("cangling-jar-partial-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("incoming")).unwrap();
        root
    }
    fn report(root: &Path, nodes: &[InspectedHost]) {
        let report = JarEnvironmentReport {
            checked_at: "test".into(),
            nodes: nodes.iter().map(|n| n.status.clone()).collect(),
        };
        std::fs::write(
            root.join("environment.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("upgrade-jars.sh"),
            generate_upgrade_script(root, nodes).unwrap(),
        )
        .unwrap();
    }
    #[tokio::test]
    async fn unavailable_gpu_does_not_block_a_matching_business_target() {
        let root = root();
        let nodes = vec![
            node(true, "cis-server-1.0.0.jar", &root.join("app.jar")),
            node(false, "cis-server-1.0.0.jar", &root.join("failed.jar")),
        ];
        report(&root, &nodes);
        std::fs::write(root.join("incoming/cis-server-1.0.0.jar"), b"uploaded").unwrap();
        let status = load_status_at(&root).await.unwrap();
        assert!(status.ready);
        assert_eq!(status.eligible_targets, 1);
        assert_eq!(status.skipped_nodes, 1);
        assert!(!status.nodes[1].ok);
        assert!(status.nodes[1].message.contains("SSH unavailable"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn all_failed_unmatched_upload_or_missing_script_still_blocks_execution() {
        let root = root();
        std::fs::write(root.join("incoming/cis-map-1.0.0.jar"), b"uploaded").unwrap();
        report(
            &root,
            &[node(false, "cis-map-1.0.0.jar", &root.join("bad.jar"))],
        );
        assert!(!load_status_at(&root).await.unwrap().ready);
        report(
            &root,
            &[node(true, "cis-server-1.0.0.jar", &root.join("app.jar"))],
        );
        assert!(!load_status_at(&root).await.unwrap().ready);
        std::fs::write(root.join("incoming/cis-server-1.0.0.jar"), b"uploaded").unwrap();
        assert!(load_status_at(&root).await.unwrap().ready);
        std::fs::remove_file(root.join("upgrade-jars.sh")).unwrap();
        assert!(!load_status_at(&root).await.unwrap().ready);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn generated_script_upgrades_only_passed_nodes_and_reports_exclusions() {
        let root = root();
        let app = root.join("app.jar");
        let failed = root.join("failed.jar");
        std::fs::write(&app, b"old").unwrap();
        std::fs::write(&failed, b"do not touch").unwrap();
        std::fs::write(root.join("incoming/cis-server-1.0.0.jar"), b"new").unwrap();
        let nodes = vec![
            node(true, "cis-server-1.0.0.jar", &app),
            node(false, "cis-server-1.0.0.jar", &failed),
        ];
        let script = generate_upgrade_script(&root, &nodes).unwrap();
        let path = root.join("upgrade.sh");
        std::fs::write(&path, &script).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let docker = bin.join("docker");
        std::fs::write(
            &docker,
            "#!/bin/sh\ncase \"$1\" in restart) exit 0;; inspect) echo true;; *) exit 99;; esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new("/bin/sh")
            .arg(&path)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(&app).unwrap(), b"new");
        assert_eq!(std::fs::read(&failed).unwrap(), b"do not touch");
        assert!(String::from_utf8_lossy(&output.stderr).contains("GPU 'not installed'"));
        assert!(String::from_utf8_lossy(&output.stdout).contains("已跳过 1 个检查失败节点"));
        assert!(!script.contains("docker restart 'failed-app'"));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod upgrade_progress_tests {
    use super::*;
    #[test]
    fn target_progress_uses_actual_completed_count_and_ignores_other_output() {
        let jobs = crate::progress::JobHub::default();
        let job = jobs.create();
        let mut completed = 0;
        assert!(!record_upgrade_progress(
            "ordinary rollout output",
            &jobs,
            &job.id,
            &mut completed,
            2
        ));
        assert!(record_upgrade_progress(
            "CK_JAR_PROGRESS|start|node/app",
            &jobs,
            &job.id,
            &mut completed,
            2
        ));
        assert_eq!(jobs.get(&job.id).unwrap().percent, 0);
        assert!(record_upgrade_progress(
            "CK_JAR_PROGRESS|done|node/app",
            &jobs,
            &job.id,
            &mut completed,
            2
        ));
        assert_eq!(jobs.get(&job.id).unwrap().percent, 50);
        assert_eq!(jobs.get(&job.id).unwrap().current, 1);
    }
    #[tokio::test]
    async fn non_utf8_output_does_not_turn_a_completed_upgrade_into_a_read_failure() {
        let bytes = b"legacy-output-\xff\n";
        assert_eq!(
            capture_upgrade_output(&bytes[..], |_| false).await.unwrap(),
            bytes
        );
    }
    #[tokio::test]
    async fn output_progress_is_visible_before_exit_and_failure_stderr_is_retained() {
        let path = std::env::temp_dir().join(format!("jar-progress-{}", Uuid::new_v4()));
        std::fs::write(&path,"#!/bin/sh\nprintf 'CK_JAR_PROGRESS|start|node/app\\n'\nprintf 'CK_JAR_PROGRESS|done|node/app\\n'\nsleep 0.3\necho deployment-failed >&2\nexit 7\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let jobs = crate::progress::JobHub::default();
        let job = jobs.create();
        let worker_jobs = jobs.clone();
        let id = job.id.clone();
        let script = path.clone();
        let task =
            tokio::spawn(
                async move { execute_upgrade_script(&script, &worker_jobs, &id, 2).await },
            );
        for _ in 0..100 {
            if jobs.get(&job.id).unwrap().current == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(jobs.get(&job.id).unwrap().percent, 50);
        assert!(!task.is_finished());
        let (status, out, err) = task.await.unwrap().unwrap();
        assert_eq!(status.code(), Some(7));
        assert!(out.is_empty());
        assert!(String::from_utf8_lossy(&err).contains("deployment-failed"));
        std::fs::remove_file(path).unwrap();
    }
}
