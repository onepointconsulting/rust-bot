use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::cron_context::{self, CronContextToken};
use crate::agent::tools::base::Tool;
use crate::cron::{
    CronJob, CronJobState, CronPayloadKind, CronRunStatus, CronSchedule, CronScheduleKind, CronService,
    RemoveJobResult,
};

/// Tool to schedule reminders and recurring tasks.
pub struct CronTool {
    name: String,
    description: String,
    cron_service: Arc<CronService>,
    default_timezone: String,
    channel: Mutex<String>,
    chat_id: Mutex<String>,
}

impl CronTool {
    pub fn new(cron_service: Arc<CronService>, default_timezone: impl Into<String>) -> Self {
        let default_timezone = default_timezone.into();
        Self {
            name: "cron".to_string(),
            description: format!(
                "Schedule reminders and recurring tasks. Actions: add, list, remove. \
                 If tz is omitted, cron expressions and naive ISO times default to {default_timezone}."
            ),
            cron_service,
            default_timezone,
            channel: Mutex::new(String::new()),
            chat_id: Mutex::new(String::new()),
        }
    }

    /// Set the current session context for delivery.
    pub fn set_context(&self, channel: &str, chat_id: &str) {
        *self.channel.lock().unwrap() = channel.to_string();
        *self.chat_id.lock().unwrap() = chat_id.to_string();
    }

    /// Whether the tool is executing inside a cron job callback.
    pub fn in_cron_context(&self) -> bool {
        cron_context::in_cron_context()
    }

    /// Mark whether the tool is executing inside a cron job callback.
    pub fn set_cron_context(&self, active: bool) -> CronContextToken {
        cron_context::set_cron_context(active)
    }

    /// Restore previous cron context.
    pub fn reset_cron_context(&self, token: CronContextToken) {
        cron_context::reset_cron_context(token);
    }

    /// Returns an error string if `tz` is invalid, else `None` (Python `_validate_timezone`).
    fn validate_timezone(tz: &str) -> Option<String> {
        crate::cron::validate_timezone(tz)
    }

    fn display_timezone(&self, schedule: &CronSchedule) -> String {
        schedule
            .tz
            .clone()
            .unwrap_or_else(|| self.default_timezone.clone())
    }

    /// Format a Unix timestamp in ms for display (Python `_format_timestamp`).
    pub fn format_timestamp(ms: i64, tz_name: &str) -> Result<String, String> {
        crate::cron::format_timestamp(ms, tz_name)
    }

    /// Add a scheduled job (Python `_add_job`).
    pub async fn add_job(
        &self,
        name: Option<&str>,
        message: &str,
        every_seconds: Option<i64>,
        cron_expr: Option<&str>,
        tz: Option<&str>,
        at: Option<&str>,
        deliver: bool,
    ) -> String {
        if message.is_empty() {
            return "Error: message is required for add".to_string();
        }

        let channel = self.channel.lock().unwrap().clone();
        let chat_id = self.chat_id.lock().unwrap().clone();
        if channel.is_empty() || chat_id.is_empty() {
            return "Error: no session context (channel/chat_id)".to_string();
        }

        if tz.is_some() && cron_expr.is_none() {
            return "Error: tz can only be used with cron_expr".to_string();
        }

        if let Some(tz_name) = tz {
            if let Some(err) = Self::validate_timezone(tz_name) {
                return err;
            }
        }

        let mut delete_after = false;
        let schedule = if let Some(secs) = every_seconds {
            if secs <= 0 {
                return "Error: either every_seconds, cron_expr, or at is required".to_string();
            }
            CronSchedule {
                kind: CronScheduleKind::Every,
                every_ms: Some(secs * 1000),
                ..Default::default()
            }
        } else if let Some(expr) = cron_expr {
            let effective_tz = tz
                .map(str::to_string)
                .unwrap_or_else(|| self.default_timezone.clone());
            if let Some(err) = Self::validate_timezone(&effective_tz) {
                return err;
            }
            CronSchedule {
                kind: CronScheduleKind::Cron,
                expr: Some(expr.to_string()),
                tz: Some(effective_tz),
                ..Default::default()
            }
        } else if let Some(at_str) = at {
            let at_ms = match crate::cron::parse_at_iso(at_str, &self.default_timezone) {
                Ok(ms) => ms,
                Err(e) => return e,
            };
            delete_after = true;
            CronSchedule {
                kind: CronScheduleKind::At,
                at_ms: Some(at_ms),
                ..Default::default()
            }
        } else {
            return "Error: either every_seconds, cron_expr, or at is required".to_string();
        };

        let job_name = name
            .map(str::to_string)
            .unwrap_or_else(|| message.chars().take(30).collect::<String>());

        match self
            .cron_service
            .add_job(
                job_name,
                schedule,
                message,
                deliver,
                Some(channel),
                Some(chat_id),
                delete_after,
            )
            .await
        {
            Ok(job) => format!("Created job '{}' (id: {})", job.name, job.id),
            Err(e) => format!("Error: {e}"),
        }
    }

