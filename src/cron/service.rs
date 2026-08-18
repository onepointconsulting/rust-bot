//! Cron service for scheduling agent tasks.

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use chrono_tz::Tz;
use croner::Cron;

use crate::cron::{CronSchedule, CronScheduleKind};

/// Current Unix time in milliseconds, matching Python `int(time.time() * 1000)`.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Next cron fire time strictly after `now_ms` for a cron expression (croniter parity).
fn next_cron_run(expr: &str, now_ms: i64, tz: Option<&str>) -> Option<i64> {
    // Five-field rust-bot expressions use Cron::new (not bare FromStr in croner 2.x).
    let cron = Cron::new(expr).parse().ok()?;
    let secs = now_ms.div_euclid(1000);
    let nanos = (now_ms.rem_euclid(1000) * 1_000_000) as u32;

    if let Some(tz_name) = tz {
        let tz: Tz = tz_name.parse().ok()?;
        let base = tz.timestamp_opt(secs, nanos).single()?;
        let next = cron.find_next_occurrence(&base, false).ok()?;
        Some(next.timestamp_millis())
    } else {
        let base = Local.timestamp_opt(secs, nanos).single()?;
        let next = cron.find_next_occurrence(&base, false).ok()?;
        Some(next.timestamp_millis())
    }
}

/// Compute next run time in ms.
pub fn compute_next_run(schedule: &CronSchedule, now_ms: i64) -> Option<i64> {
    match schedule.kind {
        CronScheduleKind::At => match schedule.at_ms {
            Some(at_ms) if at_ms > now_ms => Some(at_ms),
            _ => None,
        },
        CronScheduleKind::Every => {
            let every_ms = schedule.every_ms?;
            if every_ms <= 0 {
                return None;
            }
            Some(now_ms + every_ms)
        }
        CronScheduleKind::Cron => {
            let expr = schedule.expr.as_deref()?;
            if expr.is_empty() {
                return None;
            }
            next_cron_run(expr, now_ms, schedule.tz.as_deref())
        }
    }
}

/// Returns an error message if `tz` is not a known IANA name, else `None` (Python `_validate_timezone`).
pub fn validate_timezone(tz: &str) -> Option<String> {
    if tz.parse::<Tz>().is_ok() {
        None
    } else {
        Some(format!("Error: unknown timezone '{tz}'"))
    }
}

/// Parse an ISO datetime string to Unix ms (Python `datetime.fromisoformat` + default tz).
pub fn parse_at_iso(at: &str, default_timezone: &str) -> Result<i64, String> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(at) {
        return Ok(dt.timestamp_millis());
    }
    if let Ok(dt) = DateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S%#z") {
        return Ok(dt.timestamp_millis());
    }
    if let Ok(dt) = DateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S%.f%#z") {
        return Ok(dt.timestamp_millis());
    }

    let naive = NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S%.f"))
        .map_err(|_| {
            format!(
                "Error: invalid ISO datetime format '{at}'. Expected format: YYYY-MM-DDTHH:MM:SS"
            )
        })?;

    if let Some(err) = validate_timezone(default_timezone) {
        return Err(err);
    }
    let tz: Tz = default_timezone
        .parse()
        .map_err(|_| format!("Error: unknown timezone '{default_timezone}'"))?;
    let local = tz
        .from_local_datetime(&naive)
        .single()
        .ok_or_else(|| format!("Error: invalid local datetime '{at}'"))?;
    Ok(local.timestamp_millis())
}

/// Format a Unix timestamp in ms for display (Python `_format_timestamp`).
pub fn format_timestamp(ms: i64, tz_name: &str) -> Result<String, String> {
    let tz: Tz = tz_name
        .parse()
        .map_err(|_| format!("Error: unknown timezone '{tz_name}'"))?;
    let secs = ms.div_euclid(1000);
    let nanos = (ms.rem_euclid(1000) * 1_000_000) as u32;
    let dt = tz
        .timestamp_opt(secs, nanos)
        .single()
        .ok_or_else(|| format!("Error: invalid timestamp {ms}"))?;
    Ok(format!("{} ({tz_name})", dt.to_rfc3339()))
}

