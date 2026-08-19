use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use tar::{Archive, Builder, EntryType, Header};
use walkdir::WalkDir;

#[derive(Serialize, Deserialize, Default)]
struct ArchiveMeta {
    bytes: u64,
    files: u64,
}

pub fn snapshot_directory(src: &Path, dst: &Path) -> Result<u64> {
    snapshot_directory_with_progress(src, dst, |_, _, _| {})
}

pub fn snapshot_directory_with_progress(
    src: &Path,
    dst: &Path,
    mut on_progress: impl FnMut(u64, u64, &str),
) -> Result<u64> {
    if !src.is_dir() {
        bail!("source is not a directory: {}", src.display());
    }

    if looks_like_archive(dst) {
        return snapshot_tar_gz(src, dst, &mut on_progress);
    }
    snapshot_copy_tree(src, dst, &mut on_progress)
}

pub fn restore_directory(snapshot: &Path, live: &Path) -> Result<()> {
    restore_directory_with_progress(snapshot, live, |_, _, _| {})
}

pub fn restore_directory_with_progress(
    snapshot: &Path,
    live: &Path,
    mut on_progress: impl FnMut(u64, u64, &str),
) -> Result<()> {
    if looks_like_archive(snapshot) {
        return restore_tar_gz(snapshot, live, &mut on_progress);
    }
    if snapshot.is_dir() {
        return restore_copy_tree(snapshot, live, &mut on_progress);
    }
    bail!("snapshot is missing: {}", snapshot.display());
}

fn looks_like_archive(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.ends_with(".tar.gz") || name.ends_with(".tgz")
}

fn snapshot_tar_gz(
    src: &Path,
    dst: &Path,
    on_progress: &mut impl FnMut(u64, u64, &str),
) -> Result<u64> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create snapshot dir {}", parent.display()))?;
    }

    let mut total = 0u64;
    let mut files = 0u64;
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry.with_context(|| format!("walk {}", src.display()))?;
        let rel = match entry.path().strip_prefix(src) {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };
        if should_skip(rel) {
            continue;
        }
        if entry.file_type().is_file() {
            total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
            files += 1;
        }
    }
    on_progress(0, total, "开始打包");

    let file = File::create(dst).with_context(|| format!("create {}", dst.display()))?;
    let enc = GzEncoder::new(BufWriter::new(file), Compression::default());
    let mut tar = Builder::new(enc);
    tar.follow_symlinks(false);

    let mut done = 0u64;
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry.with_context(|| format!("walk {}", src.display()))?;
        let rel = match entry.path().strip_prefix(src) {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };
        if should_skip(rel) {
            continue;
        }
        let name = rel.to_string_lossy().replace('\\', "/");
        let ft = entry.file_type();
        if ft.is_dir() {
            append_dir(&mut tar, entry.path(), &name)?;
        } else if ft.is_symlink() {
            tar.append_path_with_name(entry.path(), &name).with_context(|| {
                format!("archive symlink {}", entry.path().display())
            })?;
        } else if ft.is_file() {
            let mut f = File::open(entry.path())
                .with_context(|| format!("open {}", entry.path().display()))?;
            tar.append_file(&name, &mut f)
                .with_context(|| format!("archive {}", entry.path().display()))?;
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            done = done.saturating_add(len);
            on_progress(done, total, &name);
        }
    }
    tar.finish().context("finish tar")?;
    let encoder = tar.into_inner().context("unwrap gzip encoder")?;
    let mut writer = encoder.finish().context("finish gzip")?;
    writer.flush().context("flush snapshot")?;

    let meta = ArchiveMeta { bytes: total, files };
    let _ = fs::write(
        meta_path(dst),
        serde_json::to_vec(&meta).unwrap_or_else(|_| b"{}".to_vec()),
    );
    on_progress(total, total, "备份完成");
    Ok(files)
}

