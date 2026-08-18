//! Cron job store, timer loop, and public API (Python `CronService` parity).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use serde::Serialize;
use uuid::Uuid;

use crate::cron::service::{compute_next_run, now_ms, validate_schedule_for_add};
use crate::cron::{
    CronJob, CronJobState, CronPayload, CronPayloadKind, CronRunRecord, CronRunStatus,
    CronSchedule, CronScheduleKind, CronStore,
};

const MAX_RUN_HISTORY: usize = 20;

/// Async handler invoked when a job runs.
pub type CronJobCallback =
    Arc<dyn Fn(CronJob) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

/// Result of [`CronService::remove_job`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveJobResult {
    Removed,
    Protected,
    NotFound,
}

/// Service status snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CronServiceStatus {
    pub enabled: bool,
    pub jobs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_wake_at_ms: Option<i64>,
}

struct CronServiceInner {
    store_path: PathBuf,
    on_job: Option<CronJobCallback>,
    store: Option<CronStore>,
    last_mtime: Option<SystemTime>,
    timer_handle: Option<tokio::task::JoinHandle<()>>,
    running: bool,
}

/// Service for managing and executing scheduled jobs.
pub struct CronService {
    inner: Arc<tokio::sync::Mutex<CronServiceInner>>,
}

impl CronService {
    /// Create a new cron service. Returns `Arc` for timer spawning and shared ownership.
    pub fn new(store_path: PathBuf, on_job: Option<CronJobCallback>) -> Arc<Self> {
        Arc::new(CronService {
            inner: Arc::new(tokio::sync::Mutex::new(CronServiceInner {
                store_path,
                on_job,
                store: None,
                last_mtime: None,
                timer_handle: None,
                running: false,
            })),
        })
    }

    pub async fn set_on_job(self: &Arc<Self>, on_job: CronJobCallback) {
        let mut inner = self.inner.lock().await;
        inner.on_job = Some(on_job);
    }

    /// Start the cron service.
    pub async fn start(self: &Arc<Self>) {
        let mut inner = self.inner.lock().await;
        inner.running = true;
        drop(inner);

        self.load_store().await;
        self.recompute_next_runs().await;
        self.save_store().await;
        self.arm_timer_async().await;

        let count = self
            .inner
            .lock()
            .await
            .store
            .as_ref()
            .map(|s| s.jobs.len())
            .unwrap_or(0);
        log::info!("Cron service started with {count} jobs");
    }

    /// Stop the cron service.
    pub async fn stop(self: &Arc<Self>) {
        let mut inner = self.inner.lock().await;
        inner.running = false;
        if let Some(handle) = inner.timer_handle.take() {
            handle.abort();
        }
    }

    /// List jobs, optionally including disabled; sorted by next run time.
    pub async fn list_jobs(&self, include_disabled: bool) -> Vec<CronJob> {
        let mut inner = self.inner.lock().await;
        inner.reload_store_if_needed();
        let store = inner.store_ref();
        let mut jobs: Vec<CronJob> = store
            .jobs
            .iter()
            .filter(|j| include_disabled || j.enabled)
            .cloned()
            .collect();
        jobs.sort_by_key(|j| j.state.next_run_at_ms.unwrap_or(i64::MAX));
        jobs
    }

    /// Add a new job.
    pub async fn add_job(
        self: &Arc<Self>,
        name: impl Into<String>,
        schedule: CronSchedule,
        message: impl Into<String>,
        deliver: bool,
        channel: Option<String>,
        to: Option<String>,
        delete_after_run: bool,
    ) -> Result<CronJob, String> {
        validate_schedule_for_add(&schedule)?;

        let now = now_ms();
        let job = CronJob {
            id: Uuid::new_v4().to_string()[..8].to_string(),
            name: name.into(),
            enabled: true,
            schedule: schedule.clone(),
            payload: CronPayload {
                kind: CronPayloadKind::AgentTurn,
                message: message.into(),
                deliver,
                channel,
                to,
                ..Default::default()
            },
            state: CronJobState {
                next_run_at_ms: compute_next_run(&schedule, now),
                ..Default::default()
            },
            created_at_ms: now,
            updated_at_ms: now,
            delete_after_run,
        };

        {
            let mut inner = self.inner.lock().await;
            let store = inner.store_mut();
            store.jobs.push(job.clone());
        }

        self.save_store().await;
        self.schedule_arm_timer();
        log::info!("Cron: added job '{}' ({})", job.name, job.id);
        Ok(job)
    }

