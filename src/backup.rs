use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

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
    std::fs::create_dir_all(dst)
        .with_context(|| format!("create snapshot {}", dst.display()))?;

    let mut total = 0u64;
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry.with_context(|| format!("walk {}", src.display()))?;
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
        let entry = entry.with_context(|| format!("walk {}", src.display()))?;
        let rel = match entry.path().strip_prefix(src) {
            Ok(r) if !r.as_os_str().is_empty() => r,
            _ => continue,
        };
        if should_skip(rel) {
            continue;
        }
        let target = dst.join(rel);
        let ft = entry.file_type();
        if ft.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if ft.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copy {} -> {}", entry.path().display(), target.display()))?;
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            done = done.saturating_add(len);
            files += 1;
            let name = rel.to_string_lossy();
            on_progress(done, total, name.as_ref());
        }
    }
    on_progress(total, total, "备份完成");
    Ok(files)
}

pub fn restore_directory(snapshot: &Path, live: &Path) -> Result<()> {
    if !snapshot.is_dir() {
        bail!("snapshot is missing: {}", snapshot.display());
    }
    std::fs::create_dir_all(live)
        .with_context(|| format!("create live dir {}", live.display()))?;

    let snapshot_paths = collect_rel_paths(snapshot)?;
    snapshot_directory(snapshot, live)?;

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
        if !snapshot_paths.contains(&rel) {
            extras.push(entry.path().to_path_buf());
        }
    }
    for path in extras {
        let _ = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
    }
    Ok(())
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

fn should_skip(rel: &Path) -> bool {
    rel.components().any(|c| {
        let s = c.as_os_str();
        s == ".git" || s == "lost+found"
    })
}

pub fn remove_dir_if_exists(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}
