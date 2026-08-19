use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

    if looks_like_gitref(dst) {
        if git_available() {
            return snapshot_git(src, dst, &mut on_progress);
        }
        let tar = dst.with_file_name("tree.tar.gz");
        let n = snapshot_tar_gz(src, &tar, &mut on_progress)?;
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(dst, format!("tar\n{}\n", tar.display()))?;
        return Ok(n);
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
    if looks_like_gitref(snapshot) {
        return restore_gitref(snapshot, live, &mut on_progress);
    }
    if looks_like_archive(snapshot) {
        return restore_tar_gz(snapshot, live, &mut on_progress);
    }
    if snapshot.is_dir() {
        return restore_copy_tree(snapshot, live, &mut on_progress);
    }
    bail!("snapshot is missing: {}", snapshot.display());
}

fn looks_like_gitref(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("gitref"))
        .unwrap_or(false)
}

fn looks_like_archive(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.ends_with(".tar.gz") || name.ends_with(".tgz")
}

#[derive(Serialize, Deserialize)]
struct FileAttr {
    path: String,
    kind: String,
    mode: u32,
    uid: u32,
    gid: u32,
    #[serde(default)]
    target: Option<String>,
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn snapshot_git(
    src: &Path,
    gitref: &Path,
    on_progress: &mut impl FnMut(u64, u64, &str),
) -> Result<u64> {
    let version_dir = gitref
        .parent()
        .context("gitref has no parent")?;
    let project_dir = version_dir
        .parent()
        .context("version dir has no parent")?;
    let repo = project_dir.join("repo.git");
    ensure_git_repo(&repo)?;

    let mut attrs = Vec::new();
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
        let path = rel.to_string_lossy().replace('\\', "/");
        let meta = entry.metadata().ok();
        let (mode, uid, gid) = unix_ids(entry.path(), meta.as_ref());
        let ft = entry.file_type();
        if ft.is_dir() {
            attrs.push(FileAttr {
                path,
                kind: "dir".into(),
                mode,
                uid,
                gid,
                target: None,
            });
        } else if ft.is_symlink() {
            let target = fs::read_link(entry.path())
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            attrs.push(FileAttr {
                path,
                kind: "symlink".into(),
                mode,
                uid,
                gid,
                target,
            });
        } else if ft.is_file() {
            let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            total = total.saturating_add(len);
            files += 1;
            attrs.push(FileAttr {
                path: path.clone(),
                kind: "file".into(),
                mode,
                uid,
                gid,
                target: None,
            });
            on_progress(files, 0, &format!("扫描 {path}"));
        }
    }

    let total = total.max(1);
    on_progress(0, total, "开始写入 Git 对象");

    let mut file_entries: Vec<(String, PathBuf, u64, u32)> = Vec::new();
    let mut symlink_entries: Vec<(String, String)> = Vec::new();
    for attr in &attrs {
        match attr.kind.as_str() {
            "file" => {
                file_entries.push((
                    attr.path.clone(),
                    src.join(&attr.path),
                    fs::metadata(src.join(&attr.path))
                        .map(|m| m.len())
                        .unwrap_or(0),
                    attr.mode,
                ));
            }
            "symlink" => {
                if let Some(t) = &attr.target {
                    symlink_entries.push((attr.path.clone(), t.clone()));
                }
            }
            _ => {}
        }
    }

    let mut index_info = String::new();
    let mut done = 0u64;
    let mut current_paths = HashSet::new();
    for (rel, abs, len, mode) in &file_entries {
        current_paths.insert(rel.clone());
        let sha = git_hash_object_file(&repo, abs, rel, done, total, on_progress)?;
        let git_mode = if mode & 0o111 != 0 { "100755" } else { "100644" };
        index_info.push_str(&format!("{git_mode} blob {sha}\t{rel}\n"));
        done = done.saturating_add(*len);
        on_progress(done, total, rel);
    }
    for (rel, target) in &symlink_entries {
        current_paths.insert(rel.clone());
        let sha = git_hash_object_bytes(&repo, target.as_bytes())?;
        index_info.push_str(&format!("120000 blob {sha}\t{rel}\n"));
    }

