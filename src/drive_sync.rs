//! Read-only drive snapshots. Never publish partial or unverified installation resources.
use fs2::FileExt;
use reqwest::{header, Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

pub const DEFAULT_URL: &str = "https://soft.cangling.cn:22002";
pub const DEFAULT_DIRECTORY: &str = "05f8876b41974cfdadfa1f000d77809c";
// Embedded read-only credential explicitly supplied for the distributed clients.
pub const DEFAULT_TOKEN: &str =
    "drv_9f835f59befa11baaad09c467e9fc0c0134e2ed7d7f4092fc5d99446c567b4f9";
const IDLE: Duration = Duration::from_secs(45);
const ATTEMPTS: u32 = 5;

#[derive(Clone)]
pub struct Config {
    pub server: String,
    pub directory: String,
    pub token: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            server: DEFAULT_URL.into(),
            directory: DEFAULT_DIRECTORY.into(),
            token: DEFAULT_TOKEN.into(),
        }
        .environment()
    }
}
impl Config {
    pub fn environment(mut self) -> Self {
        if let Ok(v) = std::env::var("CANGLING_DRIVE_URL") {
            self.server = v;
        }
        if let Ok(v) = std::env::var("CANGLING_DRIVE_DIRECTORY") {
            self.directory = v;
        }
        if let Ok(v) = std::env::var("CANGLING_DRIVE_TOKEN") {
            self.token = v;
        }
        self
    }
    pub fn validate(&self) -> Result<(), String> {
        self.client().map(|_| ())
    }
    fn client(&self) -> Result<(Client, Url), String> {
        let mut url = Url::parse(self.server.trim()).map_err(|_| "网盘地址无效".to_string())?;
        if !(url.scheme() == "https"
            || (url.scheme() == "http"
                && matches!(
                    url.host_str(),
                    Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
                )))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("网盘仅支持 HTTPS（本机测试可使用 HTTP），地址不能含帐号或查询参数".into());
        }
        if !valid_id(&self.directory) || self.token.trim().is_empty() {
            return Err("网盘目录 ID 或读取 key 无效".into());
        }
        let path = format!("{}/api/v1/drive/", url.path().trim_end_matches('/'));
        url.set_path(&path);
        let mut token = header::HeaderValue::from_str(&self.token)
            .map_err(|_| "网盘读取 key 无效".to_string())?;
        token.set_sensitive(true);
        let mut headers = header::HeaderMap::new();
        headers.insert("X-Drive-Token", token);
        let client = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| "无法创建网盘客户端".to_string())?;
        Ok((client, url))
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub directory: bool,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub sha256: Option<String>,
}
#[derive(Deserialize)]
struct Page {
    entries: Vec<Entry>,
    total: u64,
    page: u64,
    #[serde(rename = "pageSize")]
    page_size: u64,
}
#[derive(Deserialize)]
struct Envelope {
    code: i32,
    data: Option<Page>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Remote {
    pub path: PathBuf,
    pub entry: Entry,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub downloaded: u64,
    pub skipped: u64,
    pub resumed: u64,
    pub total: u64,
    pub bytes: u64,
    pub snapshot: String,
}
#[derive(Clone)]
pub struct Progress {
    pub phase: &'static str,
    pub path: String,
    pub current: u64,
    pub total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub overall_done: u64,
    pub overall_total: u64,
}
fn valid_id(v: &str) -> bool {
    v.len() == 32
        && v.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn valid_name(name: &str) -> bool {
    if name.is_empty()
        || name.trim() != name
        || name == "."
        || name == ".."
        || name.eq_ignore_ascii_case(".git")
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return false;
    }
    if cfg!(windows) {
        let base = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        if name.contains([':', '*', '?', '"', '<', '>', '|'])
            || name.ends_with('.')
            || matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((base.starts_with("COM") || base.starts_with("LPT"))
                && base.len() == 4
                && matches!(base.as_bytes()[3], b'1'..=b'9'))
        {
            return false;
        }
    }
    Path::new(name).components().count() == 1
        && matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
}
fn io(error: std::io::Error) -> String {
    format!("本地资源文件操作失败：{error}")
}
fn safe_dir(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
            Err("资源目录包含符号链接或非目录，已停止同步".into())
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(path).map_err(io),
        Err(e) => Err(io(e)),
    }
}
fn safe_path(root: &Path, rel: &Path) -> Result<PathBuf, String> {
    let mut result = root.to_path_buf();
    let parts: Vec<_> = rel.components().collect();
    for (i, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err("远端文件路径无效".into());
        };
        result.push(name);
        if i + 1 < parts.len() {
            safe_dir(&result)?;
        } else if let Ok(m) = fs::symlink_metadata(&result) {
            if m.file_type().is_symlink() {
                return Err("资源文件不能是符号链接".into());
            }
        }
    }
    Ok(result)
}
fn regular(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && !m.file_type().is_symlink())
}
fn digest(path: &Path) -> Result<String, String> {
    let mut f = File::open(path).map_err(io)?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn matches(path: &Path, entry: &Entry) -> bool {
    regular(path)
        && fs::metadata(path).is_ok_and(|m| m.len() == entry.size)
        && digest(path).is_ok_and(|h| Some(&h) == entry.sha256.as_ref())
}
fn reusable(root: &Path, rel: &Path) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    if !fs::symlink_metadata(root).ok()?.is_dir() {
        return None;
    }
    for c in rel.components() {
        let Component::Normal(c) = c else { return None };
        path.push(c);
        if fs::symlink_metadata(&path).ok()?.file_type().is_symlink() {
            return None;
        }
    }
    Some(path)
}
fn executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut magic = [0u8; 4];
        let mut file = File::open(path).map_err(io)?;
        let n = file.read(&mut magic).map_err(io)?;
        if (n >= 2 && &magic[..2] == b"#!")
            || (n == 4 && &magic == b"\x7fELF")
            || path.extension().is_some_and(|s| s == "sh" || s == "run")
        {
            let mode = fs::metadata(path).map_err(io)?.permissions().mode() | 0o111;
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(io)?;
        }
    }
    Ok(())
}
fn workspace(destination: &Path) -> Result<PathBuf, String> {
    let parent = destination.parent().ok_or("资源目录无父目录")?;
    let parent = parent.canonicalize().map_err(io)?;
    let name = destination
        .file_name()
        .ok_or("资源目录无效")?
        .to_string_lossy();
    Ok(parent.join(format!(".{name}.drive-sync")))
}
pub fn completed(destination: &Path) -> bool {
    workspace(destination).is_ok_and(|w| {
        !w.join("publish.json").exists()
            && destination.is_dir()
            && !destination.is_symlink()
            && fs::read(w.join("complete.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Report>(&b).ok())
                .is_some_and(|r| r.snapshot.len() == 64)
    })
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    if fs::symlink_metadata(&tmp).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("状态文件路径异常".into());
    }
    let bytes = serde_json::to_vec(value).map_err(|_| "无法保存资源状态".to_string())?;
    let mut f = File::create(&tmp).map_err(io)?;
    f.write_all(&bytes).map_err(io)?;
    f.sync_all().map_err(io)?;
    fs::rename(tmp, path).map_err(io)
}
fn recover(destination: &Path, work: &Path) -> Result<(), String> {
    let previous = work.join("previous");
    let tree = work.join("tree");
    let journal = work.join("publish.json");
    if journal.exists() {
        if destination.exists() && !tree.exists() {
            let report: Report = serde_json::from_slice(&fs::read(&journal).map_err(io)?)
                .map_err(|_| "资源发布记录损坏".to_string())?;
            write_json(&work.join("complete.json"), &report)?;
        } else if !destination.exists() && previous.exists() {
            fs::rename(&previous, destination).map_err(io)?;
        }
        fs::remove_file(&journal).map_err(io)?;
    }
    if previous.exists() {
        if !destination.exists() {
            fs::rename(&previous, destination).map_err(io)?;
        } else {
            fs::remove_dir_all(previous).map_err(io)?;
        }
    }
    Ok(())
}
fn retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}
async fn retry_delay(attempt: u32) {
    #[cfg(test)]
    let wait = Duration::from_millis(10 << attempt);
    #[cfg(not(test))]
    let wait = Duration::from_secs(1 << attempt);
    tokio::time::sleep(wait).await;
}
async fn page(client: &Client, base: &Url, parent: &str, number: u64) -> Result<Page, String> {
    for attempt in 0..ATTEMPTS {
        let request = client.get(base.join("entries").unwrap()).query(&[
            ("parentId", parent),
            ("pageSize", "1000"),
            ("page", &number.to_string()),
        ]);
        let result = tokio::time::timeout(IDLE, request.send()).await;
        match result {
            Ok(Ok(response)) if response.status() == StatusCode::OK => {
                // Bound metadata before decoding; never buffer an unbounded response.
                let mut response = response;
                let mut bytes = Vec::new();
                loop {
                    let chunk = tokio::time::timeout(IDLE, response.chunk())
                        .await
                        .map_err(|_| "读取网盘目录超时")?
                        .map_err(|_| "读取网盘目录失败")?;
                    let Some(chunk) = chunk else { break };
                    if bytes.len() + chunk.len() > 16 * 1024 * 1024 {
                        return Err("网盘目录响应过大".into());
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let value: Envelope =
                    serde_json::from_slice(&bytes).map_err(|_| "网盘目录响应无效")?;
                if value.code != 200 {
                    return Err("读取网盘目录失败".into());
                }
                return value.data.ok_or_else(|| "网盘目录缺少清单".into());
            }
            Ok(Ok(response)) if !retryable(response.status()) => {
                return Err(format!(
                    "读取网盘目录失败（HTTP {}）",
                    response.status().as_u16()
                ))
            }
            _ => {}
        }
        if attempt + 1 < ATTEMPTS {
            retry_delay(attempt).await;
        }
    }
    Err("读取网盘目录失败，已自动重试，请稍后继续同步".into())
}
pub async fn manifest(config: &Config) -> Result<Vec<Remote>, String> {
    let (client, base) = config.client()?;
    enumerate(&client, &base, &config.directory).await
}
async fn enumerate(client: &Client, base: &Url, id: &str) -> Result<Vec<Remote>, String> {
    let mut queue = VecDeque::from([(id.to_string(), PathBuf::new(), 0usize)]);
    let mut ids = HashSet::new();
    ids.insert(id.to_string());
    let mut paths = HashSet::new();
    let mut files = Vec::new();
    while let Some((parent, path, depth)) = queue.pop_front() {
        if depth > 127 || files.len() > 1_000_000 {
            return Err("网盘目录层级或文件数超过限制".into());
        }
        let mut number = 1;
        loop {
            let value = page(client, base, &parent, number).await?;
            if value.page != number
                || value.page_size == 0
                || value.page_size > 1000
                || value.entries.len() > 1000
            {
                return Err("网盘分页响应无效".into());
            }
            for mut entry in value.entries {
                if files.len() >= 1_000_000 {
                    return Err("网盘文件数超过限制".into());
                }
                if !valid_id(&entry.id) || !valid_name(&entry.name) || !ids.insert(entry.id.clone())
                {
                    return Err("网盘包含无效或重复的文件路径".into());
                }
                let rel = path.join(&entry.name);
                let key = if cfg!(windows) {
                    rel.to_string_lossy().to_lowercase()
                } else {
                    rel.to_string_lossy().to_string()
                };
                if !paths.insert(key) {
                    return Err("网盘存在冲突文件名".into());
                }
                if entry.directory {
                    queue.push_back((entry.id.clone(), rel.clone(), depth + 1));
                } else {
                    let hash = entry.sha256.as_deref().unwrap_or("");
                    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return Err("网盘文件缺少 SHA-256".into());
                    }
                    entry.sha256 = Some(hash.to_ascii_lowercase());
                }
                files.push(Remote { path: rel, entry });
            }
            if number.checked_mul(value.page_size).ok_or("网盘分页溢出")? >= value.total {
                break;
            }
            number += 1;
            if number > 100_000 {
                return Err("网盘分页超过限制".into());
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}
fn snapshot(entries: &[Remote]) -> Result<String, String> {
    let bytes = serde_json::to_vec(entries).map_err(|_| "无法校验资源清单")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
async fn download(
    client: &Client,
    base: &Url,
    entry: &Entry,
    part: &Path,
    mut progress: impl FnMut(u64),
) -> Result<bool, String> {
    if fs::symlink_metadata(part).is_ok_and(|m| m.file_type().is_symlink() || !m.is_file()) {
        return Err("下载暂存文件异常".into());
    }
    let mut resumed = false;
    for attempt in 0..ATTEMPTS {
        let mut offset = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        if offset > entry.size {
            File::create(part).map_err(io)?;
            offset = 0;
        }
        if offset == entry.size && regular(part) {
            if matches(part, entry) {
                return Ok(resumed || offset > 0);
            }
            File::create(part).map_err(io)?;
            offset = 0;
        }
        let request = client
            .get(base.join(&format!("files/{}/content", entry.id)).unwrap())
            .header(header::ACCEPT_ENCODING, "identity");
        let request = if offset > 0 {
            request.header(header::RANGE, format!("bytes={offset}-"))
        } else {
            request
        };
        let result = tokio::time::timeout(IDLE, request.send()).await;
        if let Ok(Ok(mut response)) = result {
            let status = response.status();
            if status == StatusCode::OK || status == StatusCode::PARTIAL_CONTENT {
                if status == StatusCode::PARTIAL_CONTENT {
                    resumed |= offset > 0;
                    let expected = format!(
                        "bytes {offset}-{}/{}",
                        entry.size.saturating_sub(1),
                        entry.size
                    );
                    if response
                        .headers()
                        .get(header::CONTENT_RANGE)
                        .and_then(|h| h.to_str().ok())
                        != Some(expected.as_str())
                    {
                        return Err("网盘续传范围不匹配，未修改已发布资源".into());
                    }
                } else if offset > 0 {
                    File::create(part).map_err(io)?;
                    offset = 0;
                }
                if response
                    .content_length()
                    .is_some_and(|n| n != entry.size - offset)
                {
                    return Err("网盘下载长度不匹配".into());
                }
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(part)
                    .map_err(io)?;
                let mut count = offset;
                let mut interrupted = false;
                loop {
                    match tokio::time::timeout(IDLE, response.chunk()).await {
                        Ok(Ok(Some(chunk))) => {
                            if chunk.len() as u64 > entry.size.saturating_sub(count) {
                                return Err("网盘文件超出声明长度".into());
                            }
                            file.write_all(&chunk).map_err(io)?;
                            count += chunk.len() as u64;
                            progress(count);
                        }
                        Ok(Ok(None)) => break,
                        _ => {
                            interrupted = true;
                            break;
                        }
                    }
                }
                file.sync_all().map_err(io)?;
                drop(file);
                if !interrupted && count == entry.size {
                    if matches(part, entry) {
                        return Ok(resumed);
                    }
                    File::create(part).map_err(io)?;
                }
            } else if !retryable(status) {
                return Err(format!("下载网盘文件失败（HTTP {}）", status.as_u16()));
            }
        }
        if attempt + 1 < ATTEMPTS {
            retry_delay(attempt).await;
        }
    }
    Err("网盘下载中断或摘要校验失败，已保留断点，请重新同步".into())
}

pub async fn synchronize(
    config: &Config,
    destination: &Path,
    mut emit: impl FnMut(Progress),
) -> Result<Report, String> {
    let (client, base) = config.client()?;
    let parent = destination.parent().ok_or("资源目录无父目录")?;
    fs::create_dir_all(parent).map_err(io)?;
    if destination.is_symlink() || (destination.exists() && !destination.is_dir()) {
        return Err("资源目录必须是真实目录，不能是符号链接".into());
    }
    let work = workspace(destination)?;
    safe_dir(&work)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&work, fs::Permissions::from_mode(0o700)).map_err(io)?;
    }
    let lock_path = work.join("sync.lock");
    if lock_path.is_symlink() {
        return Err("同步锁路径异常".into());
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(io)?;
    lock.try_lock_exclusive()
        .map_err(|_| "资源正在由其他进程同步，请稍后重试")?;
    recover(destination, &work)?;
    let tree = work.join("tree");
    safe_dir(&tree)?;
    let parts = work.join("parts");
    safe_dir(&parts)?;
    emit(Progress {
        phase: "manifest",
        path: "正在读取网盘目录清单…".into(),
        current: 0,
        total: 0,
        bytes_done: 0,
        bytes_total: 0,
        overall_done: 0,
        overall_total: 0,
    });
    let entries = enumerate(&client, &base, &config.directory).await?;
    let revision = snapshot(&entries)?;
    let total = entries.iter().filter(|e| !e.entry.directory).count() as u64;
    let bytes = entries.iter().try_fold(0u64, |sum, e| {
        sum.checked_add(if e.entry.directory { 0 } else { e.entry.size })
            .ok_or("网盘总大小溢出")
    })?;
    let mut report = Report {
        total,
        bytes,
        snapshot: revision.clone(),
        ..Report::default()
    };
    let mut done = 0u64;
    let mut current = 0;
    let expected: HashSet<_> = entries.iter().map(|e| e.path.clone()).collect();
    // Staging may contain an older attempt. Remove only its obsolete entries, never the live tree.
    prune(&tree, &tree, &expected)?;
    for remote in &entries {
        let target = safe_path(&tree, &remote.path)?;
        if remote.entry.directory {
            if regular(&target) {
                fs::remove_file(&target).map_err(io)?;
            }
            safe_dir(&target)?;
            continue;
        }
        if target.is_dir() {
            fs::remove_dir_all(&target).map_err(io)?;
        }
        current += 1;
        let entry = &remote.entry;
        let mut skipped = matches(&target, entry);
        if !skipped {
            if let Some(existing) = reusable(destination, &remote.path) {
                if matches(&existing, entry) {
                    let copy = parts.join(format!("{}-copy", entry.id));
                    if copy.is_symlink() {
                        return Err("复用文件路径异常".into());
                    }
                    fs::copy(&existing, &copy).map_err(io)?;
                    if matches(&copy, entry) {
                        executable(&copy)?;
                        fs::rename(&copy, &target).map_err(io)?;
                        skipped = true;
                    } else {
                        let _ = fs::remove_file(copy);
                    }
                }
            }
        }
        if skipped {
            report.skipped += 1;
        } else {
            let part = parts.join(format!(
                "{}-{}-{}.part",
                entry.id,
                entry.sha256.as_deref().unwrap(),
                entry.size
            ));
            let mut last = Instant::now() - Duration::from_secs(1);
            let resumed = download(&client, &base, entry, &part, |n| {
                if last.elapsed() >= Duration::from_millis(200) || n == entry.size {
                    last = Instant::now();
                    emit(Progress {
                        phase: "download",
                        path: remote.path.to_string_lossy().into_owned(),
                        current,
                        total,
                        bytes_done: n,
                        bytes_total: entry.size,
                        overall_done: done + n,
                        overall_total: bytes,
                    });
                }
            })
            .await?;
            executable(&part)?;
            fs::rename(&part, &target).map_err(io)?;
            report.downloaded += 1;
            if resumed {
                report.resumed += 1;
            }
        }
        done += entry.size;
        emit(Progress {
            phase: if skipped { "skip" } else { "download" },
            path: remote.path.to_string_lossy().into_owned(),
            current,
            total,
            bytes_done: entry.size,
            bytes_total: entry.size,
            overall_done: done,
            overall_total: bytes,
        });
    }
    emit(Progress {
        phase: "verify",
        path: "正在复核网盘快照…".into(),
        current: total,
        total,
        bytes_done: 0,
        bytes_total: 0,
        overall_done: bytes,
        overall_total: bytes,
    });
    if snapshot(&enumerate(&client, &base, &config.directory).await?)? != revision {
        return Err("网盘目录在同步期间发生变化，请再次同步；当前资源保持不变".into());
    }
    // Re-verify the complete stage before publishing; no partially updated installers become visible.
    for remote in &entries {
        if !remote.entry.directory && !matches(&safe_path(&tree, &remote.path)?, &remote.entry) {
            return Err("暂存资源校验失败，请重新同步".into());
        }
    }
    let previous = work.join("previous");
    write_json(&work.join("publish.json"), &report)?;
    if destination.exists() {
        fs::rename(destination, &previous).map_err(io)?;
    }
    if let Err(error) = fs::rename(&tree, destination) {
        if previous.exists() {
            let _ = fs::rename(&previous, destination);
        }
        let _ = fs::remove_file(work.join("publish.json"));
        return Err(io(error));
    }
    write_json(&work.join("complete.json"), &report)?;
    fs::remove_file(work.join("publish.json")).map_err(io)?;
    // Cleanup failure leaves a safe backup; publication itself has already succeeded.
    if previous.exists() {
        let _ = fs::remove_dir_all(previous);
    }
    emit(Progress {
        phase: "complete",
        path: "网盘资源同步完成".into(),
        current: total,
        total,
        bytes_done: 0,
        bytes_total: 0,
        overall_done: bytes,
        overall_total: bytes,
    });
    Ok(report)
}
fn prune(root: &Path, dir: &Path, expected: &HashSet<PathBuf>) -> Result<(), String> {
    for item in fs::read_dir(dir).map_err(io)? {
        let item = item.map_err(io)?;
        let path = item.path();
        let kind = item.file_type().map_err(io)?;
        if kind.is_symlink() {
            return Err("暂存目录包含符号链接".into());
        }
        let rel = path.strip_prefix(root).map_err(|_| "暂存路径无效")?;
        if !expected.contains(rel) {
            if kind.is_dir() {
                fs::remove_dir_all(path).map_err(io)?;
            } else {
                fs::remove_file(path).map_err(io)?;
            }
        } else if kind.is_dir() {
            prune(root, &path, expected)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    struct Fixture {
        config: Config,
        entries: Arc<Mutex<std::collections::HashMap<String, Vec<Entry>>>>,
        files: Arc<std::collections::HashMap<String, Vec<u8>>>,
        mode: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<String>>>,
        listener: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.listener.abort();
        }
    }
    fn entry(id: char, name: &str, bytes: Option<&[u8]>) -> Entry {
        Entry {
            id: id.to_string().repeat(32),
            name: name.into(),
            directory: bytes.is_none(),
            size: bytes.map_or(0, |b| b.len() as u64),
            sha256: bytes.map(|b| format!("{:x}", Sha256::digest(b))),
        }
    }
    impl Fixture {
        async fn new() -> Self {
            let a = "a".repeat(32);
            let b = "b".repeat(32);
            let c = "c".repeat(32);
            let d = "d".repeat(32);
            let e = "e".repeat(32);
            let f = "f".repeat(32);
            let payload: Vec<_> = (0..16384).map(|n| (n % 251) as u8).collect();
            let script = b"#!/bin/sh\necho verified\n".to_vec();
            let entries = Arc::new(Mutex::new(std::collections::HashMap::from([
                (
                    a.clone(),
                    vec![
                        entry('b', "nested", None),
                        entry('c', "one.bin", Some(&payload)),
                        entry('d', "empty.txt", Some(&[])),
                    ],
                ),
                (
                    b,
                    vec![
                        entry('e', "install.sh", Some(&script)),
                        entry('f', "empty-dir", None),
                    ],
                ),
                (f, vec![]),
            ])));
            let files = Arc::new(std::collections::HashMap::from([
                (c, payload),
                (d, vec![]),
                (e, script),
            ]));
            let mode = Arc::new(AtomicUsize::new(0));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let downloads = Arc::new(AtomicUsize::new(0));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let es = entries.clone();
            let fs = files.clone();
            let md = mode.clone();
            let rq = requests.clone();
            let listener = tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let es = es.clone();
                    let fs = fs.clone();
                    let md = md.clone();
                    let rq = rq.clone();
                    let dl = downloads.clone();
                    tokio::spawn(async move {
                        let mut buffer = Vec::new();
                        let mut one = [0u8; 1];
                        while buffer.len() < 8192 {
                            if socket.read(&mut one).await.unwrap_or(0) == 0 {
                                return;
                            }
                            buffer.push(one[0]);
                            if buffer.ends_with(b"\r\n\r\n") {
                                break;
                            }
                        }
                        let text = String::from_utf8_lossy(&buffer);
                        let line = text.lines().next().unwrap_or("");
                        let path = line.split_whitespace().nth(1).unwrap_or("/");
                        rq.lock().unwrap().push(path.to_string());
                        let authorized = text
                            .lines()
                            .any(|l| l.to_ascii_lowercase() == "x-drive-token: test-read-key");
                        if !authorized {
                            let _=socket.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                            return;
                        }
                        let url = Url::parse(&format!("http://127.0.0.1{path}")).unwrap();
                        if url.path().ends_with("/entries") {
                            if md.load(Ordering::SeqCst) == 4 {
                                let _=socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: /leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                                return;
                            }
                            let query: std::collections::HashMap<_, _> =
                                url.query_pairs().into_owned().collect();
                            let parent = query.get("parentId").unwrap();
                            let page = query.get("page").unwrap().parse::<usize>().unwrap();
                            let all = es.lock().unwrap().get(parent).cloned().unwrap_or_default();
                            let items: Vec<_> =
                                all.iter().skip((page - 1) * 2).take(2).cloned().collect();
                            let body=serde_json::to_vec(&serde_json::json!({"code":200,"data":{"entries":items,"total":all.len(),"page":page,"pageSize":2}})).unwrap();
                            let header=format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len());
                            let _ = socket.write_all(header.as_bytes()).await;
                            let _ = socket.write_all(&body).await;
                            return;
                        }
                        let Some(id) = url
                            .path()
                            .strip_prefix("/api/v1/drive/files/")
                            .and_then(|p| p.strip_suffix("/content"))
                        else {
                            return;
                        };
                        let Some(body) = fs.get(id) else {
                            return;
                        };
                        let offset = text
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("range: bytes=")
                                    .map(str::to_string)
                            })
                            .and_then(|s| s.trim_end_matches('-').parse::<usize>().ok())
                            .unwrap_or(0);
                        let mode = md.load(Ordering::SeqCst);
                        let interrupt =
                            mode == 1 && body.len() >= 64 && dl.fetch_add(1, Ordering::SeqCst) == 0;
                        let offset = if mode == 3 { 0 } else { offset };
                        let range = offset > 0;
                        let start = if mode == 5 && range {
                            offset + 1
                        } else {
                            offset
                        };
                        let mut header = format!(
                            "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                            if range {
                                "206 Partial Content"
                            } else {
                                "200 OK"
                            },
                            body.len().saturating_sub(offset)
                        );
                        if range {
                            header.push_str(&format!(
                                "Content-Range: bytes {}-{}/{}\r\n",
                                start,
                                body.len() - 1,
                                body.len()
                            ));
                        }
                        header.push_str("\r\n");
                        let _ = socket.write_all(header.as_bytes()).await;
                        if interrupt {
                            let _ = socket.write_all(&body[..body.len().min(64)]).await;
                            let _ = socket.flush().await;
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            return;
                        }
                        if mode == 2 && !body.is_empty() {
                            let _ = socket.write_all(&vec![b'x'; body.len() - offset]).await;
                        } else {
                            let _ = socket.write_all(&body[offset..]).await;
                        }
                    });
                }
            });
            Self {
                config: Config {
                    server: format!("http://{address}"),
                    directory: a,
                    token: "test-read-key".into(),
                },
                entries,
                files,
                mode,
                requests,
                listener,
            }
        }
    }
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("drive-download-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn dest(&self) -> PathBuf {
            self.0.join("cangling-repo")
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[tokio::test]
    async fn paginated_verified_snapshot_reuses_existing_files_and_preserves_empty_dirs() {
        let fixture = Fixture::new().await;
        let tmp = Temp::new();
        let dest = tmp.dest();
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("obsolete"), b"old").unwrap();
        fs::write(dest.join("one.bin"), &fixture.files[&"c".repeat(32)]).unwrap();
        let report = synchronize(&fixture.config, &dest, |_| {}).await.unwrap();
        assert_eq!(report.total, 3);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.downloaded, 2);
        assert!(!dest.join("obsolete").exists());
        assert!(dest.join("nested/empty-dir").is_dir());
        assert!(completed(&dest));
        let second = synchronize(&fixture.config, &dest, |_| {}).await.unwrap();
        assert_eq!(second.downloaded, 0);
        assert_eq!(second.skipped, 3);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(
                fs::metadata(dest.join("nested/install.sh"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0
            );
        }
    }
    #[tokio::test]
    async fn process_cancellation_preserves_old_snapshot_and_resume_uses_range() {
        let fixture = Fixture::new().await;
        fixture.mode.store(1, Ordering::SeqCst);
        let tmp = Temp::new();
        let dest = tmp.dest();
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("old"), b"usable").unwrap();
        let config = fixture.config.clone();
        let target = dest.clone();
        let job = tokio::spawn(async move { synchronize(&config, &target, |_| {}).await });
        let work = workspace(&dest).unwrap();
        let mut partial = false;
        for _ in 0..300 {
            if fs::read_dir(work.join("parts")).ok().is_some_and(|items| {
                items
                    .flatten()
                    .any(|e| e.metadata().is_ok_and(|m| m.len() == 64))
            }) {
                partial = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(partial);
        job.abort();
        assert!(job.await.unwrap_err().is_cancelled());
        assert_eq!(fs::read(dest.join("old")).unwrap(), b"usable");
        fixture.mode.store(0, Ordering::SeqCst);
        let report = synchronize(&fixture.config, &dest, |_| {}).await.unwrap();
        assert!(report.resumed >= 1);
        assert_eq!(
            fs::read(dest.join("nested/install.sh")).unwrap(),
            fixture.files[&"e".repeat(32)]
        );
        assert!(completed(&dest));
    }
    #[tokio::test]
    async fn corrupt_download_never_replaces_current_resources() {
        let fixture = Fixture::new().await;
        fixture.mode.store(2, Ordering::SeqCst);
        let tmp = Temp::new();
        let dest = tmp.dest();
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("old"), b"usable").unwrap();
        assert!(synchronize(&fixture.config, &dest, |_| {}).await.is_err());
        assert_eq!(fs::read(dest.join("old")).unwrap(), b"usable");
        assert!(!completed(&dest));
    }
    #[tokio::test]
    async fn ignored_range_restarts_file_without_appending_and_bad_range_is_rejected() {
        let fixture = Fixture::new().await;
        let tmp = Temp::new();
        let part = tmp.0.join("partial");
        let entry = entry('c', "one.bin", Some(&fixture.files[&"c".repeat(32)]));
        let (client, url) = fixture.config.client().unwrap();
        fs::write(&part, &fixture.files[&"c".repeat(32)][..17]).unwrap();
        fixture.mode.store(3, Ordering::SeqCst);
        download(&client, &url, &entry, &part, |_| {})
            .await
            .unwrap();
        assert!(matches(&part, &entry));
        fs::write(&part, &fixture.files[&"c".repeat(32)][..17]).unwrap();
        fixture.mode.store(5, Ordering::SeqCst);
        assert!(download(&client, &url, &entry, &part, |_| {})
            .await
            .unwrap_err()
            .contains("范围"));
        assert_eq!(fs::metadata(part).unwrap().len(), 17);
    }
    #[tokio::test]
    async fn redirects_and_unsafe_remote_names_are_rejected() {
        let fixture = Fixture::new().await;
        fixture.mode.store(4, Ordering::SeqCst);
        let error = manifest(&fixture.config).await.err().unwrap();
        assert!(error.contains("302"));
        assert!(!error.contains(&fixture.config.token));
        assert!(!fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r == "/leak"));
        fixture.mode.store(0, Ordering::SeqCst);
        fixture
            .entries
            .lock()
            .unwrap()
            .get_mut(&fixture.config.directory)
            .unwrap()[0]
            .name = "../escape".into();
        assert!(manifest(&fixture.config).await.is_err());
    }
    #[tokio::test]
    async fn only_one_process_can_publish_and_pending_journal_recovers() {
        let fixture = Fixture::new().await;
        let tmp = Temp::new();
        let dest = tmp.dest();
        let work = workspace(&dest).unwrap();
        safe_dir(&work).unwrap();
        let lock = File::create(work.join("sync.lock")).unwrap();
        lock.lock_exclusive().unwrap();
        assert!(synchronize(&fixture.config, &dest, |_| {})
            .await
            .err()
            .unwrap()
            .contains("其他进程"));
        drop(lock);
        fs::create_dir(work.join("previous")).unwrap();
        fs::write(work.join("previous/old"), b"usable").unwrap();
        fs::create_dir(work.join("tree")).unwrap();
        write_json(&work.join("publish.json"), &Report::default()).unwrap();
        recover(&dest, &work).unwrap();
        assert_eq!(fs::read(dest.join("old")).unwrap(), b"usable");
        assert!(!work.join("publish.json").exists());
    }
    #[tokio::test]
    async fn changed_snapshot_is_not_published() {
        let fixture = Fixture::new().await;
        let tmp = Temp::new();
        let dest = tmp.dest();
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("old"), b"usable").unwrap();
        let entries = fixture.entries.clone();
        let id = fixture.config.directory.clone();
        let result = synchronize(&fixture.config, &dest, |progress| {
            if progress.phase == "verify" {
                entries.lock().unwrap().get_mut(&id).unwrap()[1].name = "renamed.bin".into();
            }
        })
        .await;
        assert!(result.err().unwrap().contains("变化"));
        assert_eq!(fs::read(dest.join("old")).unwrap(), b"usable");
    }
    #[tokio::test]
    #[ignore = "reads live directory metadata and a small ranged file using the configured default key"]
    async fn live_read_only_protocol() {
        let config = Config::default();
        let entries = manifest(&config).await.unwrap();
        let file = entries
            .iter()
            .find(|r| !r.entry.directory && r.entry.size > 8 && r.entry.size < 2048)
            .unwrap();
        let (client, url) = config.client().unwrap();
        let response = client
            .get(
                url.join(&format!("files/{}/content", file.entry.id))
                    .unwrap(),
            )
            .header(header::RANGE, "bytes=0-0")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let first = response.bytes().await.unwrap();
        assert_eq!(first.len(), 1);
        let tmp = Temp::new();
        let partial = tmp.0.join("live.part");
        fs::write(&partial, &first).unwrap();
        assert!(download(&client, &url, &file.entry, &partial, |_| {})
            .await
            .unwrap());
        assert!(matches(&partial, &file.entry));
    }
}
