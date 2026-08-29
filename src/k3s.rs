//! `cangling-update fix-k3s` — Traefik 端口覆盖，以及 kubectl/k9s 默认 kubeconfig。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const K3S_BINS: &[&str] = &["k3s", "/usr/local/bin/k3s", "/usr/bin/k3s", "/opt/bin/k3s"];
const K3S_DATA_DIR: &str = "/var/lib/rancher/k3s";
const K3S_UNIT_PATHS: &[&str] = &[
    "/etc/systemd/system/k3s.service",
    "/usr/lib/systemd/system/k3s.service",
    "/lib/systemd/system/k3s.service",
];
pub const MANIFESTS_DIR: &str = "/var/lib/rancher/k3s/server/manifests";
pub const TRAEFIK_CONFIG_NAME: &str = "traefik-config.yaml";
pub const K3S_KUBECONFIG: &str = "/etc/rancher/k3s/k3s.yaml";
pub const ROOT_KUBECONFIG: &str = "/root/.kube/config";

pub const TRAEFIK_CONFIG_YAML: &str = "\
apiVersion: helm.cattle.io/v1
kind: HelmChartConfig
metadata:
  name: traefik
  namespace: kube-system
spec:
  valuesContent: |-
    ports:
      web:
        exposedPort: 8020
      websecure:
        exposedPort: 8443
";

struct K3sInstall {
    binary: Option<PathBuf>,
    version: Option<String>,
}

pub fn fix() -> Result<()> {
    match detect() {
        Some(info) => apply(&info),
        None => {
            print_not_installed_tip();
            Ok(())
        }
    }
}

/// 供集群初始化调用：确保 Traefik 端口覆盖 manifest 已写入（best-effort）。
pub fn ensure_traefik_config() -> Result<String> {
    let manifests = PathBuf::from(MANIFESTS_DIR);
    if !manifests.is_dir() {
        std::fs::create_dir_all(&manifests)
            .with_context(|| format!("创建 {}，需要 root 权限", manifests.display()))?;
    }
    let dest = manifests.join(TRAEFIK_CONFIG_NAME);
    std::fs::write(&dest, TRAEFIK_CONFIG_YAML)
        .with_context(|| format!("写入 {}", dest.display()))?;
    Ok(format!(
        "已写入 Traefik 端口配置（HTTP 8020 / HTTPS 8443）：{}",
        dest.display()
    ))
}

/// 检查 kubectl/k9s 默认 kubeconfig（`/root/.kube/config`），缺失或与 k3s.yaml 不一致则拷贝。
/// 若 `HOME` 指向其它目录，同时写入 `$HOME/.kube/config`。
pub fn ensure_kubeconfig() -> Result<String> {
    let src = Path::new(K3S_KUBECONFIG);
    if !src.is_file() {
        bail!(
            "{} 不存在，无法写入 {}。请确认 k3s server 已启动。",
            src.display(),
            ROOT_KUBECONFIG
        );
    }
    let mut dests = vec![PathBuf::from(ROOT_KUBECONFIG)];
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            let extra = PathBuf::from(home).join(".kube").join("config");
            if extra != dests[0] {
                dests.push(extra);
            }
        }
    }
    let mut msgs = Vec::new();
    for dest in &dests {
        msgs.push(sync_kubeconfig(src, dest)?);
    }
    Ok(msgs.join("\n"))
}

fn sync_kubeconfig(src: &Path, dest: &Path) -> Result<String> {
    if dest.is_file() {
        let src_bytes = std::fs::read(src).with_context(|| format!("读取 {}", src.display()))?;
        let dest_bytes = std::fs::read(dest).with_context(|| format!("读取 {}", dest.display()))?;
        if src_bytes == dest_bytes {
            return Ok(format!("kubeconfig 已存在：{}", dest.display()));
        }
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建 {}，需要 root 权限", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    std::fs::copy(src, dest).with_context(|| {
        format!(
            "复制 {} → {}，需要 root 权限",
            src.display(),
            dest.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("设置权限 {}", dest.display()))?;
    }
    Ok(format!(
        "已将 k3s kubeconfig 复制到 {}（kubectl/k9s 默认路径）",
        dest.display()
    ))
}

fn detect() -> Option<K3sInstall> {
    let binary = find_k3s_binary();
    let data_dir = Path::new(K3S_DATA_DIR).is_dir();
    let unit = k3s_unit_exists();
    if binary.is_none() && !data_dir && !unit {
        return None;
    }
    let version = binary.as_ref().and_then(|p| k3s_version(p));
    Some(K3sInstall { binary, version })
}

