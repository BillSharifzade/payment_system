//! Posting throughput and latency, interleaved on one box:
//!
//! - `postgres`:  `PostgresLedger::post_on` (row locks, balances in Postgres)
//! - `hybrid`:    `HybridLedger::post_on` against the live cluster (reserve, Postgres step, post)
//! - `hybrid-sim`: the same protocol against the in-process model, i.e. the Postgres step with
//!   TigerBeetle costing ~nothing: the ceiling the Postgres step alone imposes
//! - `direct`:    `TbLedger::post_direct`, the pure-TigerBeetle path (no Postgres, no guards)
//!
//! Each round measures every backend at every concurrency, rotating the backend order, so
//! background load (other builds on the box) lands on all of them alike; the report gives the
//! median over rounds and the spread. `--guard aml` adds the per-user AML guard's shape (user
//! row lock + windowed debit sum) to the Postgres-backed paths; `direct` cannot run guards.
//!
//! `TB_ADDRESS`, `TB_CLUSTER_ID`, `DATABASE_URL`; flags: `--concurrency 1,4,16,32`
//! `--seconds 5` `--rounds 3` `--wallets 2000` `--guard none|aml` `--tb-sessions 1`
//! `--recipients 0` (0: any wallet pays any other; N: everyone pays one of the first N, so
//! `--recipients 1` is a single hot merchant account).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ledger::{Account, AccountId, AccountType, Entry, Transaction};
use ledger_tigerbeetle::{HybridLedger, SimTb, TbClient, TbConfig, TbLedger};
use ledger_tigerbeetle_live::LiveTb;
use money::{Currency, Money};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgConnection;
use storage::{HookError, PostHook, PostOptions, PostgresLedger};
use uuid::Uuid;

#[derive(Clone)]
enum Backend {
    Postgres(PostgresLedger),
    Hybrid(HybridLedger<LiveTb>),
    HybridSim(HybridLedger<SimTb>),
    Direct(TbLedger<LiveTb>),
}

impl Backend {
    fn name(&self) -> &'static str {
        match self {
            Backend::Postgres(_) => "postgres",
            Backend::Hybrid(_) => "hybrid",
            Backend::HybridSim(_) => "hybrid-sim",
            Backend::Direct(_) => "direct",
        }
    }

    async fn post(
        &self,
        conn: &mut PgConnection,
        txn: &Transaction,
        guard: Option<PostHook>,
    ) -> bool {
        let opts = PostOptions {
            guard,
            ..PostOptions::default()
        };
        match self {
            Backend::Postgres(l) => l.post_on(conn, txn, opts).await.is_ok(),
            Backend::Hybrid(l) => l.post_on(conn, txn, opts).await.is_ok(),
            Backend::HybridSim(l) => l.post_on(conn, txn, opts).await.is_ok(),
            Backend::Direct(l) => l.post_direct(txn).await.is_ok(),
        }
    }
}

struct Setup {
    backend: Backend,
    wallets: Vec<(AccountId, Uuid)>,
}

struct Args {
    concurrency: Vec<usize>,
    seconds: u64,
    rounds: usize,
    wallets: usize,
    aml: bool,
    sessions: usize,
    recipients: usize,
}

