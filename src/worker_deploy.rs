//! Deploy only the cangling-update agent over managed SSH connections.
use crate::{
    binaries::{self, Arch},
    cluster::Role,
    error::AppError,
    hosts::WorkerSshHost,
    state::AppState,
};
use axum::{extract::State, routing::get, Json, Router};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/hosts/deploy-workers", get(options).post(start))
}
fn require_master(s: &AppState) -> Result<(), AppError> {
    if s.cluster.role != Role::Master || s.cluster.token.as_deref().unwrap_or("").is_empty() {
        return Err(AppError::bad("请在 master 主节点上部署工作节点"));
    }
    Ok(())
}
async fn options(State(s): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    require_master(&s)?;
    let ip = crate::hostinfo::primary_ip();
    let host = if ip.contains(':') {
        format!("[{ip}]")
    } else {
        ip
    };
    Ok(Json(
        json!({"master_url":format!("http://{host}:{}",s.port),"worker_port":5400,"version":env!("CARGO_PKG_VERSION")}),
    ))
}
#[derive(Deserialize)]
struct Request {
    host_ids: Vec<String>,
    master_url: String,
}
fn master_url(raw: &str) -> Result<String, AppError> {
    let url = raw.trim().trim_end_matches('/');
    let uri = url
        .parse::<axum::http::Uri>()
        .map_err(|_| AppError::bad("主节点地址无效"))?;
    let invalid_host = uri.host().is_some_and(|h| {
        h.eq_ignore_ascii_case("localhost")
            || h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
    });
    if invalid_host
        || uri.scheme_str() != Some("http")
        || uri.host().is_none()
        || uri.authority().is_some_and(|v| v.as_str().contains('@'))
        || uri.query().is_some()
        || url.contains('#')
        || uri
            .host()
            .is_some_and(|v| matches!(v, "localhost" | "127.0.0.1" | "[::1]" | "0.0.0.0"))
    {
        return Err(AppError::bad(
            "请填写工作节点可访问的主节点 HTTP 地址，例如 http://192.168.3.122:5400",
        ));
    }
    Ok(url.into())
}
async fn start(
    State(s): State<AppState>,
    Json(body): Json<Request>,
) -> Result<Json<crate::progress::JobProgress>, AppError> {
    require_master(&s)?;
    if body.host_ids.is_empty() || body.host_ids.len() > 64 {
        return Err(AppError::bad("请选择 1 至 64 台工作节点"));
    }
    if s.init
        .lock()
        .map_err(|_| AppError::internal("init lock"))?
        .running
    {
        return Err(AppError::conflict(
            "集群初始化正在执行，请完成后再部署工作节点",
        ));
    }
    let master = master_url(&body.master_url)?;
    let hosts = crate::hosts::worker_deploy_hosts(&s, &body.host_ids)?;
    let guard = s
        .lock_project("worker-deploy")
        .try_lock_owned()
        .map_err(|_| AppError::conflict("已有工作节点部署任务执行中"))?;
    let job = s.jobs.create();
    let id = job.id.clone();
    tokio::spawn(async move {
        let _guard = guard;
        let total = hosts.len() as u64;
        let mut results = Vec::new();
        let mut failed = 0;
        for (index, host) in hosts.into_iter().enumerate() {
            let previous = results.join("\n");
            let mut progress = |stage: &str| {
                s.jobs.set(
                    &id,
                    "worker-deploy",
                    &format!("{previous}\n{}（{}）：{stage}", host.name, host.host).trim(),
                    index as u64,
                    total,
                )
            };
            match deploy(&s, &host, &master, &id, &mut progress).await {
                Ok(version) => results.push(format!(
                    "✓ {}（{}）：已部署 v{version}，已注册到主节点",
                    host.name, host.host
                )),
                Err(error) => {
                    failed += 1;
                    results.push(format!(
                        "✗ {}（{}）：{}",
                        host.name,
                        host.host,
                        sanitize(&error.to_string(), &s, &host)
                    ));
                }
            }
            s.jobs.set(
                &id,
                "worker-deploy",
                &results.join("\n"),
                index as u64 + 1,
                total,
            );
        }
        let message = format!(
            "部署完成：成功 {}，失败 {failed}\n{}",
            total - failed,
            results.join("\n")
        );
        if failed == 0 {
            s.jobs.finish_ok(&id, &message);
        } else {
            s.jobs.finish_err(&id, &message);
        }
    });
    Ok(Json(job))
}
fn sanitize(text: &str, s: &AppState, host: &WorkerSshHost) -> String {
    let mut output = text.to_owned();
    for secret in [s.cluster.token.as_deref(), host.password.as_deref()]
        .into_iter()
        .flatten()
        .filter(|v| !v.is_empty())
    {
        output = output.replace(secret, "[已隐藏]");
    }
    output.chars().take(6000).collect()
}
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
struct Askpass(Option<PathBuf>);
impl Drop for Askpass {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}
fn ssh(s: &AppState, h: &WorkerSshHost, remote: &str) -> Result<(Command, Askpass), AppError> {
    let mut c = Command::new("ssh");
    c.args([
        "-p",
        &h.port.to_string(),
        "-o",
        "ConnectTimeout=12",
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=2",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        &format!(
            "UserKnownHostsFile={}",
            s.paths.config_dir.join("ssh-known-hosts").display()
        ),
        "-o",
        "NumberOfPasswordPrompts=1",
    ]);
    if h.password.is_none() {
        c.args(["-o", "BatchMode=yes"]);
    }
    let ask =
        crate::hosts::prepare_askpass(c.as_std_mut(), h.password.as_deref(), &s.paths.config_dir)?;
    c.arg(format!("{}@{}", h.username, h.host))
        .arg(remote)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok((c, Askpass(ask)))
}
async fn run(
    s: &AppState,
    h: &WorkerSshHost,
    script: &str,
    limit: u64,
) -> Result<String, AppError> {
    let (mut c, _ask) = ssh(s, h, "sh -s")?;
    let mut child = c
        .spawn()
        .map_err(|e| AppError::bad(format!("SSH 无法启动：{e}")))?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| AppError::internal("SSH stdin"))?;
    let work = async {
        let write = async move {
            input.write_all(script.as_bytes()).await?;
            input.shutdown().await?;
            drop(input);
            Ok::<(), std::io::Error>(())
        };
        let (write, output) = tokio::join!(write, child.wait_with_output());
        write?;
        output
    };
    let output = tokio::time::timeout(Duration::from_secs(limit), work)
        .await
        .map_err(|_| AppError::bad("SSH 操作超时，请检查节点连接及服务状态"))?
        .map_err(|e| AppError::bad(format!("SSH 执行失败：{e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.status.success() {
        return Err(AppError::bad(format!(
            "SSH 操作失败（退出码 {:?}）：{} {}",
            output.status.code(),
            stdout.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(stdout)
}
async fn upload(
    s: &AppState,
    h: &WorkerSshHost,
    file: &Path,
    remote: &str,
) -> Result<(), AppError> {
    let script = format!(
        "umask 077; mkdir -- {dir} && cat > {file}",
        dir = quote(remote),
        file = quote(&format!("{remote}/candidate"))
    );
    let (mut c, _ask) = ssh(s, h, &script)?;
    let mut child = c.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| AppError::internal("SSH stdin"))?;
    let mut file = tokio::fs::File::open(file).await?;
    let task = async {
        let send = async move {
            tokio::io::copy(&mut file, &mut stdin).await?;
            stdin.shutdown().await?;
            drop(stdin);
            Ok::<(), std::io::Error>(())
        };
        let (sent, output) = tokio::join!(send, child.wait_with_output());
        let output = output?;
        if output.status.success() {
            sent?;
        }
        Ok::<_, std::io::Error>(output)
    };
    let output = tokio::time::timeout(Duration::from_secs(300), task)
        .await
        .map_err(|_| AppError::bad("传输程序超时，请检查 SSH 网络"))??;
    if !output.status.success() {
        return Err(AppError::bad(format!(
            "传输失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}
fn binary(s: &AppState, arch: Arch) -> Result<PathBuf, AppError> {
    let repo = crate::repo::repo_root(&s.paths);
    let version = std::fs::read_to_string(repo.join("binaries/VERSION")).unwrap_or_default();
    if binaries::strip_v(&version) == env!("CARGO_PKG_VERSION") {
        let p = repo
            .join("binaries/linux")
            .join(match arch {
                Arch::Amd64 => "amd64",
                Arch::Arm64 => "arm64",
            })
            .join("cangling-update");
        if binaries::elf_arch_file(&p) == Some(arch) {
            return Ok(p);
        }
    }
    if Arch::host() == Some(arch) {
        return binaries::current_exe().map_err(AppError::from);
    }
    binaries::stored(&s.paths.exe_dir, arch)
        .and_then(|b| b.path)
        .map(PathBuf::from)
        .ok_or_else(|| {
            AppError::bad(format!(
                "缺少 {} 程序，请同步与主节点版本一致的 cangling-repo/binaries 或 updates/",
                arch.label()
            ))
        })
}
fn probe() -> &'static str {
    r#"set -eu
test "$(id -u)" = 0 || { echo '部署需要 root SSH 账号' >&2; exit 1; }
command -v systemctl >/dev/null || { echo '目标系统缺少 systemd' >&2; exit 1; }
test -s /etc/machine-id || { echo '目标节点缺少 machine-id' >&2; exit 1; }
printf 'CK_ARCH=%s\n' "$(uname -m)"
printf 'CK_MACHINE=%s\n' "$(cat /etc/machine-id)"
"#
}
fn value<'a>(output: &'a str, key: &str) -> Option<&'a str> {
    output
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .map(str::trim)
}
async fn deploy(
    s: &AppState,
    h: &WorkerSshHost,
    master: &str,
    id: &str,
    progress: &mut impl FnMut(&str),
) -> Result<String, AppError> {
    progress("检查 SSH、root 权限与 CPU 架构");
    let out = run(s, h, probe(), 30).await?;
    let machine =
        value(&out, "CK_MACHINE=").ok_or_else(|| AppError::bad("未能确认目标节点身份"))?;
    let own = std::fs::read_to_string("/etc/machine-id").unwrap_or_default();
    if machine.is_empty() || machine == own.trim() {
        return Err(AppError::bad("目标是本机或节点身份无效，已停止部署"));
    }
    let arch = Arch::parse(value(&out, "CK_ARCH=").unwrap_or(""))
        .ok_or_else(|| AppError::bad("目标 CPU 架构不受支持，仅支持 AMD64/ARM64"))?;
    let file = binary(s, arch)?;
    let checksum = format!("{:x}", Sha256::digest(tokio::fs::read(&file).await?));
    let remote = format!("/tmp/cangling-worker-{}", uuid::Uuid::new_v4());
    let result = async {
        progress(&format!("传输 {} 程序", arch.label()));
        upload(s, h, &file, &remote).await?;
        progress("校验程序、备份并安装 worker 服务");
        let script = install_script(
            &remote,
            master,
            s.cluster.token.as_deref().unwrap_or(""),
            id,
            env!("CARGO_PKG_VERSION"),
            &checksum,
        );
        let output = run(s, h, &script, 180).await?;
        let node = value(&output, "CK_NODE_ID=")
            .ok_or_else(|| AppError::bad("安装后未获得工作节点 ID"))?;
        uuid::Uuid::parse_str(node).map_err(|_| AppError::bad("工作节点 ID 无效"))?;
        progress("等待工作节点注册到当前主节点（最多 45 秒）");
        let started = chrono::Utc::now();
        for _ in 0..45 {
            let online = {
                let db = s.db.lock().map_err(|_| AppError::internal("db lock"))?;
                db.query_row(
                    "SELECT role,version,last_seen FROM cluster_nodes WHERE id=?1",
                    [node],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .ok()
                .is_some_and(|(role, v, last)| {
                    role == "worker"
                        && binaries::strip_v(&v) == env!("CARGO_PKG_VERSION")
                        && chrono::DateTime::parse_from_rfc3339(&last).is_ok_and(|t| {
                            t.with_timezone(&chrono::Utc) >= started
                                && (0..=20).contains(
                                    &chrono::Utc::now().signed_duration_since(t).num_seconds(),
                                )
                        })
                })
            };
            if online {
                return Ok(env!("CARGO_PKG_VERSION").to_string());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err(AppError::bad(format!(
            "服务已安装，但 45 秒内未注册；请检查目标节点能否访问 {master}、集群令牌及服务日志"
        )))
    }
    .await;
    let _ = run(s, h, &format!("rm -rf -- {}", quote(&remote)), 15).await;
    result
}
fn install_script(
    remote: &str,
    master: &str,
    token: &str,
    id: &str,
    version: &str,
    checksum: &str,
) -> String {
    format!(
        "stage={}\nmaster={}\ntoken={}\njob={}\nexpected={}\nchecksum={}\n",
        quote(remote),
        quote(master),
        quote(token),
        quote(id),
        quote(&format!("v{version}")),
        quote(checksum)
    ) + r#"set -eu
umask 077
app=/opt/cangling-update
unit=/etc/systemd/system/cangling-update.service
trap 'rm -rf -- "$stage"' EXIT
test "$(sha256sum "$stage/candidate" | cut -d ' ' -f 1)" = "$checksum" || { echo '传输后 SHA-256 校验失败' >&2; exit 1; }
chmod 700 "$stage/candidate"
actual=$("$stage/candidate" version)
test "$actual" = "$expected" || { echo "程序版本不匹配：$actual；主节点 $expected，请先准备同版本程序" >&2; exit 1; }
exec_line=$(systemctl show -p ExecStart --value cangling-update 2>/dev/null || true)
env_line=$(systemctl show -p Environment --value cangling-update 2>/dev/null || true)
if printf '%s %s' "$exec_line" "$env_line" | grep -Eq -- '--role[=[:space:]]+master|CANGLING_ROLE=master'; then echo '目标已有 master 服务，不能改为 worker' >&2; exit 1; fi
if [ -n "$exec_line" ]; then
 case "$exec_line" in *'/opt/cangling-update/cangling-update'*) ;; *) echo '目标服务使用自定义安装目录，请通过现有服务升级' >&2; exit 1;; esac
 case "$exec_line $env_line" in *'--data-dir'*|*'CANGLING_HOME'*) echo '已有服务使用自定义数据目录，请通过现有服务升级' >&2; exit 1;; esac
 if printf '%s %s' "$exec_line" "$env_line" | grep -Eq -- '--role[=[:space:]]+worker|CANGLING_ROLE=worker'; then
  case "$exec_line $env_line" in *"$token"*) ;; *) echo '目标 worker 属于其他集群，未覆盖其配置' >&2; exit 1;; esac
 fi
fi
mkdir -p "$app/config" "$app/backups"
backup="$app/backups/worker-deploy-$job"
mkdir -p "$backup"; chmod 700 "$backup"
was_active=0; systemctl is-active --quiet cangling-update && was_active=1 || true
was_enabled=0; systemctl is-enabled --quiet cangling-update && was_enabled=1 || true
[ ! -f "$app/cangling-update" ] || cp -p "$app/cangling-update" "$backup/cangling-update"
[ ! -f "$unit" ] || cp -p "$unit" "$backup/service"
changed=0
firewall_runtime_added=0
firewall_permanent_added=0
rollback() {
 status=$?
 if [ "$status" != 0 ] && [ "$changed" = 1 ]; then
  systemctl stop cangling-update >/dev/null 2>&1 || true
  if [ -f "$backup/cangling-update" ]; then cp -p "$backup/cangling-update" "$app/.rollback-$job"; mv -f "$app/.rollback-$job" "$app/cangling-update"; else rm -f "$app/cangling-update"; fi
  if [ -f "$backup/service" ]; then cp -p "$backup/service" "$unit"; else rm -f "$unit"; fi
  if [ -f "$backup/config/cangling.db" ]; then rm -f "$app/config/cangling.db-wal" "$app/config/cangling.db-shm"; fi
  if [ -d "$backup/config" ]; then cp -Rp "$backup/config/." "$app/config/"; fi
  if [ "$firewall_runtime_added" = 1 ]; then firewall-cmd --remove-port=5400/tcp >/dev/null 2>&1 || true; fi
  if [ "$firewall_permanent_added" = 1 ]; then firewall-cmd --permanent --remove-port=5400/tcp >/dev/null 2>&1 || true; fi
  systemctl daemon-reload >/dev/null 2>&1 || true
  if [ "$was_enabled" = 0 ]; then systemctl disable cangling-update >/dev/null 2>&1 || true; fi
  if [ "$was_active" = 1 ]; then systemctl start cangling-update >/dev/null 2>&1 || true; fi
  echo "安装失败，已尝试恢复原服务；备份位于 $backup" >&2
 fi
 rm -f "$app/.worker-$job"
 rm -rf -- "$stage"
 exit "$status"
}
trap rollback EXIT
install -m 0755 "$stage/candidate" "$app/.worker-$job"
changed=1
systemctl stop cangling-update >/dev/null 2>&1 || { [ "$was_active" = 0 ] || exit 1; }
mkdir -p "$backup/config"
find "$app/config" -maxdepth 1 -type f -exec cp -p {} "$backup/config/" \;
mv -f "$app/.worker-$job" "$app/cangling-update"
"$app/cangling-update" --bind 0.0.0.0 --port 5400 --role worker --master "$master" --cluster-token "$token" install-service >/dev/null
if command -v firewall-cmd >/dev/null 2>&1 && systemctl is-active --quiet firewalld; then
 if ! firewall-cmd --permanent --query-port=5400/tcp >/dev/null; then firewall-cmd --permanent --add-port=5400/tcp >/dev/null; firewall_permanent_added=1; fi
 if ! firewall-cmd --query-port=5400/tcp >/dev/null; then firewall-cmd --add-port=5400/tcp >/dev/null; firewall_runtime_added=1; fi
fi
systemctl restart cangling-update
systemctl is-active --quiet cangling-update
attempt=0
while [ ! -s "$app/config/node-id" ]; do
 attempt=$((attempt + 1))
 [ "$attempt" -lt 30 ] || { echo '服务启动后未生成节点 ID，请检查服务日志' >&2; exit 1; }
 systemctl is-active --quiet cangling-update
 sleep 1
done
printf 'CK_NODE_ID=%s\n' "$(cat "$app/config/node-id")"
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, os::unix::fs::PermissionsExt};
    #[test]
    fn validates_reachable_master_urls_and_preserves_custom_ports() {
        assert_eq!(
            master_url("http://10.0.0.140:4300/").unwrap(),
            "http://10.0.0.140:4300"
        );
        assert_eq!(
            master_url("http://[2001:db8::1]:5400/update/").unwrap(),
            "http://[2001:db8::1]:5400/update"
        );
        for url in [
            "https://10.0.0.1:5400",
            "http://localhost:5400",
            "http://127.1.2.3:5400",
            "http://[::1]:5400",
            "http://0.0.0.0:5400",
            "http://root:password@10.0.0.1",
            "http://10.0.0.1?token=x",
        ] {
            assert!(master_url(url).is_err(), "{url}");
        }
    }
    fn sandbox(fail: bool, existing: &str) -> (PathBuf, std::process::Output) {
        let dir =
            std::env::temp_dir().join(format!("cangling-worker-test-{}", uuid::Uuid::new_v4()));
        let app = dir.join("app");
        let stage = dir.join("stage");
        let bin = dir.join("bin");
        for p in [&app, &stage, &bin] {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::create_dir_all(app.join("config")).unwrap();
        std::fs::write(app.join("config/keep"), "original-config").unwrap();
        std::fs::write(app.join("cangling-update"), "original-binary").unwrap();
        std::fs::write(dir.join("service"), "original-service").unwrap();
        let candidate = r#"#!/bin/sh
if [ "$1" = version ]; then echo v1.2.3; exit 0; fi
if [ "$WORKER_TEST_FAIL" = 1 ]; then echo 'injected installation failure' >&2; exit 9; fi
mkdir -p "$WORKER_TEST_APP/config"
printf 'c90cac38-d694-4d27-8bce-b5f23784c7d7\n' > "$WORKER_TEST_APP/config/node-id"
"#;
        std::fs::write(stage.join("candidate"), candidate).unwrap();
        let checksum = format!("{:x}", Sha256::digest(candidate.as_bytes()));
        let systemctl = r#"#!/bin/sh
case "$1" in
 show) case "$3" in ExecStart) printf '%s\n' "$WORKER_TEST_EXEC";; esac; exit 0;;
 is-active) test -f "$WORKER_TEST_APP/active"; exit $?;;
 is-enabled) exit 1;;
 start|restart) touch "$WORKER_TEST_APP/active";;
 stop) rm -f "$WORKER_TEST_APP/active";;