fn apply(info: &K3sInstall) -> Result<()> {
    eprintln!("已检测到 k3s");
    match &info.binary {
        Some(p) => eprintln!("  程序      {}", p.display()),
        None => eprintln!(
            "  程序      （未在 PATH 中找到 k3s 可执行文件，但发现了数据目录或 systemd 单元）"
        ),
    }
    match &info.version {
        Some(v) => eprintln!("  版本      {v}"),
        None => eprintln!("  版本      未知"),
    }
    eprintln!("  manifests {MANIFESTS_DIR}");

    require_root()?;

    let manifests = PathBuf::from(MANIFESTS_DIR);
    if !manifests.is_dir() {
        eprintln!("manifests 目录不存在，正在创建 {}", manifests.display());
        std::fs::create_dir_all(&manifests)
            .with_context(|| format!("创建 {}，需要 root 权限", manifests.display()))?;
    }

    let dest = manifests.join(TRAEFIK_CONFIG_NAME);
    let existed = dest.is_file();
    std::fs::write(&dest, TRAEFIK_CONFIG_YAML)
        .with_context(|| format!("写入 {}", dest.display()))?;

    if existed {
        eprintln!("已更新 Traefik 端口配置：{}", dest.display());
    } else {
        eprintln!("已写入 Traefik 端口配置：{}", dest.display());
    }
    eprintln!("已将默认入口端口改为：");
    eprintln!("  HTTP  (web)       80 -> 8020");
    eprintln!("  HTTPS (websecure) 443 -> 8443");

    restart_traefik(info, &dest)?;

    match ensure_kubeconfig() {
        Ok(msg) => {
            for line in msg.lines() {
                eprintln!("{line}");
            }
        }
        Err(e) => eprintln!("kubeconfig：{e:#}"),
    }
    Ok(())
}

fn restart_traefik(info: &K3sInstall, config_path: &Path) -> Result<()> {
    let kubectl = resolve_kubectl(info).ok_or_else(|| {
        anyhow::anyhow!(
            "配置已写入 {}，但找不到 k3s/kubectl，无法重启 Traefik。请手动执行：\n  k3s kubectl apply -f {}\n  k3s kubectl -n kube-system rollout restart deploy/traefik",
            config_path.display(),
            config_path.display()
        )
    })?;

    eprintln!("正在立即应用 HelmChartConfig …");
    let config = config_path.display().to_string();
    kubectl_run(&kubectl, &["apply", "-f", &config], true)?;

    eprintln!("正在重启 Traefik …");
    if kubectl_ok(&kubectl, &["-n", "kube-system", "get", "deploy/traefik"]) {
        kubectl_run(
            &kubectl,
            &["-n", "kube-system", "rollout", "restart", "deploy/traefik"],
            true,
        )?;
        eprintln!("等待 Traefik Deployment 滚动完成 …");
        kubectl_run(
            &kubectl,
            &[
                "-n",
                "kube-system",
                "rollout",
                "status",
                "deploy/traefik",
                "--timeout=90s",
            ],
            true,
        )?;
        eprintln!("Traefik Deployment 已重启。");
        return Ok(());
    }

    if kubectl_ok(&kubectl, &["-n", "kube-system", "get", "ds/traefik"]) {
        kubectl_run(
            &kubectl,
            &["-n", "kube-system", "rollout", "restart", "ds/traefik"],
            true,
        )?;
        eprintln!("等待 Traefik DaemonSet 滚动完成 …");
        kubectl_run(
            &kubectl,
            &[
                "-n",
                "kube-system",
                "rollout",
                "status",
                "ds/traefik",
                "--timeout=90s",
            ],
            true,
        )?;
        eprintln!("Traefik DaemonSet 已重启。");
        return Ok(());
    }

    eprintln!("未找到 traefik Deployment/DaemonSet，改为按标签重建 Pod。");
    kubectl_run(
        &kubectl,
        &[
            "-n",
            "kube-system",
            "delete",
            "pod",
            "-l",
            "app.kubernetes.io/name=traefik",
            "--ignore-not-found=true",
        ],
        true,
    )?;
    eprintln!("若 Traefik 尚未由 Helm 安装完成，k3s 会在稍后自动应用配置并拉起新 Pod。");
    Ok(())
}

enum Kubectl {
    K3s(PathBuf),
    Kubectl,
}

fn resolve_kubectl(info: &K3sInstall) -> Option<Kubectl> {
    if let Some(bin) = &info.binary {
        return Some(Kubectl::K3s(bin.clone()));
    }
    if look_path("kubectl").is_some() {
        return Some(Kubectl::Kubectl);
    }
    None
}

fn kubectl_command(kubectl: &Kubectl) -> Command {
    match kubectl {
        Kubectl::K3s(bin) => {
            let mut cmd = Command::new(bin);
            cmd.arg("kubectl");
            cmd
        }
        Kubectl::Kubectl => {
            let mut cmd = Command::new("kubectl");
            if Path::new("/etc/rancher/k3s/k3s.yaml").is_file() {
                cmd.env("KUBECONFIG", "/etc/rancher/k3s/k3s.yaml");
            }
            cmd
        }
    }
}

