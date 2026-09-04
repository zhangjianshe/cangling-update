use crate::cluster::ClusterConfig;
use crate::docker::Docker;
use crate::paths::AppPaths;
use crate::progress::JobHub;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Clone)]
pub struct AppState {
    pub paths: AppPaths,
    pub db: Arc<Mutex<Connection>>,
    pub docker: Docker,
    pub locks: Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    pub jobs: JobHub,
    pub port: u16,
    pub images_dir: PathBuf,
    pub image_import_lock: Arc<AsyncMutex<()>>,
    pub active_image_job: Arc<Mutex<Option<String>>>,
    pub login_guard: LoginGuard,
    pub cluster: ClusterConfig,
    /// worker 侧当前发现的 master 地址（无则 None），供仓库代理等复用。
    pub master_url: Arc<Mutex<Option<String>>>,
    /// 集群初始化进度。
    pub init: crate::cluster::init::InitState,
}

impl AppState {
    pub fn new(
        paths: AppPaths,
        db: Connection,
        docker: Docker,
        port: u16,
        cluster: ClusterConfig,
        images_dir: PathBuf,
    ) -> Self {
        Self {
            paths,
            db: Arc::new(Mutex::new(db)),
            docker,
            locks: Arc::new(Mutex::new(HashMap::new())),
            jobs: JobHub::default(),
            port,
            images_dir,
            image_import_lock: Arc::new(AsyncMutex::new(())),
            active_image_job: Arc::new(Mutex::new(None)),
            login_guard: LoginGuard::new(),
            master_url: Arc::new(Mutex::new(cluster.master_url.clone())),
            init: Arc::new(Mutex::new(crate::cluster::init::InitStatus::default())),
            cluster,
        }
    }

    pub fn lock_project(&self, id: &str) -> Arc<AsyncMutex<()>> {
        let mut map = self.locks.lock().expect("project lock map");
        map.entry(id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

const MAX_LOGIN_FAILURES: u32 = 3;
const LOGIN_LOCK_SECS: i64 = 3 * 60;

/// Tracks failed login attempts per username and enforces a short lockout.
#[derive(Clone, Default)]
pub struct LoginGuard {
    inner: Arc<Mutex<HashMap<String, LoginEntry>>>,
}

#[derive(Default)]
struct LoginEntry {
    failures: u32,
    locked_until: Option<DateTime<Utc>>,
}

impl LoginGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seconds remaining before the account can be tried again, if currently locked.
    pub fn lockout_remaining_secs(&self, username: &str) -> Option<i64> {
        let map = self.inner.lock().ok()?;
        let entry = map.get(username)?;
        let until = entry.locked_until?;
        let remaining = until.signed_duration_since(Utc::now()).num_seconds();
        (remaining > 0).then_some(remaining)
    }

    /// Records a failed attempt; locks the account once the threshold is reached.
    pub fn record_failure(&self, username: &str) {
        let Ok(mut map) = self.inner.lock() else {
            return;
        };
        let entry = map.entry(username.to_string()).or_default();
        let now = Utc::now();
        if let Some(until) = entry.locked_until {
            if now >= until {
                entry.locked_until = None;
                entry.failures = 0;
            }
        }
        entry.failures += 1;
        if entry.failures >= MAX_LOGIN_FAILURES {
            entry.locked_until = Some(now + chrono::TimeDelta::seconds(LOGIN_LOCK_SECS));
            entry.failures = 0;
        }
    }

    /// Clears any recorded failures (called after a successful login).
    pub fn clear(&self, username: &str) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(username);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_guard_locks_after_three_failures() {
        let guard = LoginGuard::new();
        assert!(guard.lockout_remaining_secs("admin").is_none());

        guard.record_failure("admin");
        guard.record_failure("admin");
        assert!(guard.lockout_remaining_secs("admin").is_none());

        guard.record_failure("admin");
        let remaining = guard
            .lockout_remaining_secs("admin")
            .expect("locked after 3 failures");
        assert!(
            remaining > 0 && remaining <= LOGIN_LOCK_SECS,
            "remaining = {remaining}"
        );

        guard.clear("admin");
        assert!(guard.lockout_remaining_secs("admin").is_none());
    }
}