    /// Register an internal system job (idempotent on restart).
    pub async fn register_system_job(self: &Arc<Self>, mut job: CronJob) -> CronJob {
        let now = now_ms();
        job.state.next_run_at_ms = compute_next_run(&job.schedule, now);
        job.created_at_ms = now;
        job.updated_at_ms = now;

        {
            let mut inner = self.inner.lock().await;
            let store = inner.store_mut();
            store.jobs.retain(|j| j.id != job.id);
            store.jobs.push(job.clone());
        }

        self.save_store().await;
        self.schedule_arm_timer();
        log::info!("Cron: registered system job '{}' ({})", job.name, job.id);
        job
    }

    /// Remove a job by ID, unless it is a protected system job.
    pub async fn remove_job(self: &Arc<Self>, job_id: &str) -> RemoveJobResult {
        let mut inner = self.inner.lock().await;
        let store = inner.store_mut();

        let Some(job) = store.jobs.iter().find(|j| j.id == job_id) else {
            return RemoveJobResult::NotFound;
        };

        if job.payload.kind == CronPayloadKind::SystemEvent {
            log::info!("Cron: refused to remove protected system job {job_id}");
            return RemoveJobResult::Protected;
        }

        let before = store.jobs.len();
        store.jobs.retain(|j| j.id != job_id);
        if store.jobs.len() >= before {
            return RemoveJobResult::NotFound;
        }

        drop(inner);
        self.save_store().await;
        self.schedule_arm_timer();
        log::info!("Cron: removed job {job_id}");
        RemoveJobResult::Removed
    }

    /// Enable or disable a job.
    pub async fn enable_job(self: &Arc<Self>, job_id: &str, enabled: bool) -> Option<CronJob> {
        let mut inner = self.inner.lock().await;
        let store = inner.store_mut();
        let job = store.jobs.iter_mut().find(|j| j.id == job_id)?;
        job.enabled = enabled;
        job.updated_at_ms = now_ms();
        if enabled {
            job.state.next_run_at_ms = compute_next_run(&job.schedule, now_ms());
        } else {
            job.state.next_run_at_ms = None;
        }
        let job = job.clone();
        drop(inner);

        self.save_store().await;
        self.schedule_arm_timer();
        Some(job)
    }

    /// Manually run a job.
    pub async fn run_job(self: &Arc<Self>, job_id: &str, force: bool) -> bool {
        let should_run = {
            let mut inner = self.inner.lock().await;
            inner.reload_store_if_needed();
            let store = inner.store_ref();
            let Some(job) = store.jobs.iter().find(|j| j.id == job_id) else {
                return false;
            };
            force || job.enabled
        };

        if !should_run {
            return false;
        }

        self.execute_job(job_id).await;
        self.save_store().await;
        self.schedule_arm_timer();
        true
    }

    /// Get a job by ID.
    pub async fn get_job(&self, job_id: &str) -> Option<CronJob> {
        let mut inner = self.inner.lock().await;
        inner.reload_store_if_needed();
        let store = inner.store_ref();
        store.jobs.iter().find(|j| j.id == job_id).cloned()
    }

    /// Get service status.
    pub async fn status(&self) -> CronServiceStatus {
        let mut inner = self.inner.lock().await;
        inner.reload_store_if_needed();
        let store = inner.store_ref();
        CronServiceStatus {
            enabled: inner.running,
            jobs: store.jobs.len(),
            next_wake_at_ms: Self::get_next_wake_ms(store),
        }
    }

    async fn load_store(self: &Arc<Self>) {
        let mut inner = self.inner.lock().await;
        inner.reload_store_if_needed();
    }

    async fn save_store(self: &Arc<Self>) {
        let mut inner = self.inner.lock().await;
        inner.save_store_to_disk();
    }

    async fn recompute_next_runs(self: &Arc<Self>) {
        let now = now_ms();
        let mut inner = self.inner.lock().await;
        let store = inner.store_mut();
        for job in &mut store.jobs {
            if job.enabled {
                job.state.next_run_at_ms = compute_next_run(&job.schedule, now);
            }
        }
    }

    fn get_next_wake_ms(store: &CronStore) -> Option<i64> {
        store
            .jobs
            .iter()
            .filter(|j| j.enabled)
            .filter_map(|j| j.state.next_run_at_ms)
            .min()
    }