fn kubectl_prefix(kubectl: &Kubectl) -> String {
    match kubectl {
        Kubectl::K3s(bin) => format!("{} kubectl", bin.display()),
        Kubectl::Kubectl => "kubectl".into(),
    }
}

fn kubectl_ok(kubectl: &Kubectl, args: &[&str]) -> bool {
    kubectl_run(kubectl, args, false).unwrap_or(false)
}

fn kubectl_run(kubectl: &Kubectl, args: &[&str], print: bool) -> Result<bool> {
    if print {
        eprintln!("  $ {} {}", kubectl_prefix(kubectl), args.join(" "));
    }
    let output = kubectl_command(kubectl)
        .args(args)
        .output()
        .with_context(|| format!("执行 {} {}", kubectl_prefix(kubectl), args.join(" ")))?;
    if print {
        print_cmd_output(&output.stdout, &output.stderr);
    }
    if !output.status.success() && print {
        bail!(
            "{} {} 失败，退出码 {:?}",
            kubectl_prefix(kubectl),
            args.join(" "),
            output.status.code()
        );
    }
    Ok(output.status.success())
}

fn print_cmd_output(stdout: &[u8], stderr: &[u8]) {
    for chunk in [stdout, stderr] {
        let text = String::from_utf8_lossy(chunk);
        for line in text.lines() {
            if !line.trim().is_empty() {
                eprintln!("  {line}");
            }
        }
    }
}

fn print_not_installed_tip() {
    let exe = exe_name();
    eprintln!("未检测到 k3s，已跳过 Traefik 端口调整。");
    eprintln!("提示：请先在本机安装 k3s（服务端），例如：");
    eprintln!("  curl -sfL https://get.k3s.io | sh -");
    eprintln!("安装完成后再执行：");
    eprintln!("  sudo {exe} fix-k3s");
}

fn find_k3s_binary() -> Option<PathBuf> {
    for name in K3S_BINS {
        if let Some(p) = look_path(name) {
            return Some(p);
        }
    }
    None
}

fn look_path(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

fn k3s_unit_exists() -> bool {
    K3S_UNIT_PATHS.iter().any(|p| Path::new(p).is_file())
}

fn k3s_version(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    let mut s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        s = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    if s.is_empty() {
        return None;
    }
    Some(
        s.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or(&s)
            .to_string(),
    )
}

fn require_root() -> Result<()> {
    if !running_as_root() {
        bail!("请使用 root 执行此命令，例如：sudo {} fix-k3s", exe_name());
    }
    Ok(())
}

fn running_as_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

fn exe_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "cangling-update".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traefik_yaml_sets_custom_ports() {
        let v: serde_yaml::Value = serde_yaml::from_str(TRAEFIK_CONFIG_YAML).unwrap();
        assert_eq!(v["apiVersion"], "helm.cattle.io/v1");
        assert_eq!(v["kind"], "HelmChartConfig");
        assert_eq!(v["metadata"]["name"], "traefik");
        assert_eq!(v["metadata"]["namespace"], "kube-system");
        let values = v["spec"]["valuesContent"].as_str().unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(values).unwrap();
        assert_eq!(parsed["ports"]["web"]["exposedPort"], 8020);
        assert_eq!(parsed["ports"]["websecure"]["exposedPort"], 8443);
    }

    #[test]
    fn traefik_yaml_has_expected_filename() {
        assert_eq!(TRAEFIK_CONFIG_NAME, "traefik-config.yaml");
        assert!(MANIFESTS_DIR.ends_with("/server/manifests"));
    }

    #[test]
    fn kubeconfig_paths_are_k3s_defaults() {
        assert_eq!(K3S_KUBECONFIG, "/etc/rancher/k3s/k3s.yaml");
        assert_eq!(ROOT_KUBECONFIG, "/root/.kube/config");
    }

    #[test]
    fn sync_kubeconfig_copies_when_missing() {
        let tmp = std::env::temp_dir().join(format!("cangling-kube-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let src = tmp.join("k3s.yaml");
        let dest = tmp.join(".kube").join("config");
        std::fs::write(&src, "apiVersion: v1\nkind: Config\n").unwrap();

        let msg = sync_kubeconfig(&src, &dest).unwrap();
        assert!(msg.contains("复制"), "{msg}");
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "apiVersion: v1\nkind: Config\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(dest.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        let again = sync_kubeconfig(&src, &dest).unwrap();
        assert!(again.contains("已存在"), "{again}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sync_kubeconfig_updates_when_stale() {
        let tmp =
            std::env::temp_dir().join(format!("cangling-kube-stale-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(tmp.join(".kube")).unwrap();
        let src = tmp.join("k3s.yaml");
        let dest = tmp.join(".kube").join("config");
        std::fs::write(&src, "current\n").unwrap();
        std::fs::write(&dest, "stale\n").unwrap();

        let msg = sync_kubeconfig(&src, &dest).unwrap();
        assert!(msg.contains("复制"), "{msg}");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "current\n");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
