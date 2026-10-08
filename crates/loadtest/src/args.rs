use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde::Serialize;

pub const USAGE: &str = "\
payment-loadtest — drive the money path and prove the ledger is intact afterwards

USAGE: payment-loadtest [--flag value]...
       payment-loadtest summarize REPORT.json...   (median/min–max table, grouped by --label)

  --mode http|direct        http: end to end through payment-server (default);
                            direct: in-process PostgresLedger::post_on with the handler's guard
  --workload W              uniform (random pairs, default) | hot (Zipf-skewed merchants)
                            | contention (one payer) | mixed (uniform + balance/statement reads)
  --mix op=w,...            op weights; ops: transfer balance statement fx check
                            (default transfer=1; mixed: transfer=2,balance=1,statement=1)
  --concurrency N           workers (default 64)
  --duration D              measured time, e.g. 30s, 2m (default 30s)
  --warmup D                unmeasured lead-in (default 5s)
  --rate R                  open loop: R ops/s in total, latency from the intended start
                            (default: closed loop, back to back)
  --users N                 wallets (default 1000)
  --merchants N             hot workload: recipients (default 10)
  --zipf S                  hot workload: Zipf exponent (default 1.1)
  --amount LO..HI           transfer amount range in minor units (default 1000..50000)
  --fund MINOR              deposit per wallet (default 100000000)
  --fee-bps N               transfer fee; direct mode, or the spawned server (default 30)
  --pool-size N             DB connections: direct workers, or the spawned server (default 32)
  --database-url URL        setup, verification, pg stats (default $DATABASE_URL)
  --base-url URL            a running payment-server (http mode)
  --server-bin PATH         spawn this payment-server instead (http mode)
  --server-port N           port for the spawned server (default 18080; metrics on N+1)
  --server-env K=V          extra env for the spawned server (repeatable)
  --server-log PATH         spawned server's output (default /dev/null)
  --redis-url URL           give the spawned server Redis rate limiting
  --sign                    register a P-256 device per user and sign every money request
  --retries N               resend with the same key on 502/503/504/transport errors (default 5)
  --interval D              timeline resolution (default 1s)
  --seed N                  RNG seed (default: from the clock; printed)
  --report PATH             write the JSON report here
  --label TEXT              free-form tag stored in the report
  --pg-stats                diff pg_stat_statements / pg_stat_wal / pg_stat_database over the run
  --allow-errors            exit 0 even if some requests failed (invariants still enforced)
  --p99-ceiling-ms MS       fail if any op's p99 exceeds this
  --help
";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Http,
    Direct,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Workload {
    Uniform,
    Hot,
    Contention,
    Mixed,
}

/// What the mix chooses between. A `check` is two requests: create, then pay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MixOp {
    Transfer,
    Balance,
    Statement,
    Fx,
    Check,
}