esac
exit 0
"#;
        std::fs::write(bin.join("systemctl"), systemctl).unwrap();
        std::fs::set_permissions(
            bin.join("systemctl"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let script = install_script(
            stage.to_str().unwrap(),
            "http://10.0.0.1:4300",
            "test-cluster-token",
            "test-job",
            "1.2.3",
            &checksum,
        )
        .replace(
            "app=/opt/cangling-update",
            &format!("app={}", quote(app.to_str().unwrap())),
        )
        .replace(
            "unit=/etc/systemd/system/cangling-update.service",
            &format!("unit={}", quote(dir.join("service").to_str().unwrap())),
        );
        let mut c = std::process::Command::new("sh");
        c.arg("-s")
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("WORKER_TEST_APP", &app)
            .env("WORKER_TEST_FAIL", if fail { "1" } else { "0" })
            .env("WORKER_TEST_EXEC", existing)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        (dir, output)
    }
    #[test]
    fn installer_executes_from_private_upload_and_preserves_config() {
        let (dir, out) = sandbox(false, "");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("CK_NODE_ID="));
        assert_eq!(
            std::fs::read_to_string(dir.join("app/config/keep")).unwrap(),
            "original-config"
        );
        assert!(!dir.join("stage").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_install_restores_binary_unit_and_config() {
        let (dir, out) = sandbox(true, "");
        assert!(!out.status.success());
        assert_eq!(
            std::fs::read_to_string(dir.join("app/cangling-update")).unwrap(),
            "original-binary"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("service")).unwrap(),
            "original-service"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("app/config/keep")).unwrap(),
            "original-config"
        );
        assert!(!dir.join("stage").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn installer_rejects_existing_master_and_other_cluster_workers() {
        for exec in [
            "/opt/cangling-update/cangling-update --role master",
            "/opt/cangling-update/cangling-update --role worker --cluster-token other",
        ] {
            let (dir, out) = sandbox(false, exec);
            assert!(!out.status.success());
            assert_eq!(
                std::fs::read_to_string(dir.join("app/cangling-update")).unwrap(),
                "original-binary"
            );
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