fn args() -> Args {
    let mut a = Args {
        concurrency: vec![1, 4, 16, 32],
        seconds: 5,
        rounds: 3,
        wallets: 2_000,
        aml: false,
        sessions: 1,
        recipients: 0,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    for pair in argv.chunks(2) {
        let (flag, value) = (
            pair[0].as_str(),
            pair.get(1).map(String::as_str).unwrap_or(""),
        );
        let bad = || -> ! { panic!("{flag} {value:?}: see the doc comment for usage") };
        match flag {
            "--concurrency" => {
                a.concurrency = value
                    .split(',')
                    .map(|c| c.parse().unwrap_or_else(|_| bad()))
                    .collect()
            }
            "--seconds" => a.seconds = value.parse().unwrap_or_else(|_| bad()),
            "--rounds" => a.rounds = value.parse().unwrap_or_else(|_| bad()),
            "--wallets" => a.wallets = value.parse().unwrap_or_else(|_| bad()),
            "--tb-sessions" => a.sessions = value.parse().unwrap_or_else(|_| bad()),
            "--recipients" => a.recipients = value.parse().unwrap_or_else(|_| bad()),
            "--guard" => match value {
                "aml" => a.aml = true,
                "none" => a.aml = false,
                _ => bad(),
            },
            _ => bad(),
        }
    }
    a
}

/// The AML guard's shape: one users-row lock, then the user's debits in the window.
fn aml(user: Uuid) -> PostHook {
    Box::new(move |conn: &mut PgConnection| {
        Box::pin(async move {
            sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR NO KEY UPDATE")
                .bind(user)
                .execute(&mut *conn)
                .await?;
            let spent: i64 = sqlx::query_scalar(
                "SELECT COALESCE(SUM(e.amount_minor), 0)::BIGINT
                 FROM accounts a JOIN entries e ON e.account_id = a.id
                 WHERE a.owner_user_id = $1 AND e.direction = 'debit'
                   AND e.created_at >= now() - interval '24 hours'",
            )
            .bind(user)
            .fetch_one(&mut *conn)
            .await?;
            if spent > i64::MAX / 2 {
                return Err(HookError::Rejected {
                    rule: "daily_limit".into(),
                    message: "unreachable in the benchmark".into(),
                });
            }
            Ok(())
        })
    })
}

async fn setup(backend: Backend, pg: &PostgresLedger, n: usize) -> Setup {
    let tjs = Currency::tjs();
    let settlement = AccountId::new();
    let open = async |account: Account, owner: Option<Uuid>| match &backend {
        Backend::Postgres(l) => l.open_account_owned(&account, owner).await.unwrap(),
        Backend::Hybrid(l) => {
            let mut conn = pg.pool().acquire().await.unwrap();
            l.open_account_owned_on(&mut conn, &account, owner)
                .await
                .unwrap()
        }
        Backend::HybridSim(l) => {
            let mut conn = pg.pool().acquire().await.unwrap();
            l.open_account_owned_on(&mut conn, &account, owner)
                .await
                .unwrap()
        }
        Backend::Direct(l) => l.open_account(&account).await.unwrap(),
    };
    open(
        Account::new(settlement, AccountType::SystemSettlement, tjs),
        None,
    )
    .await;
    let mut wallets = Vec::with_capacity(n);
    for _ in 0..n {
        let (wallet, user) = (AccountId::new(), Uuid::now_v7());
        sqlx::query("INSERT INTO users (id, phone, password_hash) VALUES ($1, $2, 'x')")
            .bind(user)
            .bind(format!("tb-bench-{user}"))
            .execute(pg.pool())
            .await
            .unwrap();
        open(
            Account::new(wallet, AccountType::UserWallet, tjs),
            Some(user),
        )
        .await;
        let deposit = Transaction::with_entries(vec![
            Entry::debit(settlement, Money::from_minor(1_000_000_000, tjs)),
            Entry::credit(wallet, Money::from_minor(1_000_000_000, tjs)),
        ]);
        let mut conn = pg.pool().acquire().await.unwrap();
        assert!(
            backend.post(&mut conn, &deposit, None).await,
            "funding {}",
            backend.name()
        );
        wallets.push((wallet, user));
    }
    Setup { backend, wallets }
}

struct Sample {
    ops_per_sec: f64,
    p50_us: u64,
    p99_us: u64,
    errors: usize,
}

async fn measure(
    setup: &Arc<Setup>,
    pg: &PostgresLedger,
    workers: usize,
    secs: u64,
    guard: bool,
    recipients: usize,
) -> Sample {
    let stop = Arc::new(AtomicBool::new(false));
    let tasks: Vec<_> = (0..workers)
        .map(|w| {
            let (setup, stop, pool) = (setup.clone(), stop.clone(), pg.pool().clone());
            tokio::spawn(async move {
                let mut conn = pool.acquire().await.unwrap();
                let (mut lat, mut errors) = (Vec::new(), 0usize);
                let mut x = (w as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                let n = setup.wallets.len() as u64;
                while !stop.load(Ordering::Relaxed) {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let (from, to) = match recipients as u64 {
                        0 => {
                            let (from, to) = (x % n, (x >> 20) % (n - 1));
                            (from as usize, (to + u64::from(to >= from)) as usize)
                        }
                        r => ((r + x % (n - r)) as usize, ((x >> 20) % r) as usize),
                    };
                    let amount = Money::from_minor(1 + (x >> 40) as i128 % 100, Currency::tjs());
                    let (wallet, user) = setup.wallets[from];
                    let txn = Transaction::with_entries(vec![
                        Entry::debit(wallet, amount),
                        Entry::credit(setup.wallets[to].0, amount),
                    ]);
                    let started = Instant::now();
                    if setup
                        .backend
                        .post(&mut conn, &txn, guard.then(|| aml(user)))
                        .await
                    {
                        lat.push(started.elapsed().as_micros() as u64);
                    } else {
                        errors += 1;
                    }
                }
                (lat, errors)
            })
        })
        .collect();
    let started = Instant::now();
    tokio::time::sleep(Duration::from_secs(secs)).await;
    stop.store(true, Ordering::Relaxed);
    let (mut lat, mut errors) = (Vec::new(), 0);
    for t in tasks {
        let (l, e) = t.await.unwrap();
        lat.extend(l);
        errors += e;
    }
    let elapsed = started.elapsed().as_secs_f64();
    lat.sort_unstable();
    let pct = |p: f64| {
        lat.get(((lat.len() as f64 * p) as usize).min(lat.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
    };
    Sample {
        ops_per_sec: lat.len() as f64 / elapsed,
        p50_us: pct(0.50),
        p99_us: pct(0.99),
        errors,
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let a = args();
    let max = *a.concurrency.iter().max().unwrap();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = PgPoolOptions::new()
        .max_connections(max as u32 + 4)
        .connect(&url)
        .await
        .unwrap();
    let pg = PostgresLedger::new(pool);
    pg.migrate().await.unwrap();
    let cfg = |cluster| TbConfig {
        request_timeout: Duration::from_secs(30),
        ..TbConfig::new(cluster, "bench")
    };
    let live = LiveTb::connect(
        std::env::var("TB_CLUSTER_ID").map_or(0, |c| c.parse().expect("TB_CLUSTER_ID")),
        &std::env::var("TB_ADDRESS").unwrap_or_else(|_| "3000".into()),
        a.sessions,
    )
    .expect("TigerBeetle client");
    let live_cluster = live.cluster_id();
    let tb = TbLedger::new(live, cfg(live_cluster));
    tb.ensure_control_accounts().await.unwrap();
    let sim = TbLedger::new(SimTb::new(), cfg(0));
    sim.ensure_control_accounts().await.unwrap();

    eprintln!("setting up {} wallets per backend…", a.wallets);
    let mut setups = Vec::new();
    for backend in [
        Backend::Postgres(pg.clone()),
        Backend::Hybrid(HybridLedger::new(tb.clone(), pg.clone())),
        Backend::HybridSim(HybridLedger::new(sim.clone(), pg.clone())),
        Backend::Direct(tb.clone()),
    ] {
        setups.push(Arc::new(setup(backend, &pg, a.wallets).await));
    }
    for s in &setups {
        measure(
            s,
            &pg,
            4,
            1,
            a.aml && !matches!(s.backend, Backend::Direct(_)),
            a.recipients,
        )
        .await;
    }

    let mut samples: Vec<Vec<Vec<Sample>>> = (0..setups.len())
        .map(|_| (0..a.concurrency.len()).map(|_| Vec::new()).collect())
        .collect();
    for round in 0..a.rounds {
        for (ci, &c) in a.concurrency.iter().enumerate() {
            for k in 0..setups.len() {
                let bi = (k + round + ci) % setups.len();
                let s = &setups[bi];
                let guard = a.aml && !matches!(s.backend, Backend::Direct(_));
                let sample = measure(s, &pg, c, a.seconds, guard, a.recipients).await;
                eprintln!(
                    "round {round} c={c:<3} {:<10} {:>9.0} ops/s  p50 {:>6} us  p99 {:>6} us  errors {}",
                    s.backend.name(),
                    sample.ops_per_sec,
                    sample.p50_us,
                    sample.p99_us,
                    sample.errors
                );
                samples[bi][ci].push(sample);
            }
        }
    }

    println!(
        "| backend | concurrency | ops/s (median) | ops/s (min-max) | p50 ms | p99 ms | errors |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|");
    for (bi, s) in setups.iter().enumerate() {
        for (ci, &c) in a.concurrency.iter().enumerate() {
            let runs = &samples[bi][ci];
            let ops: Vec<f64> = runs.iter().map(|r| r.ops_per_sec).collect();
            let (lo, hi) = ops
                .iter()
                .fold((f64::MAX, 0f64), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
            println!(
                "| {} | {c} | {:.0} | {:.0}-{:.0} | {:.2} | {:.2} | {} |",
                s.backend.name(),
                median(ops.clone()),
                lo,
                hi,
                median(runs.iter().map(|r| r.p50_us as f64).collect()) / 1000.0,
                median(runs.iter().map(|r| r.p99_us as f64).collect()) / 1000.0,
                runs.iter().map(|r| r.errors).sum::<usize>()
            );
        }
    }
}
