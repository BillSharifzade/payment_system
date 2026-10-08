mod args;
mod cpu;
mod direct;
mod http;
mod pg;
mod plan;
mod server;
mod stats;
mod summarize;
mod verify;

use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use uuid::Uuid;

use args::{Config, Mode};
use plan::{Planned, Planner, Rng};
use stats::{Recorder, Step, Summary};

/// A deposit made during setup: part of the expected balance of its wallet.
pub struct Funding {
    pub id: Uuid,
    pub wallet: Uuid,
    pub amount: i64,
}

/// Run `f` over `items` with at most `limit` in flight; results in input order.
pub async fn par<I, T, F, Fut>(limit: usize, items: Vec<I>, f: F) -> Result<Vec<T>, String>
where
    I: Send + 'static,
    T: Send + 'static,
    F: Fn(I) -> Fut,
    Fut: Future<Output = Result<T, String>> + Send + 'static,
{
    let permits = Arc::new(Semaphore::new(limit.max(1)));
    let mut set = JoinSet::new();
    let n = items.len();
    for (i, item) in items.into_iter().enumerate() {
        let permit = permits.clone().acquire_owned().await.expect("never closed");
        let fut = f(item);
        set.spawn(async move {
            let out = fut.await;
            drop(permit);
            (i, out)
        });
    }
    let mut out: Vec<Option<T>> = (0..n).map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        let (i, r) = joined.map_err(|e| format!("setup task: {e}"))?;
        out[i] = Some(r?);
    }
    Ok(out
        .into_iter()
        .map(|o| o.expect("every task joined"))
        .collect())
}

enum Target {
    Http(http::Http),
    Direct(direct::Direct),
}

impl Target {
    async fn exec(&self, planned: Planned, begin: Instant) -> Vec<Step> {
        match self {
            Target::Http(h) => h.exec(planned, begin).await,
            Target::Direct(d) => d.exec(planned, begin).await,
        }
    }
}

struct Window {
    start: Instant,
    measure_from: Instant,
    end: Instant,
}