impl FromStr for MixOp {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Ok(match s {
            "transfer" => MixOp::Transfer,
            "balance" => MixOp::Balance,
            "statement" => MixOp::Statement,
            "fx" => MixOp::Fx,
            "check" => MixOp::Check,
            _ => return Err(format!("unknown op {s:?} in --mix")),
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Mix(pub Vec<(MixOp, u32)>);

impl Mix {
    pub fn has(&self, op: MixOp) -> bool {
        self.0.iter().any(|(o, w)| *o == op && *w > 0)
    }

    pub fn total(&self) -> u32 {
        self.0.iter().map(|(_, w)| w).sum()
    }

    /// `ticket` uniform in 0..total().
    pub fn pick(&self, mut ticket: u32) -> MixOp {
        for (op, w) in &self.0 {
            if ticket < *w {
                return *op;
            }
            ticket -= w;
        }
        unreachable!("ticket beyond the mix total")
    }
}

impl FromStr for Mix {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let mut weights = BTreeMap::new();
        for part in s.split(',') {
            let (op, w) = part
                .split_once('=')
                .ok_or_else(|| format!("--mix entry {part:?} is not op=weight"))?;
            let w: u32 = w
                .parse()
                .map_err(|_| format!("--mix weight {w:?} is not a number"))?;
            if weights.insert(op.parse::<MixOp>()?, w).is_some() {
                return Err(format!("--mix names {op} twice"));
            }
        }
        let mix = Mix(weights.into_iter().collect());
        if mix.total() == 0 {
            return Err("--mix needs a positive weight".into());
        }
        Ok(mix)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Config {
    pub mode: Mode,
    pub workload: Workload,
    pub mix: Mix,
    pub concurrency: usize,
    #[serde(serialize_with = "secs")]
    pub duration: Duration,
    #[serde(serialize_with = "secs")]
    pub warmup: Duration,
    pub rate: Option<f64>,
    pub users: usize,
    pub merchants: usize,
    pub zipf: f64,
    pub amount: (i64, i64),
    pub fund: i64,
    pub fee_bps: u32,
    pub pool_size: u32,
    #[serde(skip)]
    pub database_url: String,
    pub base_url: Option<String>,
    pub server_bin: Option<PathBuf>,
    pub server_port: u16,
    pub server_env: Vec<(String, String)>,
    #[serde(skip)]
    pub server_log: PathBuf,
    #[serde(skip)]
    pub redis_url: Option<String>,
    pub sign: bool,
    pub retries: u32,
    #[serde(serialize_with = "secs")]
    pub interval: Duration,
    pub seed: u64,
    #[serde(skip)]
    pub report: Option<PathBuf>,
    pub label: Option<String>,
    pub pg_stats: bool,
    pub allow_errors: bool,
    pub p99_ceiling_ms: Option<f64>,
}

fn secs<S: serde::Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64(d.as_secs_f64())
}

pub fn parse_duration(raw: &str) -> Result<Duration, String> {
    let bad = || format!("{raw:?} is not a duration (e.g. 500ms, 30s, 2m)");
    let (num, unit) = match raw.find(|c: char| !c.is_ascii_digit() && c != '.') {
        Some(i) => raw.split_at(i),
        None => (raw, "s"),
    };
    let n: f64 = num.parse().map_err(|_| bad())?;
    let secs = match unit {
        "ms" => n / 1000.0,
        "s" => n,
        "m" => n * 60.0,
        _ => return Err(bad()),
    };
    if !secs.is_finite() || secs < 0.0 {
        return Err(bad());
    }
    Ok(Duration::from_secs_f64(secs))
}

fn num<T: FromStr>(flag: &str, raw: &str) -> Result<T, String> {
    raw.parse()
        .map_err(|_| format!("{flag}={raw:?} is not a valid value"))
}

const FLAGS: &[&str] = &["--sign", "--pg-stats", "--allow-errors", "--help"];

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Config>, String> {
    let mut values: Vec<(String, String)> = Vec::new();
    let mut flags = Vec::new();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if !arg.starts_with("--") {
            return Err(format!("unexpected argument {arg:?}"));
        }
        if FLAGS.contains(&arg.as_str()) {
            flags.push(arg);
            continue;
        }
        let (k, v) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => {
                let v = it.next().ok_or_else(|| format!("{arg} needs a value"))?;
                (arg, v)
            }
        };
        values.push((k, v));
    }
    if flags.iter().any(|f| f == "--help") {
        return Ok(None);
    }

    let mut cfg = Config {
        mode: Mode::Http,
        workload: Workload::Uniform,
        mix: Mix(vec![(MixOp::Transfer, 1)]),
        concurrency: 64,
        duration: Duration::from_secs(30),
        warmup: Duration::from_secs(5),
        rate: None,
        users: 1000,
        merchants: 10,
        zipf: 1.1,
        amount: (1_000, 50_000),
        fund: 100_000_000,
        fee_bps: 30,
        pool_size: 32,
        database_url: std::env::var("DATABASE_URL").unwrap_or_default(),
        base_url: None,
        server_bin: None,
        server_port: 18080,
        server_env: Vec::new(),
        server_log: PathBuf::from("/dev/null"),
        redis_url: None,
        sign: flags.iter().any(|f| f == "--sign"),
        retries: 5,
        interval: Duration::from_secs(1),
        seed: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1),
        report: None,
        label: None,
        pg_stats: flags.iter().any(|f| f == "--pg-stats"),
        allow_errors: flags.iter().any(|f| f == "--allow-errors"),
        p99_ceiling_ms: None,
    };
    let mut mix = None;
    for (k, v) in &values {
        let v = v.as_str();
        match k.as_str() {
            "--mode" => {
                cfg.mode = match v {
                    "http" => Mode::Http,
                    "direct" => Mode::Direct,
                    _ => return Err(format!("--mode={v:?} must be http or direct")),
                }
            }
            "--workload" => {
                cfg.workload = match v {
                    "uniform" => Workload::Uniform,
                    "hot" => Workload::Hot,
                    "contention" => Workload::Contention,
                    "mixed" => Workload::Mixed,
                    _ => return Err(format!("--workload={v:?} is not a workload")),
                }
            }
            "--mix" => mix = Some(v.parse::<Mix>()?),
            "--concurrency" => cfg.concurrency = num(k, v)?,
            "--duration" => cfg.duration = parse_duration(v)?,
            "--warmup" => cfg.warmup = parse_duration(v)?,
            "--rate" => cfg.rate = Some(num(k, v)?),
            "--users" => cfg.users = num(k, v)?,
            "--merchants" => cfg.merchants = num(k, v)?,
            "--zipf" => cfg.zipf = num(k, v)?,
            "--amount" => {
                let (lo, hi) = v
                    .split_once("..")
                    .ok_or_else(|| format!("--amount={v:?} must be LO..HI"))?;
                cfg.amount = (num(k, lo)?, num(k, hi)?);
            }
            "--fund" => cfg.fund = num(k, v)?,
            "--fee-bps" => cfg.fee_bps = num(k, v)?,
            "--pool-size" => cfg.pool_size = num(k, v)?,
            "--database-url" => cfg.database_url = v.to_string(),
            "--base-url" => cfg.base_url = Some(v.trim_end_matches('/').to_string()),
            "--server-bin" => cfg.server_bin = Some(PathBuf::from(v)),
            "--server-port" => cfg.server_port = num(k, v)?,
            "--server-env" => {
                let (ek, ev) = v
                    .split_once('=')
                    .ok_or_else(|| format!("--server-env={v:?} must be KEY=VALUE"))?;
                cfg.server_env.push((ek.to_string(), ev.to_string()));
            }
            "--server-log" => cfg.server_log = PathBuf::from(v),
            "--redis-url" => cfg.redis_url = Some(v.to_string()),
            "--retries" => cfg.retries = num(k, v)?,
            "--interval" => cfg.interval = parse_duration(v)?,
            "--seed" => cfg.seed = num(k, v)?,
            "--report" => cfg.report = Some(PathBuf::from(v)),
            "--label" => cfg.label = Some(v.to_string()),
            "--p99-ceiling-ms" => cfg.p99_ceiling_ms = Some(num(k, v)?),
            _ => return Err(format!("unknown flag {k}")),
        }
    }
    cfg.mix = mix.unwrap_or_else(|| match cfg.workload {
        Workload::Mixed => Mix(vec![
            (MixOp::Transfer, 2),
            (MixOp::Balance, 1),
            (MixOp::Statement, 1),
        ]),
        _ => Mix(vec![(MixOp::Transfer, 1)]),
    });
    validate(&cfg)?;
    Ok(Some(cfg))
}