/// Validate schedule fields that would otherwise create non-runnable jobs.
///
/// Mirrors Python `_validate_schedule_for_add`: `ZoneInfo(name)` only checks that the
/// IANA timezone exists; it does not change the system timezone. Use `chrono::Local`
/// when `tz` is unset (same as `datetime.now().astimezone().tzinfo`).
pub fn validate_schedule_for_add(schedule: &CronSchedule) -> Result<(), String> {
    if schedule.tz.is_some() && schedule.kind != CronScheduleKind::Cron {
        return Err("tz can only be used with cron schedules".into());
    }

    if schedule.kind == CronScheduleKind::Cron {
        if let Some(tz) = schedule.tz.as_deref() {
            if let Some(msg) = validate_timezone(tz) {
                return Err(msg.trim_start_matches("Error: ").to_string());
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::CronSchedule;
    use chrono::{TimeZone, Utc};

    fn cron_schedule(expr: &str, tz: Option<&str>) -> CronSchedule {
        CronSchedule {
            kind: CronScheduleKind::Cron,
            expr: Some(expr.to_string()),
            tz: tz.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn now_ms_returns_positive_unix_millis() {
        let t = now_ms();
        assert!(t > 1_700_000_000_000);
    }

    #[test]
    fn compute_next_run_at_future() {
        let schedule = CronSchedule {
            kind: CronScheduleKind::At,
            at_ms: Some(9_000),
            ..Default::default()
        };
        assert_eq!(compute_next_run(&schedule, 1_000), Some(9_000));
        assert_eq!(compute_next_run(&schedule, 9_000), None);
    }

    #[test]
    fn compute_next_run_every() {
        let schedule = CronSchedule {
            kind: CronScheduleKind::Every,
            every_ms: Some(60_000),
            ..Default::default()
        };
        assert_eq!(compute_next_run(&schedule, 1_000), Some(61_000));
    }

    #[test]
    fn compute_next_run_cron_utc_before_nine() {
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 8, 0, 0).unwrap();
        let now_ms = now.timestamp_millis();
        let schedule = cron_schedule("0 9 * * *", Some("UTC"));

        let next = compute_next_run(&schedule, now_ms).unwrap();
        let expected = Utc
            .with_ymd_and_hms(2024, 1, 15, 9, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(next, expected);
    }

    #[test]
    fn compute_next_run_cron_new_york_smoke() {
        let now_ms = Utc
            .with_ymd_and_hms(2024, 1, 15, 13, 0, 0)
            .unwrap()
            .timestamp_millis();
        let schedule = cron_schedule("0 9 * * *", Some("America/New_York"));
        assert!(compute_next_run(&schedule, now_ms).is_some());
    }

    #[test]
    fn compute_next_run_cron_local_smoke() {
        let now_ms = Utc
            .with_ymd_and_hms(2024, 6, 15, 12, 0, 0)
            .unwrap()
            .timestamp_millis();
        let schedule = cron_schedule("0 9 * * *", None);
        assert!(compute_next_run(&schedule, now_ms).is_some());
    }

    #[test]
    fn compute_next_run_cron_dream_expr() {
        let now = Utc.with_ymd_and_hms(2024, 3, 10, 1, 0, 0).unwrap();
        let schedule = cron_schedule("0 2 * * *", Some("UTC"));
        let next = compute_next_run(&schedule, now.timestamp_millis()).unwrap();
        let expected = Utc
            .with_ymd_and_hms(2024, 3, 10, 2, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(next, expected);
    }

    #[test]
    fn compute_next_run_invalid_expr_returns_none() {
        let schedule = cron_schedule("not a cron", Some("UTC"));
        assert_eq!(compute_next_run(&schedule, 0), None);
    }

    #[test]
    fn compute_next_run_invalid_tz_returns_none() {
        let schedule = cron_schedule("0 9 * * *", Some("Not/A/Zone"));
        assert_eq!(compute_next_run(&schedule, 1_704_000_000_000), None);
    }

    #[test]
    fn validate_schedule_rejects_tz_on_non_cron_kind() {
        let schedule = CronSchedule {
            kind: CronScheduleKind::Every,
            every_ms: Some(60_000),
            tz: Some("UTC".into()),
            ..Default::default()
        };
        assert_eq!(
            validate_schedule_for_add(&schedule).unwrap_err(),
            "tz can only be used with cron schedules"
        );
    }

    #[test]
    fn validate_schedule_accepts_known_iana_tz() {
        let schedule = cron_schedule("0 9 * * *", Some("America/New_York"));
        assert!(validate_schedule_for_add(&schedule).is_ok());
    }

    #[test]
    fn parse_at_iso_naive_uses_default_tz() {
        use chrono::{TimeZone, Utc};

        let ms = parse_at_iso("2030-06-15T08:30:00", "UTC").unwrap();
        let expected = Utc
            .with_ymd_and_hms(2030, 6, 15, 8, 30, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(ms, expected);
    }

    #[test]
    fn parse_at_iso_invalid_format() {
        assert!(
            parse_at_iso("not-a-date", "UTC")
                .unwrap_err()
                .contains("invalid ISO")
        );
    }

    #[test]
    fn format_timestamp_matches_isoformat_shape() {
        use chrono::{TimeZone, Utc};

        let ms = Utc
            .with_ymd_and_hms(2024, 3, 10, 14, 30, 0)
            .unwrap()
            .timestamp_millis();
        let out = format_timestamp(ms, "America/New_York").unwrap();
        assert!(out.contains("2024-03-10"));
        assert!(out.ends_with("(America/New_York)"));
    }

    #[test]
    fn validate_timezone_returns_error_prefix() {
        assert_eq!(
            validate_timezone("Not/A/Zone"),
            Some("Error: unknown timezone 'Not/A/Zone'".to_string())
        );
        assert!(validate_timezone("America/New_York").is_none());
    }

    #[test]
    fn validate_schedule_rejects_unknown_tz() {
        let schedule = cron_schedule("0 9 * * *", Some("Not/A/Zone"));
        assert_eq!(
            validate_schedule_for_add(&schedule).unwrap_err(),
            "unknown timezone 'Not/A/Zone'"
        );
    }

    #[test]
    fn compute_next_run_empty_expr_returns_none() {
        let schedule = CronSchedule {
            kind: CronScheduleKind::Cron,
            expr: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(compute_next_run(&schedule, 0), None);
    }
}
