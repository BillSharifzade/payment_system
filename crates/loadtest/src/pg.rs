// Where the database spent the run: pg_stat_statements (when the extension is installed in the
// target database), WAL and database counters, diffed between the end of warm-up and the end.

use std::collections::HashMap;

use serde::Serialize;
use sqlx::{PgPool, Row};

#[derive(Clone, Default)]
struct Statement {
    query: String,
    calls: i64,
    total_ms: f64,
    plans: i64,
    plan_ms: f64,
    rows: i64,
    hit: i64,
    read: i64,
    wal_bytes: f64,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    statements: Option<HashMap<i64, Statement>>,
    counters: HashMap<&'static str, f64>,
}

#[derive(Serialize)]
pub struct StatementReport {
    pub query: String,
    pub calls: i64,
    pub total_ms: f64,
    pub mean_ms: f64,
    /// Plans made (pg_stat_statements.track_planning = on; else 0) and their total time.
    pub plans: i64,
    pub plan_ms: f64,
    pub rows: i64,
    pub shared_hit: i64,
    pub shared_read: i64,
    pub wal_bytes: f64,
}

#[derive(Serialize)]
pub struct PgReport {
    /// Top statements by total execution time (None: pg_stat_statements not installed).
    pub statements: Option<Vec<StatementReport>>,
    pub counters: std::collections::BTreeMap<&'static str, f64>,
}

const COUNTERS: &str = "SELECT
    w.wal_records::float8, w.wal_fpi::float8, w.wal_bytes::float8, w.wal_write::float8,
    w.wal_sync::float8, w.wal_write_time, w.wal_sync_time,
    d.xact_commit::float8, d.xact_rollback::float8, d.deadlocks::float8,
    d.blks_read::float8, d.blks_hit::float8, d.blk_read_time, d.temp_bytes::float8
  FROM pg_stat_wal w, pg_stat_database d WHERE d.datname = current_database()";
const COUNTER_NAMES: [&str; 14] = [
    "wal_records",
    "wal_fpi",
    "wal_bytes",
    "wal_write",
    "wal_sync",
    "wal_write_ms",
    "wal_sync_ms",
    "xact_commit",
    "xact_rollback",
    "deadlocks",
    "blks_read",
    "blks_hit",
    "blk_read_ms",
    "temp_bytes",
];

pub async fn snapshot(pool: &PgPool) -> Result<Snapshot, String> {
    let db = |e: sqlx::Error| format!("pg stats: {e}");
    let row = sqlx::query(COUNTERS).fetch_one(pool).await.map_err(db)?;
    let counters = COUNTER_NAMES
        .iter()
        .enumerate()
        .map(|(i, n)| (*n, row.try_get::<f64, _>(i).unwrap_or(0.0)))
        .collect();
    let installed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_stat_statements')",
    )
    .fetch_one(pool)
    .await
    .map_err(db)?;
    let statements = if installed {
        let rows = sqlx::query(
            "SELECT queryid, query, calls, total_exec_time, plans, total_plan_time, rows,
                    shared_blks_hit,
                    shared_blks_read, wal_bytes::float8
             FROM pg_stat_statements
             WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
               AND queryid IS NOT NULL",
        )
        .fetch_all(pool)
        .await
        .map_err(db)?;
        let mut map = HashMap::with_capacity(rows.len());
        for r in rows {
            let entry: &mut Statement = map.entry(r.get::<i64, _>("queryid")).or_default();
            entry.query = r.get("query");
            entry.calls += r.get::<i64, _>("calls");
            entry.total_ms += r.get::<f64, _>("total_exec_time");
            entry.plans += r.get::<i64, _>("plans");
            entry.plan_ms += r.get::<f64, _>("total_plan_time");
            entry.rows += r.get::<i64, _>("rows");
            entry.hit += r.get::<i64, _>("shared_blks_hit");
            entry.read += r.get::<i64, _>("shared_blks_read");
            entry.wal_bytes += r.get::<f64, _>("wal_bytes");
        }
        Some(map)
    } else {
        None
    };
    Ok(Snapshot {
        statements,
        counters,
    })
}

pub fn diff(before: &Snapshot, after: &Snapshot, top: usize) -> PgReport {
    let statements = after.statements.as_ref().map(|now| {
        let empty = HashMap::new();
        let then = before.statements.as_ref().unwrap_or(&empty);
        let mut out: Vec<StatementReport> = now
            .iter()
            .filter_map(|(id, s)| {
                let b = then.get(id).cloned().unwrap_or_default();
                let calls = s.calls - b.calls;
                (calls > 0).then(|| {
                    let total_ms = s.total_ms - b.total_ms;
                    StatementReport {
                        query: s.query.split_whitespace().collect::<Vec<_>>().join(" "),
                        calls,
                        total_ms,
                        mean_ms: total_ms / calls as f64,
                        plans: s.plans - b.plans,
                        plan_ms: s.plan_ms - b.plan_ms,
                        rows: s.rows - b.rows,
                        shared_hit: s.hit - b.hit,
                        shared_read: s.read - b.read,
                        wal_bytes: s.wal_bytes - b.wal_bytes,
                    }
                })
            })
            .collect();
        out.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
        out.truncate(top);
        out
    });
    PgReport {
        statements,
        counters: after
            .counters
            .iter()
            .map(|(k, v)| (*k, v - before.counters.get(k).copied().unwrap_or(0.0)))
            .collect(),
    }
}

/// Server settings that decide commit and buffer behaviour, recorded with every report.
pub async fn settings(pool: &PgPool) -> Result<std::collections::BTreeMap<String, String>, String> {
    let rows = sqlx::query(
        "SELECT name, setting || COALESCE(unit, '') AS v FROM pg_settings
         WHERE name IN ('server_version', 'synchronous_commit', 'commit_delay', 'commit_siblings',
                        'shared_buffers', 'max_connections', 'wal_compression', 'fsync',
                        'full_page_writes', 'wal_buffers', 'max_wal_size', 'data_checksums',
                        'wal_sync_method', 'effective_io_concurrency')",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("pg settings: {e}"))?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<String, _>("name"), r.get::<String, _>("v")))
        .collect())
}
