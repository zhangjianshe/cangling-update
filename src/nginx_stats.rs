use crate::error::AppError;
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Datelike, Local, Timelike};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS nginx_stats_settings (
  id INTEGER PRIMARY KEY CHECK(id=1), enabled INTEGER NOT NULL DEFAULT 0,
  log_dir TEXT NOT NULL DEFAULT '', last_scan_at TEXT NOT NULL DEFAULT '',
  last_status TEXT NOT NULL DEFAULT '', last_message TEXT NOT NULL DEFAULT ''
);
INSERT OR IGNORE INTO nginx_stats_settings(id) VALUES(1);
CREATE TABLE IF NOT EXISTS nginx_log_cursors (
  identity TEXT PRIMARY KEY, path TEXT NOT NULL, offset INTEGER NOT NULL DEFAULT 0,
  pending BLOB NOT NULL DEFAULT X'', updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS nginx_traffic_buckets (
  bucket_start TEXT PRIMARY KEY, requests INTEGER NOT NULL DEFAULT 0,
  response_bytes INTEGER NOT NULL DEFAULT 0, status_1xx INTEGER NOT NULL DEFAULT 0,
  status_2xx INTEGER NOT NULL DEFAULT 0, status_3xx INTEGER NOT NULL DEFAULT 0,
  status_4xx INTEGER NOT NULL DEFAULT 0, status_5xx INTEGER NOT NULL DEFAULT 0,
  request_time_sum REAL NOT NULL DEFAULT 0, request_time_count INTEGER NOT NULL DEFAULT 0,
  slow_requests INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS nginx_bucket_clients (
  bucket_start TEXT NOT NULL, client_ip TEXT NOT NULL,
  PRIMARY KEY(bucket_start, client_ip)
);
CREATE TABLE IF NOT EXISTS nginx_bucket_upstreams (
  bucket_start TEXT NOT NULL, upstream TEXT NOT NULL, requests INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(bucket_start, upstream)
);
CREATE TABLE IF NOT EXISTS nginx_bucket_paths (
  bucket_start TEXT NOT NULL, path TEXT NOT NULL, requests INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(bucket_start, path)
);
CREATE TABLE IF NOT EXISTS nginx_bucket_devices (
  bucket_start TEXT NOT NULL, platform TEXT NOT NULL, os_version TEXT NOT NULL,
  device_model TEXT NOT NULL, network_type TEXT NOT NULL,
  client_app TEXT NOT NULL, app_version TEXT NOT NULL,
  requests INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(bucket_start, platform, os_version, device_model, network_type, client_app, app_version)
);
DROP INDEX IF EXISTS idx_nginx_clients_bucket;
DROP INDEX IF EXISTS idx_nginx_upstreams_bucket;
DROP INDEX IF EXISTS idx_nginx_paths_bucket;
DROP INDEX IF EXISTS idx_nginx_devices_bucket;
"#;

const MAX_PATHS_PER_BUCKET: i64 = 500;

pub fn ensure_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/nginx-stats/settings",
            get(get_settings).put(save_settings),
        )
        .route("/api/nginx-stats/scan", post(scan_now))
        .route("/api/nginx-stats/report", get(report))
}

#[derive(Debug, Serialize)]
struct Settings {
    enabled: bool,
    log_dir: String,
    last_scan_at: String,
    last_status: String,
    last_message: String,
}

fn read_settings(conn: &Connection) -> rusqlite::Result<Settings> {
    conn.query_row("SELECT enabled,log_dir,last_scan_at,last_status,last_message FROM nginx_stats_settings WHERE id=1", [], |r| Ok(Settings { enabled:r.get::<_,i64>(0)? != 0, log_dir:r.get(1)?, last_scan_at:r.get(2)?, last_status:r.get(3)?, last_message:r.get(4)? }))
}

async fn get_settings(State(state): State<AppState>) -> Result<Json<Settings>, AppError> {
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    Ok(Json(read_settings(&conn)?))
}

#[derive(Debug, Deserialize)]
struct SaveSettings {
    enabled: bool,
    log_dir: String,
    #[serde(default)]
    start_from_end: bool,
}

