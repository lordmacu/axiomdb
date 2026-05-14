//! Background job scheduler for cron jobs (Phase 22b.1).
//!
//! Runs as a tokio task launched at server startup. Polls `axiom_cron_jobs`
//! every minute, fires any job whose `next_run_ms` is due, updates the
//! run metadata in the catalog, and sleeps until the next scheduled job.
//!
//! SQL execution reuses `SharedDatabase::execute_query` with a synthetic
//! `SessionContext` scoped to the job's target database.

use std::sync::Arc;

use axiomdb_catalog::CronJobDef;
use axiomdb_sql::{session::SessionContext, SchemaCache};
use chrono::{DateTime, Datelike, Timelike, Utc};
use tracing::{info, warn};

use crate::mysql::SharedDatabase;

/// Starts the cron scheduler as a background tokio task.
///
/// Returns a `JoinHandle` that the server can optionally await on shutdown.
pub fn start(db: Arc<SharedDatabase>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_scheduler_loop(db).await;
    })
}

async fn run_scheduler_loop(db: Arc<SharedDatabase>) {
    info!("cron scheduler started");
    loop {
        let now_ms = utc_now_ms();
        let jobs = load_enabled_jobs(&db, now_ms);

        for job in jobs {
            let next = compute_next_run_ms(&job.schedule, now_ms);
            let due = job.next_run_ms == 0 || job.next_run_ms <= now_ms;

            if due {
                let status = execute_job(&db, &job).await;
                let next_from_now = compute_next_run_ms(&job.schedule, utc_now_ms());
                persist_run_result(&db, &job.name, now_ms, next_from_now, &status);
            } else if job.next_run_ms == 0 && next > 0 {
                // First time we see this job — prime next_run without executing.
                persist_run_result(&db, &job.name, 0, next, "");
            }
        }

        // Sleep until the next whole minute.
        sleep_until_next_minute().await;
    }
}

fn load_enabled_jobs(db: &SharedDatabase, _now_ms: i64) -> Vec<CronJobDef> {
    let snap = db.txn.snapshot();
    let mut reader = match axiomdb_catalog::CatalogReader::new(&db.storage, snap) {
        Ok(r) => r,
        Err(e) => {
            warn!(?e, "scheduler: failed to open catalog reader");
            return Vec::new();
        }
    };
    match reader.list_cron_jobs() {
        Ok(jobs) => jobs.into_iter().filter(|j| j.enabled).collect(),
        Err(e) => {
            warn!(?e, "scheduler: failed to list cron jobs");
            Vec::new()
        }
    }
}

async fn execute_job(db: &SharedDatabase, job: &CronJobDef) -> String {
    let mut ctx = SessionContext::new();
    ctx.set_current_database(job.database_name.clone());

    let mut schema_cache = SchemaCache::new();
    match db.execute_query(&job.command, &mut ctx, &mut schema_cache) {
        Ok(_) => "ok".to_string(),
        Err(e) => {
            let msg = format!("{e}");
            warn!(job = %job.name, err = %msg, "cron job failed");
            msg.chars().take(255).collect()
        }
    }
}

fn persist_run_result(
    db: &SharedDatabase,
    name: &str,
    last_run_ms: i64,
    next_run_ms: i64,
    status: &str,
) {
    let mut conn = match db.txn.begin() {
        Ok(c) => c,
        Err(e) => {
            warn!(?e, "scheduler: failed to begin txn for persist");
            return;
        }
    };
    let mut writer = match axiomdb_catalog::CatalogWriter::new(&db.storage, &db.txn, &mut conn) {
        Ok(w) => w,
        Err(e) => {
            warn!(?e, "scheduler: failed to create writer");
            let _ = db.txn.rollback(conn, &db.storage);
            return;
        }
    };
    if let Err(e) = writer.update_cron_job_run(name, last_run_ms, next_run_ms, status) {
        warn!(?e, job = %name, "scheduler: failed to update run metadata");
        let _ = db.txn.rollback(conn, &db.storage);
        return;
    }
    match db.txn.commit(conn) {
        Ok(Some(txn_id)) => {
            let _ = db.txn.wal_flush_and_fsync();
            db.txn.advance_committed_single(txn_id);
        }
        Ok(None) => {}
        Err(e) => {
            warn!(?e, job = %name, "scheduler: commit failed");
        }
    }
}

