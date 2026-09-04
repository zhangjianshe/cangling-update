use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JobProgress {
    pub id: String,
    pub phase: String,
    pub message: String,
    pub current: u64,
    pub total: u64,
    pub percent: u8,
    pub done: bool,
    pub error: Option<String>,
}

#[derive(Clone, Default)]
pub struct JobHub {
    inner: Arc<Mutex<HashMap<String, JobProgress>>>,
}

impl JobHub {
    pub fn create(&self) -> JobProgress {
        let id = Uuid::new_v4().to_string();
        let job = JobProgress {
            id: id.clone(),
            phase: "pending".into(),
            message: "等待开始".into(),
            current: 0,
            total: 0,
            percent: 0,
            done: false,
            error: None,
        };
        let mut map = self.inner.lock().expect("job map");
        if map.len() > 80 {
            map.retain(|_, j| !j.done);
        }
        map.insert(id, job.clone());
        job
    }

    pub fn get(&self, id: &str) -> Option<JobProgress> {
        self.inner.lock().expect("job map").get(id).cloned()
    }

    pub fn set(&self, id: &str, phase: &str, message: &str, current: u64, total: u64) {
        let mut map = self.inner.lock().expect("job map");
        let Some(job) = map.get_mut(id) else {
            return;
        };
        if job.done {
            return;
        }
        job.phase = phase.to_string();
        job.message = message.to_string();
        job.current = current;
        job.total = total;
        job.percent = if total > 0 {
            ((current.saturating_mul(100)) / total).min(100) as u8
        } else {
            job.percent
        };
    }

    pub fn finish_ok(&self, id: &str, message: &str) {
        let mut map = self.inner.lock().expect("job map");
        let Some(job) = map.get_mut(id) else {
            return;
        };
        job.phase = "done".into();
        job.message = message.to_string();
        job.percent = 100;
        job.done = true;
        job.error = None;
    }

    pub fn finish_err(&self, id: &str, error: &str) {
        let mut map = self.inner.lock().expect("job map");
        let Some(job) = map.get_mut(id) else {
            return;
        };
        job.phase = "error".into();
        job.message = error.to_string();
        job.done = true;
        job.error = Some(error.to_string());
    }
}