async fn save_settings(
    State(state): State<AppState>,
    Json(body): Json<SaveSettings>,
) -> Result<Json<Settings>, AppError> {
    let dir = validate_log_dir(&body.log_dir)?;
    let now = chrono::Utc::now().to_rfc3339();
    {
        let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        conn.execute("UPDATE nginx_stats_settings SET enabled=?1,log_dir=?2,last_status='saved',last_message='',last_scan_at=?3 WHERE id=1", params![body.enabled as i64,dir.to_string_lossy(),now])?;
        if body.start_from_end {
            initialize_cursors_at_end(&conn, &dir)?;
        }
    }
    if body.enabled && !body.start_from_end {
        let scan_state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = run_scan(scan_state).await {
                tracing::error!("initial Nginx access log statistics failed: {error}");
            }
        });
    }
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    Ok(Json(read_settings(&conn)?))
}

async fn scan_now(State(state): State<AppState>) -> Result<Json<ScanResult>, AppError> {
    Ok(Json(run_scan(state).await?))
}

pub async fn scheduler(state: AppState) {
    let mut timer = tokio::time::interval(Duration::from_secs(30));
    let mut last_slot = String::new();
    loop {
        timer.tick().await;
        let now = Local::now();
        if !is_scheduled_minute(now.minute()) {
            continue;
        }
        let slot = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            now.year(),
            now.month(),
            now.day(),
            now.hour(),
            now.minute()
        );
        if slot == last_slot {
            continue;
        }
        last_slot = slot;
        let enabled = state
            .db
            .lock()
            .ok()
            .and_then(|c| read_settings(&c).ok())
            .is_some_and(|s| s.enabled && !s.log_dir.is_empty());
        if enabled {
            if let Err(err) = run_scan(state.clone()).await {
                tracing::error!("Nginx access log statistics failed: {err}");
            }
        }
    }
}

fn is_scheduled_minute(minute: u32) -> bool {
    minute == 15 || minute == 45
}

fn validate_log_dir(raw: &str) -> Result<PathBuf, AppError> {
    let p = PathBuf::from(raw.trim());
    if !p.is_absolute() {
        return Err(AppError::bad("Nginx 日志目录必须是绝对路径"));
    }
    if !p.is_dir() {
        return Err(AppError::bad(format!("日志目录不存在：{}", p.display())));
    }
    Ok(p.canonicalize().unwrap_or(p))
}

fn log_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let n = e.file_name().to_string_lossy().to_ascii_lowercase();
        if is_access_log_name(&n) {
            files.push(p);
        }
    }
    files.sort();
    Ok(files)
}

fn is_access_log_name(name: &str) -> bool {
    !name.ends_with(".gz")
        && (name == "access.log"
            || name.starts_with("access.log-")
            || name.starts_with("access.log."))
}

fn initialize_cursors_at_end(conn: &Connection, dir: &Path) -> Result<(), AppError> {
    for p in log_files(dir)? {
        let m = p.metadata()?;
        let id = format!("{}:{}", m.dev(), m.ino());
        conn.execute("INSERT INTO nginx_log_cursors(identity,path,offset,pending,updated_at) VALUES(?1,?2,?3,X'',?4) ON CONFLICT(identity) DO UPDATE SET path=excluded.path,offset=excluded.offset,pending=X'',updated_at=excluded.updated_at",params![id,p.to_string_lossy(),m.len() as i64,chrono::Utc::now().to_rfc3339()])?;
    }
    Ok(())
}

#[derive(Default)]
struct Agg {
    requests: i64,
    bytes: i64,
    status: [i64; 5],
    rt_sum: f64,
    rt_count: i64,
    slow: i64,
    clients: HashSet<String>,
    upstreams: HashMap<String, i64>,
    paths: HashMap<String, i64>,
    devices: HashMap<DeviceInfo, i64>,
}
#[derive(Debug, Serialize)]
struct ScanResult {
    files: usize,
    new_lines: u64,
    parsed_lines: u64,
    skipped_lines: u64,
    bytes_read: u64,
    finished_at: String,
}

