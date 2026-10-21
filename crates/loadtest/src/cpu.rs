// CPU time spent by the harness, the spawned server and the Postgres processes over the measured
// window, from /proc (Linux, USER_HZ = 100; Postgres only when it runs on this host). On a
// shared, noisy box CPU per operation is a far steadier A/B signal than throughput: the work
// one operation costs does not depend on what the neighbours are doing.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use sqlx::PgPool;

fn proc_secs(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name; utime and stime are fields 14 and 15.
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let mut f = rest.split_whitespace().skip(11);
    let utime: f64 = f.next()?.parse().ok()?;
    let stime: f64 = f.next()?.parse().ok()?;
    Some((utime + stime) / 100.0)
}

#[derive(Default)]
pub struct Snapshot {
    harness: f64,
    server: Option<f64>,
    postgres: HashMap<u32, f64>,
}

pub async fn snapshot(pool: &PgPool, server: Option<u32>) -> Snapshot {
    let pids: Vec<i32> = sqlx::query_scalar("SELECT pid FROM pg_stat_activity")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    Snapshot {
        harness: proc_secs(std::process::id()).unwrap_or(0.0),
        server: server.and_then(proc_secs),
        // A pid from another pid namespace (Postgres in a container) names some other process
        // here, or none: only processes that are Postgres count.
        postgres: pids
            .into_iter()
            .map(|p| p as u32)
            .filter(|p| {
                std::fs::read_to_string(format!("/proc/{p}/comm"))
                    .is_ok_and(|c| c.trim_end() == "postgres")
            })
            .filter_map(|p| Some((p, proc_secs(p)?)))
            .collect(),
    }
}

#[derive(Serialize)]
pub struct CpuReport {
    pub harness_s: f64,
    pub server_s: Option<f64>,
    /// None when Postgres is not on this host.
    pub postgres_s: Option<f64>,
    /// CPU milliseconds per successful operation, per process group.
    pub per_op_ms: BTreeMap<&'static str, f64>,
}

pub fn diff(before: &Snapshot, after: &Snapshot, ok_ops: u64) -> CpuReport {
    let server_s = after.server.zip(before.server).map(|(a, b)| a - b);
    // Backends that started during the window count from zero.
    let postgres_s = (!after.postgres.is_empty()).then(|| {
        after
            .postgres
            .iter()
            .map(|(pid, a)| a - before.postgres.get(pid).copied().unwrap_or(0.0))
            .sum::<f64>()
    });
    let harness_s = after.harness - before.harness;
    let per = |s: f64| s * 1000.0 / ok_ops.max(1) as f64;
    let mut per_op_ms = BTreeMap::from([("harness", per(harness_s))]);
    if let Some(s) = server_s {
        per_op_ms.insert("server", per(s));
    }
    if let Some(s) = postgres_s {
        per_op_ms.insert("postgres", per(s));
    }
    CpuReport {
        harness_s,
        server_s,
        postgres_s,
        per_op_ms,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_proc_stat() {
        assert!(super::proc_secs(std::process::id()).is_some_and(|s| s >= 0.0));
        assert!(super::proc_secs(u32::MAX).is_none());
    }
}