/// Computes the next fire time (Unix ms) after `after_ms` for a 5-field cron expression.
///
/// Accepts 5-field standard cron (`min hour dom month dow`) and the
/// `@hourly`/`@daily`/`@weekly`/`@monthly`/`@yearly` aliases.
///
/// Returns 0 on parse errors (job will not fire).
fn compute_next_run_ms(schedule: &str, after_ms: i64) -> i64 {
    let after_secs = after_ms / 1000 + 1; // start from the next second
    let dt = match DateTime::from_timestamp(after_secs, 0) {
        Some(d) => d,
        None => return 0,
    };

    // Advance to the start of the next minute.
    let mut candidate = dt
        .with_second(0)
        .and_then(|d| d.with_nanosecond(0))
        .unwrap_or(dt);
    if candidate <= dt {
        candidate += chrono::Duration::minutes(1);
    }

    let s = schedule.trim();
    let (min_pat, hour_pat, dom_pat, month_pat, dow_pat) = match expand_alias(s) {
        Some(f) => f,
        None => {
            let fields: Vec<&str> = s.split_whitespace().collect();
            if fields.len() != 5 {
                return 0;
            }
            (fields[0], fields[1], fields[2], fields[3], fields[4])
        }
    };

    // Try up to 366 days ahead before giving up.
    for _ in 0..=(366 * 24 * 60) {
        if matches_field(candidate.minute(), min_pat, 0, 59)
            && matches_field(candidate.hour(), hour_pat, 0, 23)
            && matches_field(candidate.day(), dom_pat, 1, 31)
            && matches_field(candidate.month(), month_pat, 1, 12)
            && matches_weekday(candidate.weekday(), dow_pat)
        {
            return candidate.timestamp_millis();
        }
        candidate += chrono::Duration::minutes(1);
    }
    0
}

fn expand_alias(
    s: &str,
) -> Option<(
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
)> {
    match s {
        "@hourly" => Some(("0", "*", "*", "*", "*")),
        "@daily" | "@midnight" => Some(("0", "0", "*", "*", "*")),
        "@weekly" => Some(("0", "0", "*", "*", "0")),
        "@monthly" => Some(("0", "0", "1", "*", "*")),
        "@yearly" | "@annually" => Some(("0", "0", "1", "1", "*")),
        _ => None,
    }
}

fn matches_field(value: u32, pattern: &str, _min: u32, _max: u32) -> bool {
    if pattern == "*" {
        return true;
    }
    // Step syntax: */N
    if let Some(step_str) = pattern.strip_prefix("*/") {
        if let Ok(step) = step_str.parse::<u32>() {
            if step > 0 {
                return value.is_multiple_of(step);
            }
        }
        return false;
    }
    // Range: N-M
    if let Some(pos) = pattern.find('-') {
        let (lo_s, hi_s) = pattern.split_at(pos);
        let hi_s = &hi_s[1..];
        if let (Ok(lo), Ok(hi)) = (lo_s.parse::<u32>(), hi_s.parse::<u32>()) {
            return value >= lo && value <= hi;
        }
        return false;
    }
    // List: N,M,...
    if pattern.contains(',') {
        return pattern
            .split(',')
            .any(|p| p.trim().parse::<u32>().ok() == Some(value));
    }
    // Exact value
    pattern.parse::<u32>().ok() == Some(value)
}

fn matches_weekday(wd: chrono::Weekday, pattern: &str) -> bool {
    if pattern == "*" || pattern == "?" {
        return true;
    }
    // Map cron DOW: 0/7 = Sunday, 1 = Monday, …, 6 = Saturday
    let cron_dow = match wd {
        chrono::Weekday::Sun => 0u32,
        chrono::Weekday::Mon => 1,
        chrono::Weekday::Tue => 2,
        chrono::Weekday::Wed => 3,
        chrono::Weekday::Thu => 4,
        chrono::Weekday::Fri => 5,
        chrono::Weekday::Sat => 6,
    };
    // Accept numeric aliases like "7" for Sunday.
    let normalized = pattern.replace('7', "0");
    matches_field(cron_dow, &normalized, 0, 6)
}

fn utc_now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

async fn sleep_until_next_minute() {
    let now = Utc::now();
    let secs_to_next = 60 - now.second() as u64;
    tokio::time::sleep(tokio::time::Duration::from_secs(secs_to_next)).await;
}
