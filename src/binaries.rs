//! 主节点为工作节点准备的 `cangling-update` 双架构二进制。
//!
//! 查找顺序：
//! 1. keeper 同步的 `repo/np4/np4-update/latest/`（任意子目录中的对应 ELF）
//! 2. 程序旁 `updates/` 槽位（本机 `update` / `--import` / 启动时写入）
//!
//! ```text
//! cangling-update
//! repo/np4/np4-update/latest/.../cangling-update-linux-amd64
//! updates/
//!   cangling-update-linux-amd64
//!   cangling-update-linux-arm64
//! ```

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

/// 小于该体积不视为有效发布二进制（与 `update.rs` 下载校验一致）。
pub const MIN_BYTES: u64 = 1024;

const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Amd64,
    Arm64,
}

impl Arch {
    pub fn all() -> [Arch; 2] {
        [Arch::Amd64, Arch::Arm64]
    }

    pub fn host() -> Option<Self> {
        Self::parse(std::env::consts::ARCH)
    }

    /// 接受 `x86_64` / `amd64` / `linux-amd64` / `cangling-update-linux-amd64` 等写法。
    pub fn parse(raw: &str) -> Option<Self> {
        let s = raw.trim().to_ascii_lowercase();
        let s = s.strip_prefix("cangling-update-").unwrap_or(&s);
        match s {
            "x86_64" | "x86-64" | "amd64" | "linux-amd64" | "x64" => Some(Arch::Amd64),
            "aarch64" | "arm64" | "linux-arm64" | "armv8" | "armv8l" | "armv8a" => {
                Some(Arch::Arm64)
            }
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Arch::Amd64 => "linux-amd64",
            Arch::Arm64 => "linux-arm64",
        }
    }