fn append_dir<W: Write>(tar: &mut Builder<W>, path: &Path, name: &str) -> Result<()> {
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mut header = Header::new_gnu();
    header.set_metadata(&meta);
    header.set_entry_type(EntryType::Directory);
    header.set_size(0);
    header.set_cksum();
    tar.append_data(&mut header, name, std::io::empty())
        .with_context(|| format!("archive dir {}", path.display()))?;
    Ok(())
}

fn restore_tar_gz(
    archive_path: &Path,
    live: &Path,
    on_progress: &mut impl FnMut(u64, u64, &str),
) -> Result<()> {
    if !archive_path.is_file() {
        bail!("snapshot is missing: {}", archive_path.display());
    }
    fs::create_dir_all(live).with_context(|| format!("create live dir {}", live.display()))?;

    let total = read_meta(archive_path)
        .map(|m| m.bytes)
        .unwrap_or_else(|| estimate_archive_bytes(archive_path).unwrap_or(0));
    on_progress(0, total, "开始恢复");

    let file = File::open(archive_path)
        .with_context(|| format!("open {}", archive_path.display()))?;
    let mut archive = Archive::new(GzDecoder::new(BufReader::new(file)));
    archive.set_overwrite(true);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);

    let mut kept = HashSet::new();
    let mut done = 0u64;
    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("tar entry")?;
        let rel = match entry.path() {
            Ok(p) => p.into_owned(),
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() || should_skip(&rel) || !is_safe_rel(&rel) {
            continue;
        }
        kept.insert(rel.clone());
        let dest = live.join(&rel);
        unpack_entry(&mut entry, &dest)?;
        let size = entry.header().size().unwrap_or(0);
        done = done.saturating_add(size);
        on_progress(done, total, &rel.to_string_lossy());
    }

    remove_extras(live, &kept)?;
    on_progress(total.max(done), total.max(done), "恢复完成");
    Ok(())
}

fn unpack_entry<R: std::io::Read>(
    entry: &mut tar::Entry<'_, R>,
    dest: &Path,
) -> Result<()> {
    let header = entry.header().clone();
    let kind = header.entry_type();
    let mode = header.mode().unwrap_or(0o644);
    let uid = header.uid().ok().map(|v| v as u32);
    let gid = header.gid().ok().map(|v| v as u32);

    match kind {
        EntryType::Directory => {
            fs::create_dir_all(dest)?;
        }
        EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            if dest.exists() {
                let _ = fs::remove_file(dest);
            }
            let mut out = File::create(dest)
                .with_context(|| format!("create {}", dest.display()))?;
            std::io::copy(entry, &mut out)
                .with_context(|| format!("write {}", dest.display()))?;
            out.flush()?;
        }
        EntryType::Symlink => {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let target = entry
                .link_name()
                .ok()
                .flatten()
                .map(|p| p.into_owned())
                .unwrap_or_default();
            let _ = fs::remove_file(dest);
            let _ = fs::remove_dir(dest);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, dest).with_context(|| {
                format!("symlink {} -> {}", dest.display(), target.display())
            })?;
            #[cfg(not(unix))]
            {
                let _ = target;
            }
        }
        _ => return Ok(()),
    }

    apply_meta(dest, mode, uid, gid, kind.is_dir())?;
    Ok(())
}

fn apply_meta(path: &Path, mode: u32, uid: Option<u32>, gid: Option<u32>, is_dir: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{chown, PermissionsExt};
        if !path.is_symlink() {
            let mode = if is_dir { mode | 0o111 } else { mode };
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
        }
        if uid.is_some() || gid.is_some() {
            let _ = chown(path, uid, gid);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode, uid, gid, is_dir);
    }
    Ok(())
}

fn estimate_archive_bytes(path: &Path) -> Result<u64> {
    let file = File::open(path)?;
    let mut archive = Archive::new(GzDecoder::new(BufReader::new(file)));
    let mut total = 0u64;
    for entry in archive.entries()? {
        let entry = entry?;
        total = total.saturating_add(entry.header().size().unwrap_or(0));
    }
    Ok(total)
}

