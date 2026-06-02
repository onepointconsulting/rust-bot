//! Cron types.

use serde::{Deserialize, Serialize};

/// Schedule kind for a cron job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CronScheduleKind {
    At,
    Every,
    Cron,
}

/// Schedule definition for a cron job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronSchedule {
    pub kind: CronScheduleKind,
    /// For `"at"`: timestamp in ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<i64>,
    /// For `"every"`: interval in ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_ms: Option<i64>,
    /// For `"cron"`: cron expression (e.g. `"0 9 * * *"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,
    /// Timezone for cron expressions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
}

impl Default for CronSchedule {
    fn default() -> Self {
        Self {
            kind: CronScheduleKind::Every,
            at_ms: None,
            every_ms: None,
            expr: None,
            tz: None,
        }
    }
}

/// Payload kind for a cron job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronPayloadKind {
    SystemEvent,
    AgentTurn,
}

fn default_payload_kind() -> CronPayloadKind {
    CronPayloadKind::AgentTurn
}

/// What to do when the job runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronPayload {
    #[serde(default = "default_payload_kind")]
    pub kind: CronPayloadKind,
    #[serde(default)]
    pub message: String,
    /// Deliver response to channel.
    #[serde(default)]
    pub deliver: bool,
    /// e.g. `"whatsapp"`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// e.g. phone number
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

impl Default for CronPayload {
    fn default() -> Self {
        Self {
            kind: CronPayloadKind::AgentTurn,
            message: String::new(),
            deliver: false,
            channel: None,
            to: None,
        }
    }
}

/// Execution status for a single cron run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CronRunStatus {
    Ok,
    Error,
    Skipped,
}

/// A single execution record for a cron job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronRunRecord {
    pub run_at_ms: i64,
    pub status: CronRunStatus,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Runtime state of a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CronJobState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<CronRunStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub run_history: Vec<CronRunRecord>,
}

impl Default for CronJobState {
    fn default() -> Self {
        Self {
            next_run_at_ms: None,
            last_run_at_ms: None,
            last_status: None,
            last_error: None,
            run_history: Vec::new(),
        }
    }
}

/// A scheduled job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronJob {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub schedule: CronSchedule,
    #[serde(default)]
    pub payload: CronPayload,
    #[serde(default)]
    pub state: CronJobState,
    #[serde(default)]
    pub created_at_ms: i64,
    #[serde(default)]
    pub updated_at_ms: i64,
    #[serde(default)]
    pub delete_after_run: bool,
}

fn default_true() -> bool {
    true
}

/// Persistent store for cron jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CronStore {
    #[serde(default = "default_store_version")]
    pub version: i32,
    #[serde(default)]
    pub jobs: Vec<CronJob>,
}

fn default_store_version() -> i32 {
    1
}

impl Default for CronStore {
    fn default() -> Self {
        Self {
            version: 1,
            jobs: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_schedule_defaults_to_every() {
        let schedule = CronSchedule::default();
        assert_eq!(schedule.kind, CronScheduleKind::Every);
        assert!(schedule.at_ms.is_none());
    }

    #[test]
    fn cron_payload_defaults_to_agent_turn() {
        let payload = CronPayload::default();
        assert_eq!(payload.kind, CronPayloadKind::AgentTurn);
        assert!(payload.message.is_empty());
        assert!(!payload.deliver);
    }

    #[test]
    fn cron_store_defaults() {
        let store = CronStore::default();
        assert_eq!(store.version, 1);
        assert!(store.jobs.is_empty());
    }

    #[test]
    fn deserializes_nanobot_jobs_json_shape() {
        let json = r#"{
            "version": 1,
            "jobs": [{
                "id": "fc930569",
                "name": "hourly",
                "enabled": true,
                "schedule": {
                    "kind": "every",
                    "atMs": null,
                    "everyMs": 3600000,
                    "expr": null,
                    "tz": null
                },
                "payload": {
                    "kind": "agent_turn",
                    "message": "Check status",
                    "deliver": false,
                    "channel": null,
                    "to": null
                },
                "state": {
                    "nextRunAtMs": 1770454647636,
                    "lastRunAtMs": null,
                    "lastStatus": null,
                    "lastError": null,
                    "runHistory": [{
                        "runAtMs": 1770451047636,
                        "status": "ok",
                        "durationMs": 42,
                        "error": null
                    }]
                },
                "createdAtMs": 1770451047636,
                "updatedAtMs": 1770451047636,
                "deleteAfterRun": false
            }]
        }"#;

        let store: CronStore = serde_json::from_str(json).unwrap();
        assert_eq!(store.version, 1);
        assert_eq!(store.jobs.len(), 1);

        let job = &store.jobs[0];
        assert_eq!(job.id, "fc930569");
        assert_eq!(job.schedule.kind, CronScheduleKind::Every);
        assert_eq!(job.schedule.every_ms, Some(3_600_000));
        assert_eq!(job.payload.kind, CronPayloadKind::AgentTurn);
        assert_eq!(job.state.next_run_at_ms, Some(1770454647636));
        assert_eq!(job.state.run_history.len(), 1);
        assert_eq!(job.state.run_history[0].status, CronRunStatus::Ok);
        assert_eq!(job.state.run_history[0].duration_ms, 42);
    }
}
