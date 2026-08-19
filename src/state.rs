use crate::docker::Docker;
use crate::paths::AppPaths;
use crate::progress::JobHub;
use rusqlite::Connection;
use std::collections::HashMap;
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
}

impl AppState {
    pub fn new(paths: AppPaths, db: Connection, docker: Docker, port: u16) -> Self {
        Self {
            paths,
            db: Arc::new(Mutex::new(db)),
            docker,
            locks: Arc::new(Mutex::new(HashMap::new())),
            jobs: JobHub::default(),
            port,
        }
    }

    pub fn lock_project(&self, id: &str) -> Arc<AsyncMutex<()>> {
        let mut map = self.locks.lock().expect("project lock map");
        map.entry(id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}