    async fn arm_timer_async(self: &Arc<Self>) {
        let (running, next_wake, delay_ms) = {
            let mut inner = self.inner.lock().await;
            if let Some(handle) = inner.timer_handle.take() {
                handle.abort();
            }

            let running = inner.running;
            let next_wake = inner.store.as_ref().and_then(|s| Self::get_next_wake_ms(s));

            let delay_ms = next_wake.map(|wake| (wake - now_ms()).max(0));
            (running, next_wake, delay_ms)
        };

        let Some(delay_ms) = delay_ms else {
            return;
        };
        if !running || next_wake.is_none() {
            return;
        }

        let weak = Arc::downgrade(self);
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms as u64)).await;
            if let Some(svc) = weak.upgrade() {
                svc.on_timer().await;
            }
        });

        self.inner.lock().await.timer_handle = Some(handle);
    }

    async fn on_timer(self: &Arc<Self>) {
        self.load_store().await;

        let due_ids: Vec<String> = {
            let inner = self.inner.lock().await;
            let store = match inner.store.as_ref() {
                Some(s) => s,
                None => return,
            };
            let now = now_ms();
            store
                .jobs
                .iter()
                .filter(|j| j.enabled && j.state.next_run_at_ms.is_some_and(|t| now >= t))
                .map(|j| j.id.clone())
                .collect()
        };

        for job_id in due_ids {
            self.execute_job(&job_id).await;
        }

        self.save_store().await;
        self.schedule_arm_timer();
    }

    /// Re-arm timer without awaiting (keeps timer tick future `Send`).
    fn schedule_arm_timer(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            if let Some(svc) = weak.upgrade() {
                svc.arm_timer_async().await;
            }
        });
    }

    async fn execute_job(self: &Arc<Self>, job_id: &str) {
        let start_ms = now_ms();

        let (on_job, job_name, job_id_log) = {
            let inner = self.inner.lock().await;
            let store = inner.store_ref();
            let Some(job) = store.jobs.iter().find(|j| j.id == job_id) else {
                return;
            };
            (inner.on_job.clone(), job.name.clone(), job.id.clone())
        };

        log::info!("Cron: executing job '{job_name}' ({job_id_log})");

        let job_snapshot = {
            let inner = self.inner.lock().await;
            let store = inner.store_ref();
            store.jobs.iter().find(|j| j.id == job_id).cloned()
        };

        let Some(job_snapshot) = job_snapshot else {
            return;
        };

        let (status, error_msg) = if let Some(ref callback) = on_job {
            match callback(job_snapshot).await {
                Ok(()) => {
                    log::info!("Cron: job '{job_name}' completed");
                    (CronRunStatus::Ok, None)
                }
                Err(e) => {
                    log::error!("Cron: job '{job_name}' failed: {e}");
                    (CronRunStatus::Error, Some(e))
                }
            }
        } else {
            log::info!("Cron: job '{job_name}' completed");
            (CronRunStatus::Ok, None)
        };

        let end_ms = now_ms();
        let duration_ms = end_ms - start_ms;

        let mut inner = self.inner.lock().await;
        let store = inner.store_mut();
        let Some(idx) = store.jobs.iter().position(|j| j.id == job_id) else {
            return;
        };

        let job = &mut store.jobs[idx];
        job.state.last_status = Some(status);
        job.state.last_error = error_msg.clone();
        job.state.last_run_at_ms = Some(start_ms);
        job.updated_at_ms = end_ms;

        job.state.run_history.push(CronRunRecord {
            run_at_ms: start_ms,
            status,
            duration_ms,
            error: error_msg,
        });
        if job.state.run_history.len() > MAX_RUN_HISTORY {
            let excess = job.state.run_history.len() - MAX_RUN_HISTORY;
            job.state.run_history.drain(0..excess);
        }

        let schedule_kind = job.schedule.kind;
        let delete_after_run = job.delete_after_run;
        let schedule = job.schedule.clone();

        if schedule_kind == CronScheduleKind::At {
            if delete_after_run {
                store.jobs.remove(idx);
            } else {
                let job = &mut store.jobs[idx];
                job.enabled = false;
                job.state.next_run_at_ms = None;
            }
        } else {
            let job = &mut store.jobs[idx];
            job.state.next_run_at_ms = compute_next_run(&schedule, now_ms());
        }
    }
}

impl CronServiceInner {
    fn store_ref(&self) -> &CronStore {
        self.store.as_ref().expect("store loaded")
    }

    fn store_mut(&mut self) -> &mut CronStore {
        self.reload_store_if_needed();
        self.store.get_or_insert_with(CronStore::default)
    }