    pub fn asset_name(self) -> &'static str {
        match self {
            Arch::Amd64 => "cangling-update-linux-amd64",
            Arch::Arm64 => "cangling-update-linux-arm64",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Arch::Amd64 => "x86_64",
            Arch::Arm64 => "ARM64",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StoredBinary {
    pub arch: String,
    pub label: String,
    pub available: bool,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

pub fn updates_dir(exe_dir: &Path) -> PathBuf {
    exe_dir.join("updates")
}

pub fn binary_path(exe_dir: &Path, arch: Arch) -> PathBuf {
    updates_dir(exe_dir).join(arch.asset_name())
}

pub fn ensure_updates_dir(exe_dir: &Path) -> Result<PathBuf> {
    let dir = updates_dir(exe_dir);
    fs::create_dir_all(&dir).with_context(|| format!("创建 {}", dir.display()))?;
    Ok(dir)
}

/// 把当前正在运行的程序拷到本机架构对应的槽位（覆盖写入，保证与运行版本一致）。
pub fn seed_own_binary(exe_dir: &Path) -> Result<Arch> {
    let arch = Arch::host().ok_or_else(|| {
        anyhow::anyhow!(
            "不支持的架构 {}，仅支持 x86_64 与 aarch64",
            std::env::consts::ARCH
        )
    })?;
    let src = current_exe()?;
    install_into(exe_dir, arch, &src)?;
    Ok(arch)
}

/// 把一份二进制导入 `updates/`，按 ELF 头识别架构。
pub fn import_file(exe_dir: &Path, src: &Path) -> Result<Arch> {
    let arch = elf_arch_file(src)
        .with_context(|| format!("{} 不是 x86_64/ARM64 的 ELF 可执行文件", src.display()))?;
    install_into(exe_dir, arch, src)?;
    Ok(arch)
}

pub fn install_into(exe_dir: &Path, arch: Arch, src: &Path) -> Result<PathBuf> {
    ensure_updates_dir(exe_dir)?;
    let dest = binary_path(exe_dir, arch);
    copy_executable(src, &dest)?;
    Ok(dest)
}

pub fn inventory(exe_dir: &Path) -> Vec<StoredBinary> {
    Arch::all()
        .into_iter()
        .map(|arch| match stored(exe_dir, arch) {
            Some(info) => info,
            None => StoredBinary {
                arch: arch.slug().to_string(),
                label: arch.label().to_string(),
                available: false,
                size: 0,
                path: None,
            },
        })
        .collect()
}

pub fn stored(exe_dir: &Path, arch: Arch) -> Option<StoredBinary> {
    if let Some(path) = find_np4_update_binary(exe_dir, arch) {
        if let Some(info) = stored_at(&path, arch) {
            return Some(info);
        }
    }
    stored_at(&binary_path(exe_dir, arch), arch)
}

fn stored_at(path: &Path, arch: Arch) -> Option<StoredBinary> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() < MIN_BYTES {
        return None;
    }
    if let Some(got) = elf_arch_file(path) {
        if got != arch {
            return None;
        }
    } else {
        return None;
    }
    Some(StoredBinary {
        arch: arch.slug().to_string(),
        label: arch.label().to_string(),
        available: true,
        size: meta.len(),
        path: Some(path.display().to_string()),
    })
}

/// keeper 同步的 `repo/np4/np4-update/latest/` 下按文件名或 ELF 头匹配。
fn find_np4_update_binary(exe_dir: &Path, arch: Arch) -> Option<PathBuf> {
    let latest = crate::repo::np4_update_latest_dir(exe_dir);
    if !latest.is_dir() {
        return None;
    }
    let want = arch.asset_name();
    let mut named = None;
    let mut elf_hit = None;
    for entry in walkdir::WalkDir::new(&latest)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let fname = entry.file_name().to_string_lossy();
        if fname == want {
            named = Some(path.to_path_buf());
            break;
        }
        if elf_hit.is_none() && elf_arch_file(path) == Some(arch) {
            elf_hit = Some(path.to_path_buf());
        }
    }
    named.or(elf_hit)
}

pub fn elf_arch_file(path: &Path) -> Option<Arch> {
    let mut buf = [0u8; 64];
    let mut f = fs::File::open(path).ok()?;
    use std::io::Read;
    let n = f.read(&mut buf).ok()?;
    elf_arch(&buf[..n])
}

pub fn elf_arch(bytes: &[u8]) -> Option<Arch> {
    if bytes.len() < 20 {
        return None;
    }
    if bytes[0..4] != *b"\x7fELF" {
        return None;
    }
    let em = match bytes[5] {
        2 => u16::from_be_bytes([bytes[18], bytes[19]]),
        _ => u16::from_le_bytes([bytes[18], bytes[19]]),
    };
    match em {
        EM_X86_64 => Some(Arch::Amd64),
        EM_AARCH64 => Some(Arch::Arm64),
        _ => None,
    }
}

/// master 版本是否新于 worker（需要给 worker 升级）。不降级。
pub fn needs_upgrade(master: &str, worker: &str) -> bool {
    is_newer(strip_v(master), strip_v(worker))
}

pub fn strip_v(tag: &str) -> &str {
    tag.trim().strip_prefix('v').unwrap_or(tag.trim())
}

pub fn is_newer(remote: &str, local: &str) -> bool {
    match (parse_semver(remote), parse_semver(local)) {
        (Some(r), Some(l)) => r > l,
        _ => remote != local,
    }
}

fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

pub fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("无法解析当前可执行文件")?;
    Ok(fs::canonicalize(&exe).unwrap_or(exe))
}

pub fn copy_executable(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("创建 {}", parent.display()))?;
    }
    let src_canon = fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());
    let dest_canon = fs::canonicalize(dest).unwrap_or_else(|_| dest.to_path_buf());
    if src_canon == dest_canon {
        set_exec(dest)?;
        return Ok(());
    }
    let tmp = dest.with_file_name(format!(
        ".{}.tmp",
        dest.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "bin".into())
    ));
    let _ = fs::remove_file(&tmp);
    fs::copy(src, &tmp).with_context(|| format!("复制 {} → {}", src.display(), tmp.display()))?;
    set_exec(&tmp)?;
    fs::rename(&tmp, dest).with_context(|| format!("写入 {}", dest.display()))?;
    Ok(())
}

/// 用新文件替换当前正在运行的程序（Linux 下对正在执行的 inode 改名是允许的）。
pub fn replace_current_exe(src: &Path) -> Result<PathBuf> {
    let dest = current_exe()?;
    copy_executable(src, &dest)?;
    Ok(dest)
}