async fn run_scan(state: AppState) -> Result<ScanResult, AppError> {
    let scan_lock = state.nginx_stats_lock.clone();
    let _guard = scan_lock.lock().await;
    let (dir, enabled) = {
        let c = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        let s = read_settings(&c)?;
        (PathBuf::from(s.log_dir), s.enabled)
    };
    if !enabled {
        return Err(AppError::bad("Nginx 日志统计尚未启用"));
    }
    let dir = validate_log_dir(&dir.to_string_lossy())?;
    let st = state.clone();
    tokio::task::spawn_blocking(move || scan_blocking(&st, &dir))
        .await
        .map_err(|e| AppError::internal(e.to_string()))?
}

fn scan_blocking(state: &AppState, dir: &Path) -> Result<ScanResult, AppError> {
    let files = log_files(dir)?;
    let mut result = ScanResult {
        files: files.len(),
        new_lines: 0,
        parsed_lines: 0,
        skipped_lines: 0,
        bytes_read: 0,
        finished_at: String::new(),
    };
    for p in files {
        scan_file(state, &p, &mut result)?;
    }
    result.finished_at = chrono::Utc::now().to_rfc3339();
    let conn = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    conn.execute("UPDATE nginx_stats_settings SET last_scan_at=?1,last_status='ok',last_message=?2 WHERE id=1",params![result.finished_at,format!("读取 {} 字节，新增 {} 条记录",result.bytes_read,result.parsed_lines)])?;
    Ok(result)
}

