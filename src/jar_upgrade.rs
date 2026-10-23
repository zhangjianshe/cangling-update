use crate::error::AppError;
use crate::state::AppState;
use axum::extract::{Multipart, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;
use tokio::io::AsyncWriteExt;
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
    upgrading: bool,
    files: Vec<JarFileStatus>,
}

async fn load_status() -> Result<JarUpgradeStatus, AppError> {
    let root = deploy_dir();
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
    let ready = files.iter().any(|file| file.present)
        && tokio::fs::metadata(&script)
            .await
            .is_ok_and(|item| item.is_file());
    Ok(JarUpgradeStatus {
        incoming_dir: incoming.display().to_string(),
        script: script.display().to_string(),
        ready,
        upgrading: UPGRADING.load(Ordering::SeqCst),
        files,
    })
}

async fn status() -> Result<Json<JarUpgradeStatus>, AppError> {
    load_status().await.map(Json)
}

async fn upload(mut multipart: Multipart) -> Result<Json<JarUpgradeStatus>, AppError> {
    if UPGRADING.load(Ordering::SeqCst) {
        return Err(AppError::conflict("JAR 升级正在执行，暂时不能覆盖上传目录"));
    }
    let incoming = deploy_dir().join("incoming");
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
            "至少需要一个待升级 JAR，并且升级脚本必须存在",
        ));
    }
    if UPGRADING.swap(true, Ordering::SeqCst) {
        return Err(AppError::conflict("已有 JAR 升级任务正在执行"));
    }
    let job = state.jobs.create();
    let job_id = job.id.clone();
    let jobs = state.jobs.clone();
    let script = deploy_dir().join("upgrade-jars.sh");
    tokio::spawn(async move {
        jobs.set(
            &job_id,
            "upgrade",
            "正在同步 JAR 并滚动升级 19、18 节点…",
            0,
            0,
        );
        let result = tokio::process::Command::new(&script).output().await;
        UPGRADING.store(false, Ordering::SeqCst);
        match result {
            Ok(output) if output.status.success() => {
                let message = output_tail(&output.stdout, "JAR 升级完成");
                jobs.finish_ok(&job_id, &message);
            }
            Ok(output) => {
                let fallback = format!("升级脚本退出码：{}", output.status);
                let message = output_tail(&output.stderr, &output_tail(&output.stdout, &fallback));
                jobs.finish_err(&job_id, &message);
            }
            Err(error) => {
                jobs.finish_err(&job_id, &format!("无法执行 {}：{error}", script.display()))
            }
        }
    });
    Ok(Json(job))
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
}