fn set_exec(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(path)?.permissions();
        perm.set_mode(0o755);
        fs::set_permissions(path, perm)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn fake_elf(arch: Arch) -> Vec<u8> {
    let mut b = vec![0u8; 2048];
    b[0..4].copy_from_slice(b"\x7fELF");
    b[4] = 2;
    b[5] = 1;
    let em = match arch {
        Arch::Amd64 => EM_X86_64,
        Arch::Arm64 => EM_AARCH64,
    };
    b[18..20].copy_from_slice(&em.to_le_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cangling-binaries-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn arch_parses_common_aliases() {
        assert_eq!(Arch::parse("x86_64"), Some(Arch::Amd64));
        assert_eq!(Arch::parse("AMD64"), Some(Arch::Amd64));
        assert_eq!(Arch::parse("linux-amd64"), Some(Arch::Amd64));
        assert_eq!(
            Arch::parse("cangling-update-linux-amd64"),
            Some(Arch::Amd64)
        );
        assert_eq!(Arch::parse("aarch64"), Some(Arch::Arm64));
        assert_eq!(Arch::parse("arm64"), Some(Arch::Arm64));
        assert_eq!(Arch::parse("linux-arm64"), Some(Arch::Arm64));
        assert!(Arch::parse("riscv64").is_none());
        assert!(Arch::parse("").is_none());
    }

    #[test]
    fn elf_detects_both_arches_and_endianness() {
        assert_eq!(elf_arch(&fake_elf(Arch::Amd64)), Some(Arch::Amd64));
        assert_eq!(elf_arch(&fake_elf(Arch::Arm64)), Some(Arch::Arm64));
        let mut be = fake_elf(Arch::Arm64);
        be[5] = 2;
        be[18..20].copy_from_slice(&EM_AARCH64.to_be_bytes());
        assert_eq!(elf_arch(&be), Some(Arch::Arm64));
        assert!(elf_arch(b"not elf").is_none());
        assert!(elf_arch(&b"\x7fELF"[..]).is_none());
    }

    #[test]
    fn newer_versions() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(needs_upgrade("0.1.58", "0.1.57"));
        assert!(needs_upgrade("v0.2.0", "0.1.99"));
        assert!(!needs_upgrade("0.1.57", "0.1.57"));
        assert!(!needs_upgrade("0.1.57", "0.1.58"));
        assert!(!needs_upgrade("v0.1.57", "0.1.57"));
    }

    #[test]
    fn import_and_inventory_roundtrip() {
        let dir = temp_dir();
        let src = dir.join("incoming");
        fs::write(&src, fake_elf(Arch::Arm64)).unwrap();
        assert_eq!(import_file(&dir, &src).unwrap(), Arch::Arm64);

        let items = inventory(&dir);
        assert_eq!(items.len(), 2);
        let arm = items.iter().find(|b| b.arch == "linux-arm64").unwrap();
        let x86 = items.iter().find(|b| b.arch == "linux-amd64").unwrap();
        assert!(arm.available);
        assert_eq!(arm.size, 2048);
        assert!(!x86.available);

        let wrong = dir.join("wrong");
        fs::write(&wrong, fake_elf(Arch::Amd64)).unwrap();
        install_into(&dir, Arch::Arm64, &wrong).unwrap();
        assert!(stored(&dir, Arch::Arm64).is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_copies_current_exe_into_host_slot() {
        let dir = temp_dir();
        let arch = seed_own_binary(&dir).unwrap();
        assert_eq!(arch, Arch::host().unwrap());
        let info = stored(&dir, arch).expect("seeded binary");
        assert!(info.available);
        assert!(info.size >= MIN_BYTES);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefers_np4_update_latest_over_updates_slot() {
        let dir = temp_dir();
        let arch = Arch::Amd64;
        fs::create_dir_all(updates_dir(&dir)).unwrap();
        fs::write(binary_path(&dir, arch), fake_elf(arch)).unwrap();

        let latest = crate::repo::np4_update_latest_dir(&dir)
            .join("linux")
            .join("amd64");
        fs::create_dir_all(&latest).unwrap();
        let np4 = latest.join(arch.asset_name());
        let mut elf = fake_elf(arch);
        elf.resize(4096, 0);
        fs::write(&np4, &elf).unwrap();

        let info = stored(&dir, arch).expect("np4 binary");
        assert!(info
            .path
            .unwrap()
            .replace('\\', "/")
            .contains("np4/np4-update/latest"));
        assert_eq!(info.size, 4096);
        let _ = fs::remove_dir_all(&dir);
    }
}