fn scan_file(state: &AppState, path: &Path, result: &mut ScanResult) -> Result<(), AppError> {
    let meta = path.metadata()?;
    let identity = format!("{}:{}", meta.dev(), meta.ino());
    let (mut offset, mut pending) = {
        let c = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
        c.query_row(
            "SELECT offset,pending FROM nginx_log_cursors WHERE identity=?1",
            [&identity],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?
        .unwrap_or((0, Vec::new()))
    };
    if meta.len() < offset {
        offset = 0;
        pending.clear();
    }
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut chunk = vec![0_u8; 8 * 1024 * 1024];
    loop {
        let read = f.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        offset += read as u64;
        result.bytes_read += read as u64;
        pending.extend_from_slice(&chunk[..read]);
        let Some(last_nl) = pending.iter().rposition(|b| *b == b'\n') else {
            persist_batch(state, &identity, path, offset, &pending, HashMap::new())?;
            continue;
        };
        let rest = pending.split_off(last_nl + 1);
        let mut aggs: HashMap<String, Agg> = HashMap::new();
        for raw in pending.split(|b| *b == b'\n').filter(|x| !x.is_empty()) {
            result.new_lines += 1;
            match std::str::from_utf8(raw).ok().and_then(parse_line) {
                Some(e) => {
                    result.parsed_lines += 1;
                    let a = aggs.entry(e.bucket).or_default();
                    a.requests += 1;
                    a.bytes += e.bytes;
                    a.status[(e.status / 100 - 1) as usize] += 1;
                    if let Some(rt) = e.request_time {
                        a.rt_sum += rt;
                        a.rt_count += 1;
                        if rt > 1.0 {
                            a.slow += 1
                        }
                    }
                    a.clients.insert(e.client);
                    *a.paths.entry(e.path).or_default() += 1;
                    *a.devices.entry(e.device).or_default() += 1;
                    if let Some(u) = e.upstream {
                        *a.upstreams.entry(u).or_default() += 1
                    }
                }
                None => result.skipped_lines += 1,
            }
        }
        pending = rest;
        persist_batch(state, &identity, path, offset, &pending, aggs)?;
    }
    Ok(())
}

fn persist_batch(
    state: &AppState,
    identity: &str,
    path: &Path,
    offset: u64,
    pending: &[u8],
    aggs: HashMap<String, Agg>,
) -> Result<(), AppError> {
    let mut c = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let tx = c.transaction()?;
    for (bucket, a) in aggs {
        tx.execute("INSERT INTO nginx_traffic_buckets(bucket_start,requests,response_bytes,status_1xx,status_2xx,status_3xx,status_4xx,status_5xx,request_time_sum,request_time_count,slow_requests) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT(bucket_start) DO UPDATE SET requests=requests+excluded.requests,response_bytes=response_bytes+excluded.response_bytes,status_1xx=status_1xx+excluded.status_1xx,status_2xx=status_2xx+excluded.status_2xx,status_3xx=status_3xx+excluded.status_3xx,status_4xx=status_4xx+excluded.status_4xx,status_5xx=status_5xx+excluded.status_5xx,request_time_sum=request_time_sum+excluded.request_time_sum,request_time_count=request_time_count+excluded.request_time_count,slow_requests=slow_requests+excluded.slow_requests",params![bucket,a.requests,a.bytes,a.status[0],a.status[1],a.status[2],a.status[3],a.status[4],a.rt_sum,a.rt_count,a.slow])?;
        for ip in a.clients {
            tx.execute(
                "INSERT OR IGNORE INTO nginx_bucket_clients(bucket_start,client_ip) VALUES(?1,?2)",
                params![bucket, ip],
            )?;
        }
        for (u, n) in a.upstreams {
            tx.execute("INSERT INTO nginx_bucket_upstreams(bucket_start,upstream,requests) VALUES(?1,?2,?3) ON CONFLICT(bucket_start,upstream) DO UPDATE SET requests=requests+excluded.requests",params![bucket,u,n])?;
        }
        for (p, n) in a.paths {
            tx.execute("INSERT INTO nginx_bucket_paths(bucket_start,path,requests) VALUES(?1,?2,?3) ON CONFLICT(bucket_start,path) DO UPDATE SET requests=requests+excluded.requests",params![bucket,p,n])?;
        }
        tx.execute(
            "DELETE FROM nginx_bucket_paths WHERE bucket_start=?1 AND path NOT IN (SELECT path FROM nginx_bucket_paths WHERE bucket_start=?1 ORDER BY requests DESC,path LIMIT ?2)",
            params![bucket, MAX_PATHS_PER_BUCKET],
        )?;
        for (device, n) in a.devices {
            tx.execute("INSERT INTO nginx_bucket_devices(bucket_start,platform,os_version,device_model,network_type,client_app,app_version,requests) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(bucket_start,platform,os_version,device_model,network_type,client_app,app_version) DO UPDATE SET requests=requests+excluded.requests",params![bucket,device.platform,device.os_version,device.device_model,device.network_type,device.client_app,device.app_version,n])?;
        }
    }
    tx.execute("INSERT INTO nginx_log_cursors(identity,path,offset,pending,updated_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(identity) DO UPDATE SET path=excluded.path,offset=excluded.offset,pending=excluded.pending,updated_at=excluded.updated_at",params![identity,path.to_string_lossy(),offset as i64,pending,chrono::Utc::now().to_rfc3339()])?;
    tx.commit()?;
    Ok(())
}

struct Event {
    bucket: String,
    status: i64,
    bytes: i64,
    client: String,
    path: String,
    upstream: Option<String>,
    request_time: Option<f64>,
    device: DeviceInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DeviceInfo {
    platform: String,
    os_version: String,
    device_model: String,
    network_type: String,
    client_app: String,
    app_version: String,
}

fn user_agent_value<'a>(quoted: &'a [&str]) -> &'a str {
    quoted.get(5).copied().unwrap_or("")
}

fn value_after<'a>(text: &'a str, marker: &str, endings: &[char]) -> Option<&'a str> {
    let start = text.find(marker)? + marker.len();
    let tail = &text[start..];
    let end = tail.find(endings).unwrap_or(tail.len());
    let value = tail[..end].trim();
    (!value.is_empty()).then_some(value)
}