fn read_meta(archive: &Path) -> Option<ArchiveMeta> {
    let raw = fs::read(meta_path(archive)).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn meta_path(archive: &Path) -> PathBuf {
    let mut p = archive.as_os_str().to_os_string();
    p.push(".meta.json");
    PathBuf::from(p)
}

fn is_safe_rel(rel: &Path) -> bool {
    rel.components()
        .all(|c| !matches!(c, std::path::Component::ParentDir | std::path::Component::RootDir))
}

fn snapshot_copy_tree(
    src: &Path,
    dst: &Path,
    on_progress: &mut impl FnMut(u64, u64, &str),
) -> Result<u64> {
    fs::create_dir_all(dst).with_context(|| format!("create snapshot {}", dst.display()))?;
    let mut total = 0u64;
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(src) {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };
        if should_skip(rel) {
            continue;
        }
        total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
    }
    on_progress(0, total, "开始备份");
    let mut files = 0u64;
    let mut done = 0u64;
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry?;
        let rel = match entry.path().strip_prefix(src) {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };
        if should_skip(rel) {
            continue;
        }
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)?;
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            done = done.saturating_add(len);
            files += 1;
            on_progress(done, total, &rel.to_string_lossy());
        }
    }
    on_progress(total, total, "备份完成");
    Ok(files)
}

fn restore_copy_tree(
    snapshot: &Path,
    live: &Path,
    on_progress: &mut impl FnMut(u64, u64, &str),
) -> Result<()> {
    snapshot_copy_tree(snapshot, live, on_progress)?;
    let kept = collect_rel_paths(snapshot)?;
    remove_extras(live, &kept)
}

fn collect_rel_paths(root: &Path) -> Result<HashSet<PathBuf>> {
    let mut set = HashSet::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if let Ok(rel) = entry.path().strip_prefix(root) {
            if !rel.as_os_str().is_empty() && !should_skip(rel) {
                set.insert(rel.to_path_buf());
            }
        }
    }
    Ok(set)
}

fn remove_extras(live: &Path, kept: &HashSet<PathBuf>) -> Result<()> {
    let mut extras: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(live).follow_links(false).contents_first(true) {
        let entry = entry?;
        let rel = match entry.path().strip_prefix(live) {
            Ok(r) if !r.as_os_str().is_empty() => r.to_path_buf(),
            _ => continue,
        };
        if should_skip(&rel) {
            continue;
        }
        if !kept.contains(&rel) {
            extras.push(entry.path().to_path_buf());
        }
    }
    for path in extras {
        let _ = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
    }
    Ok(())
}

fn should_skip(rel: &Path) -> bool {
    rel.components().any(|c| {
        let s = c.as_os_str();
        s == ".git" || s == "lost+found"
    })
}

pub fn remove_dir_if_exists(path: &Path) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
    } else if path.is_file() {
        fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
        let meta = meta_path(path);
        if meta.exists() {
            let _ = fs::remove_file(meta);
        }
    }
    Ok(())
}

#[cfg(unix)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn tar_roundtrip_keeps_mode() {
        let root = std::env::temp_dir().join(format!(
            "cangling-tar-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = root.join("src");
        let dst = root.join("out.tar.gz");
        let live = root.join("live");
        fs::create_dir_all(&src).unwrap();
        let file = src.join("app.jar");
        fs::write(&file, b"hello-jar").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();

        snapshot_directory(&src, &dst).unwrap();
        assert!(dst.is_file());
        restore_directory(&dst, &live).unwrap();
        let restored = live.join("app.jar");
        assert_eq!(fs::read(&restored).unwrap(), b"hello-jar");
        let mode = fs::metadata(&restored).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        let _ = fs::remove_dir_all(&root);
    }
}