    /// Format schedule as a human-readable timing string (Python `_format_timing`).
    fn format_timing(&self, schedule: &CronSchedule) -> String {
        match schedule.kind {
            CronScheduleKind::Cron => {
                let expr = schedule.expr.as_deref().unwrap_or("");
                let tz_suffix = schedule
                    .tz
                    .as_deref()
                    .map(|tz| format!(" ({tz})"))
                    .unwrap_or_default();
                format!("cron: {expr}{tz_suffix}")
            }
            CronScheduleKind::Every => {
                let Some(ms) = schedule.every_ms.filter(|&ms| ms > 0) else {
                    return "every (invalid)".to_string();
                };
                if ms % 3_600_000 == 0 {
                    format!("every {}h", ms / 3_600_000)
                } else if ms % 60_000 == 0 {
                    format!("every {}m", ms / 60_000)
                } else if ms % 1_000 == 0 {
                    format!("every {}s", ms / 1_000)
                } else {
                    format!("every {ms}ms")
                }
            }
            CronScheduleKind::At => {
                let Some(at_ms) = schedule.at_ms else {
                    return "at (not scheduled)".to_string();
                };
                let tz = self.display_timezone(schedule);
                match CronTool::format_timestamp(at_ms, &tz) {
                    Ok(ts) => format!("at {ts}"),
                    Err(e) => format!("at (invalid: {e})"),
                }
            }
        }
    }

    fn format_timestamp_display(&self, ms: i64, schedule: &CronSchedule) -> String {
        let tz = self.display_timezone(schedule);
        CronTool::format_timestamp(ms, &tz).unwrap_or_else(|e| format!("(invalid: {e})"))
    }

