use crate::service;
use anyhow::{bail, Context, Result};
use ssh_key::{Algorithm, HashAlg, PublicKey};
use std::fs::{OpenOptions, Permissions};
use std::io::Write;
use std::path::Path;

const BUILTIN_PUBLIC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILtCuBFCcSdruaExNgQjePIfs6bBnNc0s801ofuThDQ+ satway@dev";
const TUNNEL_AUTHORIZED_KEYS_FILE: &str = "tunnel_authorized_keys";

pub fn init(config_dir: &Path) -> Result<()> {
    service::require_root()?;
    init_at(config_dir, &service::root_home_dir())
}

fn init_at(config_dir: &Path, root_home: &Path) -> Result<()> {
    let key = BUILTIN_PUBLIC_KEY
        .parse::<PublicKey>()
        .context("内置维护公钥格式无效")?;
    if key.algorithm() != Algorithm::Ed25519 {
        bail!("内置维护公钥必须使用 Ed25519");
    }

    std::fs::create_dir_all(config_dir)
        .with_context(|| format!("创建配置目录 {}", config_dir.display()))?;
    let tunnel_keys = config_dir.join(TUNNEL_AUTHORIZED_KEYS_FILE);
    let tunnel_added = append_authorized_key(&tunnel_keys, &key, 0o600)?;

    let ssh_dir = root_home.join(".ssh");
    reject_symlink(&ssh_dir)?;
    std::fs::create_dir_all(&ssh_dir)
        .with_context(|| format!("创建 SSH 目录 {}", ssh_dir.display()))?;
    set_mode(&ssh_dir, 0o700)?;
    let host_keys = ssh_dir.join("authorized_keys");
    let host_added = append_authorized_key(&host_keys, &key, 0o600)?;

    let fingerprint = key.fingerprint(HashAlg::Sha256);
    println!("维护公钥：{fingerprint}");
    print_result("cangling-update 隧道授权", &tunnel_keys, tunnel_added);
    print_result("主机 root SSH 授权", &host_keys, host_added);
    Ok(())
}

fn append_authorized_key(path: &Path, key: &PublicKey, mode: u32) -> Result<bool> {
    reject_symlink(path)?;
    let existing = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("读取 {}", path.display())),
    };
    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
    if existing
        .lines()
        .filter_map(public_key_from_authorized_line)
        .any(|existing| existing.fingerprint(HashAlg::Sha256).to_string() == fingerprint)
    {
        set_mode(path, mode)?;
        return Ok(false);
    }

    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("写入 {}", path.display()))?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "{}", key.to_openssh()?)?;
    file.sync_all()
        .with_context(|| format!("同步 {}", path.display()))?;
    set_mode(path, mode)?;
    Ok(true)
}

fn public_key_from_authorized_line(line: &str) -> Option<PublicKey> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    // authorized_keys may put restrictions before the key type. Find the
    // OpenSSH key portion so restricted entries are also deduplicated.
    let key_start = line.find("ssh-ed25519 ")?;
    let mut fields = line[key_start..].split_whitespace();
    let algorithm = fields.next()?;
    let encoded = fields.next()?;
    format!("{algorithm} {encoded}").parse().ok()
}

fn reject_symlink(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("拒绝写入符号链接：{}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("检查 {}", path.display())),
    }
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, Permissions::from_mode(mode))
            .with_context(|| format!("设置 {} 权限", path.display()))?;
    }
    Ok(())
}

fn print_result(label: &str, path: &Path, added: bool) {
    let action = if added { "已添加" } else { "已存在" };
    println!("{label}：{action}（{}）", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_root(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "cangling-update-{name}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn installs_key_once_in_both_authorized_lists() {
        let root = test_root("init-cert");
        let config = root.join("config");
        let home = root.join("root");

        init_at(&config, &home).unwrap();
        init_at(&config, &home).unwrap();

        for path in [
            config.join(TUNNEL_AUTHORIZED_KEYS_FILE),
            home.join(".ssh/authorized_keys"),
        ] {
            let content = std::fs::read_to_string(path).unwrap();
            assert_eq!(content.matches("AAAAC3NzaC1lZDI1NTE5").count(), 1);
            assert!(content.contains("satway@dev"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recognizes_restricted_authorized_key_entry() {
        let line = format!("restrict,pty {}", BUILTIN_PUBLIC_KEY);
        let parsed = public_key_from_authorized_line(&line).unwrap();
        let expected = BUILTIN_PUBLIC_KEY.parse::<PublicKey>().unwrap();
        assert_eq!(
            parsed.fingerprint(HashAlg::Sha256),
            expected.fingerprint(HashAlg::Sha256)
        );
    }
}