fn classify_user_agent(user_agent: &str) -> DeviceInfo {
    let ua = user_agent.trim();
    let lower = ua.to_ascii_lowercase();
    let mut platform = "其他".to_string();
    let mut os_version = String::new();
    let mut device_model = String::new();

    if ua.is_empty() || ua == "-" {
        platform = "未知".to_string();
    } else if lower.contains("android") {
        platform = "Android".to_string();
        os_version = value_after(ua, "Android ", &[';', ')'])
            .unwrap_or("")
            .to_string();
        if let Some(android) = ua.find("Android ") {
            let tail = &ua[android..];
            if let Some(separator) = tail.find(';') {
                let candidate = tail[separator + 1..].split(';').next().unwrap_or("").trim();
                let candidate = candidate
                    .split(" Build/")
                    .next()
                    .unwrap_or(candidate)
                    .trim();
                if !candidate.is_empty() && candidate.len() <= 80 {
                    device_model = candidate.to_string();
                }
            }
        }
    } else if lower.contains("iphone") || lower.contains("ipad") || lower.contains("ipod") {
        platform = "Apple iOS/iPadOS".to_string();
        device_model = if lower.contains("ipad") {
            "iPad"
        } else if lower.contains("ipod") {
            "iPod"
        } else {
            "iPhone"
        }
        .to_string();
        os_version = value_after(ua, "CPU iPhone OS ", &[' '])
            .or_else(|| value_after(ua, "CPU OS ", &[' ']))
            .unwrap_or("")
            .replace('_', ".");
    } else if lower.contains("windows") {
        platform = "Windows".to_string();
    } else if lower.contains("macintosh") || lower.contains("mac os x") {
        platform = "macOS".to_string();
    } else if lower.contains("linux") {
        platform = "Linux".to_string();
    }

    let network_type = value_after(ua, "NetType/", &[' ', ';', ')'])
        .map(|v| v.to_ascii_uppercase())
        .unwrap_or_else(|| "未知".to_string());
    let (client_app, app_version) = if lower.contains("micromessenger/") {
        (
            if lower.contains("miniprogram") {
                "微信小程序"
            } else {
                "微信"
            }
            .to_string(),
            value_after(ua, "MicroMessenger/", &['(', ' '])
                .unwrap_or("")
                .to_string(),
        )
    } else {
        ("其他".to_string(), String::new())
    };
    DeviceInfo {
        platform,
        os_version,
        device_model,
        network_type,
        client_app,
        app_version,
    }
}

fn parse_line(line: &str) -> Option<Event> {
    let lb = line.find('[')?;
    let rb = line[lb..].find(']')? + lb;
    let dt = DateTime::parse_from_str(&line[lb + 1..rb], "%d/%b/%Y:%H:%M:%S %z").ok()?;
    let local = dt.with_timezone(&Local);
    let bucket = local
        .with_minute(if local.minute() < 30 { 0 } else { 30 })?
        .with_second(0)?
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string();
    let quoted: Vec<&str> = line.split('"').collect();
    if quoted.len() < 3 {
        return None;
    }
    let mut req = quoted[1].split_whitespace();
    let _ = req.next()?;
    let path = normalize_path(req.next()?.split('?').next()?);
    let mut tail = quoted[2].split_whitespace();
    let status = tail.next()?.parse::<i64>().ok()?;
    if !(100..600).contains(&status) {
        return None;
    }
    let bytes = tail.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let client = quoted
        .get(7)
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != "-")
        .unwrap_or_else(|| line.split_whitespace().next().unwrap_or("-"))
        .to_string();
    let suffix = quoted.last().copied().unwrap_or("");
    let upstream = token(suffix, "upstream=")
        .filter(|v| *v != "-")
        .map(str::to_string);
    let request_time = token(suffix, "rt=").and_then(|v| v.parse().ok());
    let device = classify_user_agent(user_agent_value(&quoted));
    Some(Event {
        bucket,
        status,
        bytes,
        client,
        path,
        upstream,
        request_time,
        device,
    })
}

fn normalize_path(path: &str) -> String {
    if !path.starts_with('/') {
        return "/invalid-request".to_string();
    }
    if path.starts_with("/ib/api/v1/map3/") {
        return "/ib/api/v1/map3/:tile".to_string();
    }
    let mut normalized = String::with_capacity(path.len().min(256));
    normalized.push('/');
    for (index, segment) in path.split('/').skip(1).enumerate() {
        if index > 0 {
            normalized.push('/');
        }
        normalized.push_str(&normalize_path_segment(segment));
        if normalized.len() >= 512 {
            return "/oversized-path".to_string();
        }
    }
    normalized
}