/// Closed loop: each worker issues its next request when the previous one finishes. Open loop
/// (--rate): each worker follows a fixed schedule and latency counts from the intended start,
/// so a stall shows up in the percentiles instead of silently lowering the offered load.
async fn drive(cfg: &Config, target: Arc<Target>, planner: Arc<Planner>, w: &Window) -> Recorder {
    let mut set = JoinSet::new();
    for worker in 0..cfg.concurrency {
        let (target, planner) = (target.clone(), planner.clone());
        let mut rng = Rng::new(cfg.seed ^ (worker as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let every = cfg
            .rate
            .map(|r| Duration::from_secs_f64(cfg.concurrency as f64 / r));
        let (measure_from, end) = (w.measure_from, w.end);
        let mut next = w.start
            + every
                .unwrap_or_default()
                .mul_f64(worker as f64 / cfg.concurrency as f64);
        set.spawn(async move {
            let mut rec = Recorder::default();
            loop {
                let begin = match every {
                    Some(every) => {
                        if next >= end {
                            break;
                        }
                        tokio::time::sleep_until(next.into()).await;
                        let b = next;
                        next += every;
                        b
                    }
                    None => {
                        let now = Instant::now();
                        if now >= end {
                            break;
                        }
                        now
                    }
                };
                for step in target.exec(planner.next(&mut rng), begin).await {
                    rec.record(measure_from, step);
                }
            }
            rec
        });
    }
    let mut all = Recorder::default();
    while let Some(r) = set.join_next().await {
        all.merge(r.expect("worker panicked"));
    }
    all
}

#[derive(Serialize)]
struct Report<'a> {
    harness: &'static str,
    label: &'a Option<String>,
    config: &'a Config,
    host_cpus: usize,
    postgres: std::collections::BTreeMap<String, String>,
    summary: Summary,
    verification: verify::Verification,
    cpu: cpu::CpuReport,
    pg: Option<pg::PgReport>,
    setup_secs: f64,
    verify_secs: f64,
}

async fn connect(cfg: &Config, size: u32) -> Result<PgPool, String> {
    // The server's session settings (crates/api/src/main.rs), so direct mode behaves alike.
    let opts = PgConnectOptions::from_str(&cfg.database_url)
        .map_err(|e| format!("--database-url: {e}"))?
        .application_name("payment-loadtest")
        .options([
            ("statement_timeout", "5000"),
            ("lock_timeout", "2000"),
            ("idle_in_transaction_session_timeout", "10000"),
        ]);
    PgPoolOptions::new()
        .max_connections(size)
        .acquire_timeout(Duration::from_secs(5))
        .test_before_acquire(false)
        .connect_with(opts)
        .await
        .map_err(|e| format!("connect: {e}"))
}

fn print(s: &Summary, v: &verify::Verification) {
    eprintln!(
        "\n{:<13}{:>10}{:>8}{:>10}{:>9}{:>9}{:>9}{:>9}{:>9}",
        "op", "ok", "errors", "ok/s", "p50ms", "p90ms", "p99ms", "p99.9ms", "maxms"
    );
    let row = |name: &str, r: &stats::OpReport| {
        let l = r.latency_ms;
        eprintln!(
            "{name:<13}{:>10}{:>8}{:>10.0}{:>9.2}{:>9.2}{:>9.2}{:>9.2}{:>9.2}",
            r.ok, r.errors, r.ok_per_sec, l.p50, l.p90, l.p99, l.p999, l.max
        );
    };
    for (op, r) in &s.ops {
        row(op.name(), r);
    }
    row("total", &s.total);
    for (code, n) in &s.errors {
        eprintln!("  error {code}: {n}");
    }
    eprintln!(
        "verification: {} — {} acknowledged, {} lost, {} wrong legs, {} refused-but-posted, \
         {} unresolved ({} posted), {} unexpected, {} drifted wallets, {} balance mismatches, \
         unbalanced currencies {:?}",
        if v.passed { "PASSED" } else { "FAILED" },
        v.acknowledged,
        v.lost,
        v.wrong_legs,
        v.rejected_but_posted,
        v.unresolved,
        v.unresolved_posted,
        v.unexpected_transactions,
        v.wallet_drift,
        v.balance_mismatches,
        v.unbalanced_currencies
    );
}

async fn run(cfg: Config) -> Result<bool, String> {
    eprintln!("seed {}", cfg.seed);
    let mut rng = Rng::new(cfg.seed);
    let admin_pool = connect(&cfg, 4).await?;
    let setup_started = Instant::now();
    let mut server = None;
    let (target, funding, wallets) = match cfg.mode {
        Mode::Http => {
            let base = match &cfg.base_url {
                Some(b) => b.clone(),
                None => {
                    let (child, base) = server::spawn(&cfg).await?;
                    server = Some(child);
                    base
                }
            };
            let (h, funding) = http::setup(&cfg, base, &admin_pool, &mut rng).await?;
            let wallets = h
                .users
                .iter()
                .flat_map(|u| std::iter::once(u.wallet).chain(u.usd_wallet))
                .collect::<Vec<_>>();
            (Target::Http(h), funding, wallets)
        }
        Mode::Direct => {
            let pool = connect(&cfg, cfg.pool_size).await?;
            let (d, funding) = direct::setup(&cfg, pool, &mut rng).await?;
            let wallets = d.wallets();
            (Target::Direct(d), funding, wallets)
        }
    };
    let setup_secs = setup_started.elapsed().as_secs_f64();
    eprintln!(
        "setup {setup_secs:.1}s; running {:?}/{:?} at concurrency {} for {:?} (+{:?} warm-up)",
        cfg.mode, cfg.workload, cfg.concurrency, cfg.duration, cfg.warmup
    );

    let start = Instant::now();
    let window = Window {
        start,
        measure_from: start + cfg.warmup,
        end: start + cfg.warmup + cfg.duration,
    };
    let server_pid = server.as_ref().and_then(|c| c.id());
    let probe = {
        let (pool, at, pg_stats) = (admin_pool.clone(), window.measure_from, cfg.pg_stats);
        tokio::spawn(async move {
            tokio::time::sleep_until(at.into()).await;
            let cpu = cpu::snapshot(&pool, server_pid).await;
            let pg = match pg_stats {
                true => Some(pg::snapshot(&pool).await?),
                false => None,
            };
            Ok::<_, String>((cpu, pg))
        })
    };
    // Kept alive until the snapshots: dropping the direct pool would end its backends early.
    let target = Arc::new(target);
    let rec = drive(&cfg, target.clone(), Arc::new(Planner::new(&cfg)), &window).await;
    let measured = Instant::now()
        .min(window.end)
        .saturating_duration_since(window.measure_from);
    let (cpu_before, pg_before) = probe.await.map_err(|e| e.to_string())??;
    let cpu_after = cpu::snapshot(&admin_pool, server_pid).await;
    let pg_report = match pg_before {
        Some(before) => Some(pg::diff(&before, &pg::snapshot(&admin_pool).await?, 15)),
        None => None,
    };
    drop((target, server));

    let verify_started = Instant::now();
    let verification = verify::verify(&admin_pool, &wallets, &funding, &rec).await?;
    let summary = rec.summarise(measured.max(Duration::from_millis(1)), cfg.interval);
    let cpu = cpu::diff(&cpu_before, &cpu_after, summary.total.ok);
    print(&summary, &verification);
    eprintln!("cpu ms/op: {:?}", cpu.per_op_ms);

    let p99_ok = cfg.p99_ceiling_ms.is_none_or(|ceiling| {
        summary.ops.iter().all(|(op, r)| {
            let ok = r.latency_ms.p99 <= ceiling;
            if !ok {
                eprintln!(
                    "{} p99 {:.1} ms exceeds the {ceiling} ms ceiling",
                    op.name(),
                    r.latency_ms.p99
                );
            }
            ok
        })
    });
    let clean = summary.total.errors == 0 && verification.unresolved == 0;
    let passed = verification.passed && p99_ok && (clean || cfg.allow_errors);
    let report = Report {
        harness: env!("CARGO_PKG_VERSION"),
        label: &cfg.label,
        config: &cfg,
        host_cpus: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        postgres: pg::settings(&admin_pool).await?,
        summary,
        verification,
        cpu,
        pg: pg_report,
        setup_secs,
        verify_secs: verify_started.elapsed().as_secs_f64(),
    };
    if let Some(path) = &cfg.report {
        let json = serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| format!("{}: {e}", path.display()))?;
        eprintln!("report written to {}", path.display());
    }
    if !clean && !cfg.allow_errors {
        eprintln!("FAILED: requests failed (pass --allow-errors to accept)");
    }
    Ok(passed)
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("summarize") {
        match summarize::run(&argv[1..]) {
            Ok(table) => print!("{table}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        }
        return;
    }
    let cfg = match args::parse(argv) {
        Ok(Some(cfg)) => cfg,
        Ok(None) => {
            print!("{}", args::USAGE);
            return;
        }
        Err(e) => {
            eprintln!("{e}\n\n{}", args::USAGE);
            std::process::exit(2);
        }
    };
    match run(cfg).await {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    }
}