    fn reload_store_if_needed(&mut self) {
        if self.store.is_some() && self.store_path.exists() {
            if let Ok(meta) = std::fs::metadata(&self.store_path) {
                if let Ok(mtime) = meta.modified() {
                    if self.last_mtime != Some(mtime) {
                        log::info!("Cron: jobs.json modified externally, reloading");
                        self.store = None;
                    }
                }
            }
        }

        if self.store.is_some() {
            return;
        }

        if self.store_path.exists() {
            match std::fs::read_to_string(&self.store_path) {
                Ok(text) => match serde_json::from_str::<CronStore>(&text) {
                    Ok(store) => {
                        self.store = Some(store);
                        if let Ok(meta) = std::fs::metadata(&self.store_path) {
                            if let Ok(mtime) = meta.modified() {
                                self.last_mtime = Some(mtime);
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("Failed to load cron store: {e}");
                        self.store = Some(CronStore::default());
                    }
                },
                Err(e) => {
                    log::warn!("Failed to read cron store: {e}");
                    self.store = Some(CronStore::default());
                }
            }
        } else {
            self.store = Some(CronStore::default());
        }
    }

    fn save_store_to_disk(&mut self) {
        let Some(store) = &self.store else {
            return;
        };

        if let Some(parent) = self.store_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::warn!("Failed to create cron store directory: {e}");
                return;
            }
        }

        let json = match serde_json::to_string_pretty(store) {
            Ok(j) => j,
            Err(e) => {
                log::warn!("Failed to serialize cron store: {e}");
                return;
            }
        };

        if let Err(e) = std::fs::write(&self.store_path, json) {
            log::warn!("Failed to write cron store: {e}");
            return;
        }

        if let Ok(meta) = std::fs::metadata(&self.store_path) {
            if let Ok(mtime) = meta.modified() {
                self.last_mtime = Some(mtime);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::TempDir;

    fn noop_callback() -> CronJobCallback {
        Arc::new(|_| Box::pin(async { Ok(()) }))
    }

    fn store_path(dir: &TempDir) -> PathBuf {
        dir.path().join("jobs.json")
    }

    #[tokio::test]
    async fn add_job_round_trips_to_disk() {
        let dir = TempDir::new().unwrap();
        let svc = CronService::new(store_path(&dir), None);
        let schedule = CronSchedule {
            kind: CronScheduleKind::Every,
            every_ms: Some(60_000),
            ..Default::default()
        };
        let job = svc
            .add_job("hourly", schedule, "ping", false, None, None, false)
            .await
            .unwrap();

        let text = std::fs::read_to_string(store_path(&dir)).unwrap();
        let store: CronStore = serde_json::from_str(&text).unwrap();
        assert_eq!(store.jobs.len(), 1);
        assert_eq!(store.jobs[0].id, job.id);
        assert_eq!(store.jobs[0].name, "hourly");
    }

    #[tokio::test]
    async fn external_file_change_is_reloaded() {
        let dir = TempDir::new().unwrap();
        let path = store_path(&dir);
        let svc = CronService::new(path.clone(), None);

        svc.add_job(
            "first",
            CronSchedule {
                kind: CronScheduleKind::Every,
                every_ms: Some(1_000),
                ..Default::default()
            },
            "a",
            false,
            None,
            None,
            false,
        )
        .await
        .unwrap();

        let other = CronStore {
            version: 1,
            jobs: vec![CronJob {
                id: "external".into(),
                name: "from_disk".into(),
                enabled: true,
                schedule: CronSchedule {
                    kind: CronScheduleKind::Every,
                    every_ms: Some(2_000),
                    ..Default::default()
                },
                payload: CronPayload::default(),
                state: CronJobState::default(),
                created_at_ms: 1,
                updated_at_ms: 1,
                delete_after_run: false,
            }],
        };
        std::fs::write(&path, serde_json::to_string_pretty(&other).unwrap()).unwrap();
        // Force mtime mismatch (avoids flaky 1s resolution on some filesystems).
        {
            let mut inner = svc.inner.lock().await;
            inner.last_mtime = Some(SystemTime::UNIX_EPOCH);
        }

        let jobs = svc.list_jobs(true).await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "external");
    }

    #[tokio::test]
    async fn remove_job_protects_system_event() {
        let dir = TempDir::new().unwrap();
        let svc = CronService::new(store_path(&dir), None);
        let job = CronJob {
            id: "sys1".into(),
            name: "heartbeat".into(),
            enabled: true,
            schedule: CronSchedule::default(),
            payload: CronPayload {
                kind: CronPayloadKind::SystemEvent,
                ..Default::default()
            },
            state: CronJobState::default(),
            created_at_ms: 0,
            updated_at_ms: 0,
            delete_after_run: false,
        };
        svc.register_system_job(job).await;
        assert_eq!(svc.remove_job("sys1").await, RemoveJobResult::Protected);
    }

    #[tokio::test]
    async fn at_job_delete_after_run_removes_job() {
        let dir = TempDir::new().unwrap();
        let svc = CronService::new(store_path(&dir), Some(noop_callback()));
        let now = now_ms();
        let job = svc
            .add_job(
                "once",
                CronSchedule {
                    kind: CronScheduleKind::At,
                    at_ms: Some(now - 1),
                    ..Default::default()
                },
                "go",
                false,
                None,
                None,
                true,
            )
            .await
            .unwrap();

        svc.execute_job(&job.id).await;
        svc.save_store().await;

        assert!(svc.get_job(&job.id).await.is_none());
    }

    #[tokio::test]
    async fn at_job_without_delete_disables_job() {
        let dir = TempDir::new().unwrap();
        let svc = CronService::new(store_path(&dir), Some(noop_callback()));
        let now = now_ms();
        let job = svc
            .add_job(
                "once",
                CronSchedule {
                    kind: CronScheduleKind::At,
                    at_ms: Some(now - 1),
                    ..Default::default()
                },
                "go",
                false,
                None,
                None,
                false,
            )
            .await
            .unwrap();

        svc.execute_job(&job.id).await;
        let updated = svc.get_job(&job.id).await.unwrap();
        assert!(!updated.enabled);
        assert!(updated.state.next_run_at_ms.is_none());
    }

    #[tokio::test]
    async fn run_history_capped_at_twenty() {
        let dir = TempDir::new().unwrap();
        let svc = CronService::new(store_path(&dir), Some(noop_callback()));
        let mut job = svc
            .add_job(
                "hist",
                CronSchedule {
                    kind: CronScheduleKind::Every,
                    every_ms: Some(1_000),
                    ..Default::default()
                },
                "x",
                false,
                None,
                None,
                false,
            )
            .await
            .unwrap();

        {
            let mut inner = svc.inner.lock().await;
            let store = inner.store_mut();
            let j = store.jobs.iter_mut().find(|x| x.id == job.id).unwrap();
            j.state.run_history = (0..25)
                .map(|i| CronRunRecord {
                    run_at_ms: i,
                    status: CronRunStatus::Ok,
                    duration_ms: 1,
                    error: None,
                })
                .collect();
        }

        svc.execute_job(&job.id).await;
        job = svc.get_job(&job.id).await.unwrap();
        assert_eq!(job.state.run_history.len(), MAX_RUN_HISTORY);
    }

    #[tokio::test]
    async fn start_recomputes_next_run() {
        let dir = TempDir::new().unwrap();
        let svc = CronService::new(store_path(&dir), None);
        svc.add_job(
            "every",
            CronSchedule {
                kind: CronScheduleKind::Every,
                every_ms: Some(5_000),
                ..Default::default()
            },
            "tick",
            false,
            None,
            None,
            false,
        )
        .await
        .unwrap();

        svc.start().await;
        let job = svc.list_jobs(false).await.pop().unwrap();
        assert!(job.state.next_run_at_ms.is_some());
        svc.stop().await;
    }

    #[tokio::test]
    async fn run_job_respects_enabled_and_force() {
        let dir = TempDir::new().unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        let runs_cb = Arc::clone(&runs);
        let callback: CronJobCallback = Arc::new(move |_| {
            let runs_cb = Arc::clone(&runs_cb);
            Box::pin(async move {
                runs_cb.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });

        let svc = CronService::new(store_path(&dir), Some(callback));
        let job = svc
            .add_job(
                "manual",
                CronSchedule {
                    kind: CronScheduleKind::Every,
                    every_ms: Some(60_000),
                    ..Default::default()
                },
                "x",
                false,
                None,
                None,
                false,
            )
            .await
            .unwrap();

        svc.enable_job(&job.id, false).await;
        assert!(!svc.run_job(&job.id, false).await);
        assert_eq!(runs.load(Ordering::SeqCst), 0);

        assert!(svc.run_job(&job.id, true).await);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }
}