fn normalize_path_segment(segment: &str) -> String {
    if segment.is_empty() {
        return String::new();
    }
    if segment.bytes().all(|byte| byte.is_ascii_digit()) {
        return ":id".to_string();
    }
    if segment.contains(',')
        && segment
            .split(',')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return ":ids".to_string();
    }
    let (stem, suffix) = segment
        .split_once('.')
        .map(|(stem, suffix)| (stem, Some(suffix)))
        .unwrap_or((segment, None));
    if stem.bytes().all(|byte| byte.is_ascii_digit()) {
        return suffix
            .map(|suffix| format!(":id.{suffix}"))
            .unwrap_or_else(|| ":id".to_string());
    }
    let compact = stem.replace('-', "");
    let is_hash = compact.len() >= 16 && compact.bytes().all(|byte| byte.is_ascii_hexdigit());
    if is_hash {
        return suffix
            .map(|suffix| format!(":hash.{suffix}"))
            .unwrap_or_else(|| ":hash".to_string());
    }
    segment.to_string()
}
fn token<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let p = s.find(prefix)? + prefix.len();
    Some(&s[p..p + s[p..].find(char::is_whitespace).unwrap_or(s.len() - p)])
}

#[derive(Deserialize)]
struct ReportQuery {
    from: Option<String>,
    to: Option<String>,
}
#[derive(Serialize)]
struct Bucket {
    start: String,
    requests: i64,
    response_bytes: i64,
    status: [i64; 5],
    unique_clients: i64,
    avg_request_time: f64,
    slow_requests: i64,
}
#[derive(Serialize)]
struct NamedCount {
    name: String,
    requests: i64,
}
fn top_named(values: HashMap<String, i64>, limit: usize, include_empty: bool) -> Vec<NamedCount> {
    let mut values = values
        .into_iter()
        .filter(|(name, _)| include_empty || !name.is_empty())
        .map(|(name, requests)| NamedCount { name, requests })
        .collect::<Vec<_>>();
    values.sort_unstable_by(|a, b| b.requests.cmp(&a.requests));
    values.truncate(limit);
    values
}
#[derive(Serialize)]
struct Totals {
    requests: i64,
    response_bytes: i64,
    unique_clients: i64,
    status: [i64; 5],
    avg_request_time: f64,
    slow_requests: i64,
}
#[derive(Serialize)]
struct Report {
    from: String,
    to: String,
    totals: Totals,
    buckets: Vec<Bucket>,
    upstreams: Vec<NamedCount>,
    paths: Vec<NamedCount>,
    platforms: Vec<NamedCount>,
    os_versions: Vec<NamedCount>,
    networks: Vec<NamedCount>,
    device_models: Vec<NamedCount>,
    android_models: Vec<NamedCount>,
    android_model_requests: i64,
    android_model_count: i64,
    client_apps: Vec<NamedCount>,
    diagnostics: ReportDiagnostics,
}
#[derive(Serialize)]
struct ReportDiagnostics {
    database_bytes: u64,
    bucket_rows: i64,
    client_rows: i64,
    device_rows: i64,
    lock_wait_ms: u128,
    query_ms: u128,
}
async fn report(
    State(state): State<AppState>,
    Query(q): Query<ReportQuery>,
) -> Result<Json<Report>, AppError> {
    let today = Local::now().format("%Y-%m-%d").to_string();
    let from = q.from.unwrap_or_else(|| format!("{today}T00:00:00"));
    let to = q.to.unwrap_or_else(|| format!("{today}T23:59:59"));
    let started = Instant::now();
    let c = state.db.lock().map_err(|_| AppError::internal("db lock"))?;
    let lock_wait_ms = started.elapsed().as_millis();
    let mut st=c.prepare("SELECT b.bucket_start,b.requests,b.response_bytes,b.status_1xx,b.status_2xx,b.status_3xx,b.status_4xx,b.status_5xx,(SELECT COUNT(*) FROM nginx_bucket_clients x WHERE x.bucket_start=b.bucket_start),CASE WHEN b.request_time_count=0 THEN 0 ELSE b.request_time_sum/b.request_time_count END,b.slow_requests FROM nginx_traffic_buckets b WHERE b.bucket_start>=?1 AND b.bucket_start<=?2 ORDER BY b.bucket_start")?;
    let buckets = st
        .query_map(params![from, to], |r| {
            Ok(Bucket {
                start: r.get(0)?,
                requests: r.get(1)?,
                response_bytes: r.get(2)?,
                status: [r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?],
                unique_clients: r.get(8)?,
                avg_request_time: r.get(9)?,
                slow_requests: r.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let grouped = |table: &str, column: &str, limit: i64| -> Result<Vec<NamedCount>, AppError> {
        let sql=format!("SELECT {column},SUM(requests) n FROM {table} WHERE bucket_start>=?1 AND bucket_start<=?2 GROUP BY {column} ORDER BY n DESC LIMIT {limit}");
        let mut s = c.prepare(&sql)?;
        let rows = s.query_map(params![from, to], |r| {
            Ok(NamedCount {
                name: r.get(0)?,
                requests: r.get(1)?,
            })
        })?;
        let values = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(values)
    };
    let (client_rows, unique_clients) = c.query_row(
        "SELECT COUNT(*),COUNT(DISTINCT client_ip) FROM nginx_bucket_clients WHERE bucket_start>=?1 AND bucket_start<=?2",
        params![from, to],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let totals=c.query_row("SELECT COALESCE(SUM(requests),0),COALESCE(SUM(response_bytes),0),COALESCE(SUM(status_1xx),0),COALESCE(SUM(status_2xx),0),COALESCE(SUM(status_3xx),0),COALESCE(SUM(status_4xx),0),COALESCE(SUM(status_5xx),0),CASE WHEN COALESCE(SUM(request_time_count),0)=0 THEN 0 ELSE SUM(request_time_sum)/SUM(request_time_count) END,COALESCE(SUM(slow_requests),0) FROM nginx_traffic_buckets WHERE bucket_start>=?1 AND bucket_start<=?2",params![from,to],|r|Ok(Totals{requests:r.get(0)?,response_bytes:r.get(1)?,status:[r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?],avg_request_time:r.get(7)?,slow_requests:r.get(8)?,unique_clients}))?;
    let mut platform_counts = HashMap::new();
    let mut os_version_counts = HashMap::new();
    let mut network_counts = HashMap::new();
    let mut device_model_counts = HashMap::new();
    let mut android_model_counts = HashMap::new();
    let mut client_app_counts = HashMap::new();
    let mut device_rows = 0_i64;
    let mut statement = c.prepare("SELECT platform,os_version,device_model,network_type,client_app,requests FROM nginx_bucket_devices WHERE bucket_start>=?1 AND bucket_start<=?2")?;
    let rows = statement.query_map(params![from, to], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    for row in rows {
        let (platform, os_version, device_model, network, client_app, requests) = row?;
        device_rows += 1;
        *platform_counts.entry(platform.clone()).or_default() += requests;
        *os_version_counts.entry(os_version).or_default() += requests;
        *network_counts.entry(network).or_default() += requests;
        *device_model_counts.entry(device_model.clone()).or_default() += requests;
        *client_app_counts.entry(client_app).or_default() += requests;
        if platform == "Android" && !device_model.is_empty() {
            *android_model_counts.entry(device_model).or_default() += requests;
        }
    }
    let android_model_count = android_model_counts.len() as i64;
    let android_model_requests = android_model_counts.values().sum();
    let platforms = top_named(platform_counts, 20, true);
    let os_versions = top_named(os_version_counts, 30, false);
    let networks = top_named(network_counts, 20, true);
    let device_models = top_named(device_model_counts, 20, false);
    let android_models = top_named(android_model_counts, 7, false);
    let client_apps = top_named(client_app_counts, 20, true);
    let database_bytes = std::fs::metadata(&state.paths.db_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let bucket_rows = buckets.len() as i64;
    let upstreams = grouped("nginx_bucket_upstreams", "upstream", 50)?;
    let paths = grouped("nginx_bucket_paths", "path", 20)?;
    let query_ms = started.elapsed().as_millis();
    Ok(Json(Report {
        from: from.clone(),
        to: to.clone(),
        totals,
        buckets,
        upstreams,
        paths,
        platforms,
        os_versions,
        networks,
        device_models,
        android_models,
        android_model_requests,
        android_model_count,
        client_apps,
        diagnostics: ReportDiagnostics {
            database_bytes,
            bucket_rows,
            client_rows,
            device_rows,
            lock_wait_ms,
            query_ms,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_combined_with_upstream() {
        let l = r#"182.18.90.194 - - [09/Oct/2026:05:35:48 +0000] \"GET /app/a?q=1 HTTP/1.1\" 200 143 \"-\" \"ua\" \"223.1.1.1, 172.22.0.1\" upstream=10.132.5.223:7600 cache=- rt=0.242 urt=0.242"#;
        let e = parse_line(l).unwrap();
        assert_eq!(e.path, "/app/a");
        assert_eq!(e.client, "223.1.1.1");
        assert_eq!(e.upstream.as_deref(), Some("10.132.5.223:7600"));
        assert_eq!(e.bytes, 143);
        assert_eq!(e.device.platform, "其他");
    }

    #[test]
    fn classifies_android_user_agent() {
        let ua = "Mozilla/5.0 (Linux; Android 16; PLY110 Build/BP2A.250605.015; wv) AppleWebKit/537.36 MicroMessenger/8.0 NetType/5G Language/zh_CN miniProgram/wx123";
        let device = classify_user_agent(ua);
        assert_eq!(device.platform, "Android");
        assert_eq!(device.os_version, "16");
        assert_eq!(device.device_model, "PLY110");
        assert_eq!(device.network_type, "5G");
        assert_eq!(device.client_app, "微信小程序");
        assert_eq!(device.app_version, "8.0");
    }

    #[test]
    fn classifies_apple_user_agent() {
        let ua = "Mozilla/5.0 (iPad; CPU OS 16_5 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148 MicroMessenger/8.0 NetType/WIFI";
        let device = classify_user_agent(ua);
        assert_eq!(device.platform, "Apple iOS/iPadOS");
        assert_eq!(device.os_version, "16.5");
        assert_eq!(device.device_model, "iPad");
        assert_eq!(device.network_type, "WIFI");
    }

    #[test]
    fn normalizes_high_cardinality_paths() {
        assert_eq!(
            normalize_path("/ib/api/v1/map3/0748e9a1c92edd5c59cedbcda66ba08c/13/6784/3273.png"),
            "/ib/api/v1/map3/:tile"
        );
        assert_eq!(
            normalize_path("/app/out-check-polygon/30581657"),
            "/app/out-check-polygon/:id"
        );
        assert_eq!(
            normalize_path("/cms/v1/staff/select/19086,19085,19084"),
            "/cms/v1/staff/select/:ids"
        );
        assert_eq!(
            normalize_path("/ib/cangling/4307D9F41295E4125325A2B1AF1C502B.cache.js"),
            "/ib/cangling/:hash.cache.js"
        );
        assert_eq!(
            normalize_path("/app/out-check-polygon/feature/getFeaturesByTaskId"),
            "/app/out-check-polygon/feature/getFeaturesByTaskId"
        );
        assert_eq!(normalize_path("not-http"), "/invalid-request");
    }

    #[test]
    fn access_log_names_include_uncompressed_rotations_only() {
        assert!(is_access_log_name("access.log"));
        assert!(is_access_log_name("access.log-20261009-170101"));
        assert!(is_access_log_name("access.log.1"));
        assert!(!is_access_log_name("access.log-20261008.gz"));
        assert!(!is_access_log_name("monitor_access.log"));
        assert!(!is_access_log_name("error.log"));
    }

    #[test]
    fn scheduler_runs_at_quarter_past_and_quarter_to() {
        assert!(is_scheduled_minute(15));
        assert!(is_scheduled_minute(45));
        assert!(!is_scheduled_minute(0));
        assert!(!is_scheduled_minute(30));
    }
}