    fn run_status_label(status: CronRunStatus) -> &'static str {
        match status {
            CronRunStatus::Ok => "ok",
            CronRunStatus::Error => "error",
            CronRunStatus::Skipped => "skipped",
        }
    }

    /// Format job run state as display lines.
    fn format_state(&self, state: &CronJobState, schedule: &CronSchedule) -> Vec<String> {
        let mut lines = Vec::new();

        if let Some(last_run_at_ms) = state.last_run_at_ms {
            let status = state
                .last_status
                .map(Self::run_status_label)
                .unwrap_or("unknown");
            let ts = self.format_timestamp_display(last_run_at_ms, schedule);
            let mut info = format!("  Last run: {ts} — {status}");
            if let Some(err) = state.last_error.as_deref().filter(|e| !e.is_empty()) {
                info.push_str(&format!(" ({err})"));
            }
            lines.push(info);
        }

        if let Some(next_ms) = state.next_run_at_ms {
            let ts = self.format_timestamp_display(next_ms, schedule);
            lines.push(format!("  Next run: {ts}"));
        }

        lines
    }

    fn system_job_purpose(job: &CronJob) -> String {
        if job.name == "dream" {
            "Dream memory consolidation for long-term memory.".to_string()
        } else {
            "System-managed internal job.".to_string()
        }
    }

    async fn list_jobs(&self) -> String {
        let jobs = self.cron_service.list_jobs(false).await;
        if jobs.is_empty() {
            return "No scheduled jobs.".to_string();
        }
        let mut lines = Vec::new();
        for j in jobs {
            let timing = self.format_timing(&j.schedule);
            let mut parts = vec![format!("- {} (id: {}, {timing})", j.name, j.id)];
            if j.payload.kind == CronPayloadKind::SystemEvent {
                parts.push(format!("  Purpose: {}", Self::system_job_purpose(&j)));
                parts.push("  Protected: visible for inspection, but cannot be removed.".to_string());
            }
            parts.extend(self.format_state(&j.state, &j.schedule));
            lines.push(parts.join("\n"));
        }
        format!("Scheduled jobs:\n{}", lines.join("\n"))
    }

    async fn remove_job(&self, job_id: &str) -> String {
        if job_id.is_empty() {
            return "Error: job_id is required for remove".to_string();
        }
        match self.cron_service.remove_job(job_id).await {
            RemoveJobResult::Removed => format!("Removed job '{job_id}'"),
            RemoveJobResult::NotFound => format!("Error: job '{job_id}' not found"),
            RemoveJobResult::Protected => {
                format!("Error: job '{job_id}' is protected and cannot be removed")
            }
        }
    }
}

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn set_tool_context(&self, channel: &str, chat_id: &str, _message_id: Option<&str>) {
        self.set_context(channel, chat_id);
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "list", "remove"],
                    "description": "Action to perform: add (schedule), list (show jobs), remove (delete by job_id)",
                },
                "name": {
                    "type": "string",
                    "description": "Optional short label for add (e.g. 'daily-standup'). Defaults to first 30 chars of message.",
                },
                "message": {
                    "type": "string",
                    "description": "Required for add: agent instruction when the job runs",
                },
                "every_seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "For add: run every N seconds (mutually exclusive with cron_expr and at)",
                },
                "cron_expr": {
                    "type": "string",
                    "description": "For add: 5-field cron expression (e.g. '0 9 * * *')",
                },
                "tz": {
                    "type": "string",
                    "description": "For add with cron_expr: optional IANA timezone (e.g. 'America/Vancouver'). \
                        If omitted, cron uses the tool default timezone.",
                },
                "at": {
                    "type": "string",
                    "description": "For add: one-shot ISO datetime (e.g. '2026-02-12T10:30:00'). \
                        Naive datetimes use the tool default timezone.",
                },
                "deliver": {
                    "type": "boolean",
                    "description": "For add: deliver result to the session channel (default true)",
                    "default": true,
                },
                "job_id": {
                    "type": "string",
                    "description": "For remove: job ID to delete",
                },
            },
            "required": ["action"],
        })
    }

    async fn execute(&self, params: &Value) -> String {
        let action = params.get("action").and_then(Value::as_str).unwrap_or("");
        match action {
            "add" => {
                if self.in_cron_context() {
                    return "Error: cannot add job inside a cron job context".to_string();
                }
                let message = params.get("message").and_then(Value::as_str).unwrap_or("");
                let deliver = params
                    .get("deliver")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                self.add_job(
                    params.get("name").and_then(Value::as_str),
                    message,
                    params.get("every_seconds").and_then(Value::as_i64),
                    params.get("cron_expr").and_then(Value::as_str),
                    params.get("tz").and_then(Value::as_str),
                    params.get("at").and_then(Value::as_str),
                    deliver,
                )
                .await
            }
            "list" => self.list_jobs().await,
            "remove" => {
                let job_id = params.get("job_id").and_then(Value::as_str).unwrap_or("");
                self.remove_job(job_id).await
            }
            _ => format!("Error: unknown or unsupported action '{action}'"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::cron_context::with_cron_context_stack;
    use tempfile::TempDir;

    #[test]
    fn validate_timezone_accepts_utc() {
        assert!(CronTool::validate_timezone("UTC").is_none());
    }

    #[test]
    fn format_timestamp_utc() {
        use chrono::{TimeZone, Utc};

        let ms = Utc
            .with_ymd_and_hms(2024, 1, 15, 9, 0, 0)
            .unwrap()
            .timestamp_millis();
        let formatted = CronTool::format_timestamp(ms, "UTC").unwrap();
        assert!(formatted.starts_with("2024-01-15T09:00:00"));
        assert!(formatted.ends_with("(UTC)"));
    }

    #[test]
    fn format_timestamp_unknown_tz() {
        assert_eq!(
            CronTool::format_timestamp(0, "Not/A/Zone").unwrap_err(),
            "Error: unknown timezone 'Not/A/Zone'"
        );
    }

    fn test_tool() -> CronTool {
        CronTool::new(
            CronService::new(std::path::PathBuf::from("/tmp/jobs.json"), None),
            "UTC",
        )
    }

    #[test]
    fn format_timing_every_hours_and_cron() {
        let tool = test_tool();
        assert_eq!(
            tool.format_timing(&CronSchedule {
                kind: CronScheduleKind::Every,
                every_ms: Some(7_200_000),
                ..Default::default()
            }),
            "every 2h"
        );
        assert_eq!(
            tool.format_timing(&CronSchedule {
                kind: CronScheduleKind::Cron,
                expr: Some("0 9 * * *".into()),
                tz: Some("America/Vancouver".into()),
                ..Default::default()
            }),
            "cron: 0 9 * * * (America/Vancouver)"
        );
    }

    #[test]
    fn format_state_shows_last_and_next_run() {
        use chrono::{TimeZone, Utc};

        let tool = test_tool();
        let at_ms = Utc
            .with_ymd_and_hms(2030, 1, 1, 12, 0, 0)
            .unwrap()
            .timestamp_millis();
        let schedule = CronSchedule {
            kind: CronScheduleKind::At,
            at_ms: Some(at_ms),
            ..Default::default()
        };
        let lines = tool.format_state(
            &CronJobState {
                last_run_at_ms: Some(at_ms),
                last_status: Some(CronRunStatus::Error),
                last_error: Some("timeout".into()),
                next_run_at_ms: Some(at_ms + 60_000),
                ..Default::default()
            },
            &schedule,
        );
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Last run:"));
        assert!(lines[0].contains("— error"));
        assert!(lines[0].contains("(timeout)"));
        assert!(lines[1].starts_with("  Next run:"));
    }

    #[test]
    fn validate_timezone_rejects_unknown() {
        assert_eq!(
            CronTool::validate_timezone("Not/A/Zone"),
            Some("Error: unknown timezone 'Not/A/Zone'".to_string())
        );
    }

    #[tokio::test]
    async fn add_job_every_requires_context() {
        let dir = TempDir::new().unwrap();
        let tool = CronTool::new(CronService::new(dir.path().join("jobs.json"), None), "UTC");
        assert_eq!(
            tool.add_job(None, "ping", Some(60), None, None, None, true)
                .await,
            "Error: no session context (channel/chat_id)"
        );
    }

    #[tokio::test]
    async fn add_job_every_creates_job() {
        let dir = TempDir::new().unwrap();
        let tool = CronTool::new(CronService::new(dir.path().join("jobs.json"), None), "UTC");
        tool.set_context("telegram", "chat-1");

        let msg = tool
            .add_job(None, "Check inbox", Some(3600), None, None, None, false)
            .await;
        assert!(msg.starts_with("Created job 'Check inbox' (id: "));

        let jobs = tool.cron_service.list_jobs(false).await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].schedule.every_ms, Some(3_600_000));
        assert_eq!(jobs[0].payload.channel.as_deref(), Some("telegram"));
        assert_eq!(jobs[0].payload.to.as_deref(), Some("chat-1"));
    }

    #[tokio::test]
    async fn add_job_at_parses_naive_iso() {
        let dir = TempDir::new().unwrap();
        let tool = CronTool::new(CronService::new(dir.path().join("jobs.json"), None), "UTC");
        tool.set_context("cli", "direct");

        let msg = tool
            .add_job(
                Some("reminder"),
                "Wake up",
                None,
                None,
                None,
                Some("2030-06-15T08:30:00"),
                true,
            )
            .await;
        assert!(msg.contains("Created job 'reminder'"));

        let job = tool.cron_service.list_jobs(false).await.pop().unwrap();
        assert!(job.delete_after_run);
        assert!(job.state.next_run_at_ms.is_some());
    }

    #[tokio::test]
    async fn add_job_rejects_tz_without_cron_expr() {
        let dir = TempDir::new().unwrap();
        let tool = CronTool::new(CronService::new(dir.path().join("jobs.json"), None), "UTC");
        tool.set_context("cli", "direct");
        assert_eq!(
            tool.add_job(None, "x", Some(60), None, Some("UTC"), None, true)
                .await,
            "Error: tz can only be used with cron_expr"
        );
    }

    #[tokio::test]
    async fn execute_list_and_remove() {
        let dir = TempDir::new().unwrap();
        let tool = CronTool::new(CronService::new(dir.path().join("jobs.json"), None), "UTC");
        tool.set_context("cli", "direct");

        let add = serde_json::json!({
            "action": "add",
            "message": "ping",
            "every_seconds": 60
        });
        let created = tool.execute(&add).await;
        assert!(created.starts_with("Created job"));

        let list_empty = tool.execute(&serde_json::json!({ "action": "list" })).await;
        assert!(list_empty.contains("ping"));
        assert!(list_empty.starts_with("Scheduled jobs:"));

        let job_id = tool.cron_service.list_jobs(false).await[0].id.clone();

        assert_eq!(
            tool.execute(&serde_json::json!({ "action": "remove" })).await,
            "Error: job_id is required for remove"
        );

        let removed = tool
            .execute(&serde_json::json!({ "action": "remove", "job_id": job_id }))
            .await;
        assert_eq!(removed, format!("Removed job '{job_id}'"));

        assert_eq!(
            tool.execute(&serde_json::json!({ "action": "list" })).await,
            "No scheduled jobs."
        );
    }

    #[tokio::test]
    async fn cron_tool_delegates_context_methods() {
        let dir = TempDir::new().unwrap();
        let tool = CronTool::new(CronService::new(dir.path().join("jobs.json"), None), "UTC");

        with_cron_context_stack(|| async {
            assert!(!tool.in_cron_context());
            let token = tool.set_cron_context(true);
            assert!(tool.in_cron_context());
            tool.reset_cron_context(token);
            assert!(!tool.in_cron_context());
        })
        .await;
    }
}