    on_progress(done, total, "正在更新 Git 索引…");
    git_update_index(&repo, &index_info)?;
    prune_index(&repo, &current_paths)?;

    git_run(
        &repo,
        Some(src),
        &["commit", "--allow-empty", "-m", "cangling snapshot"],
    )?;
    let sha = git_run(&repo, None, &["rev-parse", "HEAD"])?;
    let sha = sha.trim().to_string();
    write_gitref(gitref, &sha)?;
    write_git_attrs(&repo, &sha, &attrs)?;
    on_progress(total, total, "备份完成");
    Ok(files)
}

fn git_hash_object_file(
    repo: &Path,
    file: &Path,
    rel: &str,
    start: u64,
    total: u64,
    on_progress: &mut impl FnMut(u64, u64, &str),
) -> Result<String> {
    let mut child = git_base(repo, None)
        .args(["hash-object", "-w", "--stdin", "--path", rel])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 git hash-object")?;
    let mut stdin = child.stdin.take().context("git hash-object stdin")?;
    let mut f = File::open(file).with_context(|| format!("open {}", file.display()))?;
    let mut buf = vec![0u8; 1024 * 1024];
    let mut copied = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        stdin
            .write_all(&buf[..n])
            .context("写入 git hash-object")?;
        copied += n as u64;
        on_progress(start.saturating_add(copied), total, rel);
    }
    drop(stdin);
    let output = child
        .wait_with_output()
        .context("等待 git hash-object")?;
    if !output.status.success() {
        bail!(
            "git hash-object {} 失败：{}",
            rel,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_hash_object_bytes(repo: &Path, data: &[u8]) -> Result<String> {
    let mut child = git_base(repo, None)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 git hash-object")?;
    {
        let mut stdin = child.stdin.take().context("git hash-object stdin")?;
        stdin.write_all(data)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "git hash-object 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_update_index(repo: &Path, info: &str) -> Result<()> {
    if info.is_empty() {
        return Ok(());
    }
    let mut child = git_base(repo, None)
        .args(["update-index", "--add", "--index-info"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 git update-index")?;
    {
        let mut stdin = child.stdin.take().context("git update-index stdin")?;
        stdin.write_all(info.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "git update-index 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn prune_index(repo: &Path, keep: &HashSet<String>) -> Result<()> {
    let listed = match git_run(repo, None, &["ls-files"]) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    for line in listed.lines() {
        let path = line.trim();
        if path.is_empty() || keep.contains(path) {
            continue;
        }
        let _ = git_run(repo, None, &["rm", "--cached", "-f", "--", path]);
    }
    Ok(())
}

fn restore_gitref(
    gitref: &Path,
    live: &Path,
    on_progress: &mut impl FnMut(u64, u64, &str),
) -> Result<()> {
    let text = fs::read_to_string(gitref)
        .with_context(|| format!("read {}", gitref.display()))?;
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").trim();
    if first == "tar" {
        let tar = lines
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| gitref.with_file_name("tree.tar.gz"));
        return restore_tar_gz(&tar, live, on_progress);
    }
    let sha = first;
    if sha.is_empty() {
        bail!("gitref 为空：{}", gitref.display());
    }
    let repo = gitref
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("repo.git"))
        .context("cannot resolve repo.git")?;
    if !repo.join("HEAD").exists() {
        bail!("Git 仓库不存在：{}", repo.display());
    }

    fs::create_dir_all(live)?;
    on_progress(0, 1, "正在检出 Git 快照…");
    git_run(&repo, Some(live), &["checkout", "-f", sha])?;

    let attrs = read_git_attrs(&repo, sha).unwrap_or_default();
    let total = attrs.len() as u64;
    let mut kept = HashSet::new();
    for (i, attr) in attrs.iter().enumerate() {
        let rel = PathBuf::from(&attr.path);
        if !is_safe_rel(&rel) {
            continue;
        }
        kept.insert(rel.clone());
        let dest = live.join(&rel);
        if attr.kind == "dir" {
            fs::create_dir_all(&dest)?;
        }
        apply_meta(
            &dest,
            attr.mode,
            Some(attr.uid),
            Some(attr.gid),
            attr.kind == "dir",
        )?;
        on_progress(i as u64 + 1, total.max(1), &attr.path);
    }

    if let Ok(list) = git_run(&repo, None, &["ls-tree", "-r", "--name-only", sha]) {
        for line in list.lines() {
            let rel = PathBuf::from(line);
            if is_safe_rel(&rel) && !rel.as_os_str().is_empty() {
                kept.insert(rel);
            }
        }
    }
    remove_extras(live, &kept)?;
    on_progress(1, 1, "恢复完成");
    Ok(())
}

fn ensure_git_repo(repo: &Path) -> Result<()> {
    if !repo.join("HEAD").exists() {
        fs::create_dir_all(repo)?;
        git_run(repo, None, &["init", "--bare"])?;
        git_run(repo, None, &["config", "user.name", "cangling-update"])?;
        git_run(repo, None, &["config", "user.email", "cangling-update@localhost"])?;
        let exclude = repo.join("info").join("exclude");
        if let Some(parent) = exclude.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(exclude, ".git\nlost+found\n")?;
    }
    Ok(())
}

fn git_base(repo: &Path, work: Option<&Path>) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-c").arg("user.name=cangling-update");
    cmd.arg("-c").arg("user.email=cangling-update@localhost");
    cmd.arg("-c").arg("core.quotepath=false");
    cmd.arg("-c").arg("commit.gpgsign=false");
    cmd.arg("--git-dir").arg(repo);
    if let Some(w) = work {
        cmd.arg("--work-tree").arg(w);
    }
    cmd
}

fn git_run(repo: &Path, work: Option<&Path>, args: &[&str]) -> Result<String> {
    let output = git_base(repo, work)
        .args(args)
        .output()
        .with_context(|| format!("执行 git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} 失败：{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn write_gitref(path: &Path, sha: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, format!("{sha}\n")).with_context(|| format!("write {}", path.display()))
}

fn write_git_attrs(repo: &Path, sha: &str, attrs: &[FileAttr]) -> Result<()> {
    let dir = repo.join("cangling-meta");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{sha}.json"));
    let data = serde_json::to_vec(attrs).context("serialize attrs")?;
    fs::write(path, data)?;
    Ok(())
}

fn read_git_attrs(repo: &Path, sha: &str) -> Result<Vec<FileAttr>> {
    let path = repo.join("cangling-meta").join(format!("{sha}.json"));
    let data = fs::read(path)?;
    Ok(serde_json::from_slice(&data)?)
}

fn unix_ids(path: &Path, meta: Option<&fs::Metadata>) -> (u32, u32, u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if let Some(m) = meta {
            return (m.permissions().mode(), m.uid(), m.gid());
        }
        if let Ok(m) = fs::symlink_metadata(path) {
            return (m.permissions().mode(), m.uid(), m.gid());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, meta);
    }
    (0o644, 0, 0)
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
        use std::os::unix::fs::PermissionsExt;
        if !path.is_symlink() {
            let mode = if is_dir { mode | 0o111 } else { mode };
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
        }
        if uid.is_some() || gid.is_some() {
            let _ = std::os::unix::fs::lchown(path, uid, gid);
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

    #[test]
    fn git_roundtrip_keeps_mode_and_dedups() {
        if !git_available() {
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "cangling-git-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = root.join("src");
        let live = root.join("live");
        let gitref = root.join("pid").join("vid").join("tree.gitref");
        fs::create_dir_all(&src).unwrap();
        let file = src.join("app.jar");
        fs::write(&file, b"hello-git-jar").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();

        snapshot_directory(&src, &gitref).unwrap();
        assert!(gitref.is_file());
        assert!(root.join("pid").join("repo.git").join("HEAD").exists());
        restore_directory(&gitref, &live).unwrap();
        let restored = live.join("app.jar");
        assert_eq!(fs::read(&restored).unwrap(), b"hello-git-jar");
        let mode = fs::metadata(&restored).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        let _ = fs::remove_dir_all(&root);
    }
}