fn validate(c: &Config) -> Result<(), String> {
    if c.database_url.is_empty() {
        return Err("--database-url (or DATABASE_URL) is required".into());
    }
    if c.concurrency == 0 || c.concurrency > 10_000 {
        return Err("--concurrency must be 1..=10000".into());
    }
    if c.duration.is_zero() {
        return Err("--duration must be positive".into());
    }
    if c.interval < Duration::from_millis(100) {
        return Err("--interval must be at least 100ms".into());
    }
    if c.users < 2 {
        return Err("--users must be at least 2".into());
    }
    if c.workload == Workload::Hot && (c.merchants == 0 || c.merchants >= c.users) {
        return Err("--merchants must be 1..users-1 for the hot workload".into());
    }
    if !(c.zipf.is_finite() && c.zipf >= 0.0) {
        return Err("--zipf must be a non-negative number".into());
    }
    if c.amount.0 <= 0 || c.amount.0 > c.amount.1 {
        return Err("--amount needs 0 < LO <= HI".into());
    }
    if c.fund < c.amount.1 {
        return Err("--fund must cover at least the largest amount".into());
    }
    if c.fee_bps > api::FeeConfig::MAX_BPS {
        return Err("--fee-bps must be at most 10000".into());
    }
    if c.fee_bps > 0 && (c.amount.0 as i128 * c.fee_bps as i128) / 10_000 >= c.amount.0 as i128 {
        return Err("--amount LO does not cover the fee".into());
    }
    if c.pool_size == 0 {
        return Err("--pool-size must be positive".into());
    }
    if c.rate.is_some_and(|r| !(r.is_finite() && r > 0.0)) {
        return Err("--rate must be positive".into());
    }
    match c.mode {
        Mode::Http => {
            if c.base_url.is_some() == c.server_bin.is_some() {
                return Err("http mode needs exactly one of --base-url or --server-bin".into());
            }
        }
        Mode::Direct => {
            for op in [MixOp::Statement, MixOp::Fx, MixOp::Check] {
                if c.mix.has(op) {
                    return Err(format!(
                        "{op:?} is http-only; direct mode runs transfer and balance"
                    ));
                }
            }
            if c.sign || c.base_url.is_some() || c.server_bin.is_some() {
                return Err("--sign, --base-url and --server-bin are http-mode flags".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn parses_flags_strictly() {
        let cfg = parse(args(
            "--database-url postgres://x --mode direct --workload hot --merchants 5 \
             --amount=10..20 --duration 2m --warmup 500ms --mix transfer=3,balance=1",
        ))
        .unwrap()
        .unwrap();
        assert_eq!(cfg.mode, Mode::Direct);
        assert_eq!(cfg.workload, Workload::Hot);
        assert_eq!(cfg.amount, (10, 20));
        assert_eq!(cfg.duration, Duration::from_secs(120));
        assert_eq!(cfg.warmup, Duration::from_millis(500));
        assert_eq!(cfg.mix.total(), 4);
        assert_eq!(cfg.mix.pick(0), MixOp::Transfer);
        assert_eq!(cfg.mix.pick(2), MixOp::Transfer);
        assert_eq!(cfg.mix.pick(3), MixOp::Balance);

        for bad in [
            "--database-url x --base-url http://a --bogus 1",
            "--database-url x --base-url http://a --concurrency many",
            "--database-url x --base-url http://a --duration 5h",
            "--database-url x --base-url http://a --mix transfer=1,transfer=2",
            "--database-url x --base-url http://a --server-bin ./s",
            "--database-url x --mode direct --mix statement=1",
            "--database-url x --base-url http://a --amount 10..5",
            "--database-url x --base-url http://a --workload hot --merchants 1000",
        ] {
            assert!(parse(args(bad)).is_err(), "accepted: {bad}");
        }
        assert!(parse(args("--help")).unwrap().is_none());
    }
}
