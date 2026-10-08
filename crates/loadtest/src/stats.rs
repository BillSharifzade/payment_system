use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use serde::Serialize;
use uuid::Uuid;

/// What a latency sample measures: one request (a check is a create and a pay).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Transfer,
    Fx,
    CheckCreate,
    CheckPay,
    Balance,
    Statement,
}

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Op::Transfer => "transfer",
            Op::Fx => "fx",
            Op::CheckCreate => "check_create",
            Op::CheckPay => "check_pay",
            Op::Balance => "balance",
            Op::Statement => "statement",
        }
    }
}

/// A money movement the client believes in, with its expected effect on the run's wallets.
#[derive(Clone, Debug)]
pub struct Posted {
    pub id: Uuid,
    pub legs: Vec<(Uuid, i64)>,
}

/// The client-side truth the verifier holds the database to.
#[derive(Debug)]
pub enum Event {
    /// Acknowledged (201, or a replay of a posted key).
    Posted(Posted),
    /// Definitively refused (4xx, or a rolled-back attempt): must not exist in the ledger.
    Rejected(Uuid),
    /// Outcome unknown after every retry (5xx / transport): either is acceptable.
    Unresolved(Posted),
}

pub struct Outcome {
    pub result: Result<(), String>,
    pub retries: u32,
    pub event: Option<Event>,
}

impl Outcome {
    pub fn ok() -> Self {
        Self {
            result: Ok(()),
            retries: 0,
            event: None,
        }
    }
}

pub struct Step {
    pub op: Op,
    pub started: Instant,
    pub finished: Instant,
    pub outcome: Outcome,
}

struct Sample {
    at_ms: u32,
    latency_us: u32,
    op: Op,
    ok: bool,
}

/// Per-worker raw samples (12 bytes each), merged and summarised after the run.
#[derive(Default)]
pub struct Recorder {
    samples: Vec<Sample>,
    errors: BTreeMap<String, u64>,
    retries: u64,
    pub posted: Vec<Posted>,
    pub rejected: Vec<Uuid>,
    pub unresolved: Vec<Posted>,
}

impl Recorder {
    /// Ledger events are kept whenever they happen (the verifier needs all of them); latency
    /// and errors only for steps that started inside the measured window.
    pub fn record(&mut self, measure_from: Instant, step: Step) {
        match step.outcome.event {
            Some(Event::Posted(p)) => self.posted.push(p),
            Some(Event::Rejected(id)) => self.rejected.push(id),
            Some(Event::Unresolved(p)) => self.unresolved.push(p),
            None => {}
        }
        let Some(at) = step.started.checked_duration_since(measure_from) else {
            return;
        };
        self.retries += step.outcome.retries as u64;
        if let Err(code) = &step.outcome.result {
            *self
                .errors
                .entry(format!("{} {code}", step.op.name()))
                .or_default() += 1;
        }
        self.samples.push(Sample {
            at_ms: at.as_millis().min(u32::MAX as u128) as u32,
            latency_us: step
                .finished
                .duration_since(step.started)
                .as_micros()
                .clamp(1, u32::MAX as u128) as u32,
            op: step.op,
            ok: step.outcome.result.is_ok(),
        });
    }

    pub fn merge(&mut self, other: Recorder) {
        self.samples.extend(other.samples);
        for (k, v) in other.errors {
            *self.errors.entry(k).or_default() += v;
        }
        self.retries += other.retries;
        self.posted.extend(other.posted);
        self.rejected.extend(other.rejected);
        self.unresolved.extend(other.unresolved);
    }
}

#[derive(Serialize, Clone, Copy, Default)]
pub struct Latency {
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub p999: f64,
    pub max: f64,
    pub mean: f64,
}

#[derive(Serialize)]
pub struct OpReport {
    pub ok: u64,
    pub errors: u64,
    pub ok_per_sec: f64,
    /// Successful requests only, in milliseconds.
    pub latency_ms: Latency,
}

#[derive(Serialize)]
pub struct Tick {
    pub t: f64,
    pub ok_per_sec: f64,
    pub errors: u64,
    pub p50_ms: f64,
    pub p99_ms: f64,
}

#[derive(Serialize)]
pub struct Summary {
    pub measured_secs: f64,
    pub ops: BTreeMap<Op, OpReport>,
    pub total: OpReport,
    pub errors: BTreeMap<String, u64>,
    pub retries: u64,
    pub timeline: Vec<Tick>,
}

fn histogram() -> Histogram<u64> {
    Histogram::new_with_bounds(1, 3_600_000_000, 3).expect("valid histogram bounds")
}

fn latency(h: &Histogram<u64>) -> Latency {
    if h.is_empty() {
        return Latency::default();
    }
    let ms = |us: u64| us as f64 / 1000.0;
    Latency {
        p50: ms(h.value_at_quantile(0.50)),
        p90: ms(h.value_at_quantile(0.90)),
        p99: ms(h.value_at_quantile(0.99)),
        p999: ms(h.value_at_quantile(0.999)),
        max: ms(h.max()),
        mean: h.mean() / 1000.0,
    }
}

impl Recorder {
    pub fn summarise(&self, measured: Duration, interval: Duration) -> Summary {
        let secs = measured.as_secs_f64();
        let mut per_op: BTreeMap<Op, (Histogram<u64>, u64)> = BTreeMap::new();
        let mut all = (histogram(), 0u64);
        let tick_ms = interval.as_millis().max(1) as u32;
        let ticks = (measured.as_millis() as u32).div_ceil(tick_ms).max(1) as usize;
        let mut timeline: Vec<(Histogram<u64>, u64)> =
            (0..ticks).map(|_| (histogram(), 0)).collect();
        for s in &self.samples {
            let (h, errs) = per_op.entry(s.op).or_insert_with(|| (histogram(), 0));
            let tick = &mut timeline[((s.at_ms / tick_ms) as usize).min(ticks - 1)];
            if s.ok {
                let v = s.latency_us as u64;
                h.saturating_record(v);
                all.0.saturating_record(v);
                tick.0.saturating_record(v);
            } else {
                *errs += 1;
                all.1 += 1;
                tick.1 += 1;
            }
        }
        let report = |h: &Histogram<u64>, errors: u64| OpReport {
            ok: h.len(),
            errors,
            ok_per_sec: h.len() as f64 / secs,
            latency_ms: latency(h),
        };
        Summary {
            measured_secs: secs,
            ops: per_op
                .iter()
                .map(|(op, (h, e))| (*op, report(h, *e)))
                .collect(),
            total: report(&all.0, all.1),
            errors: self.errors.clone(),
            retries: self.retries,
            timeline: timeline
                .iter()
                .enumerate()
                .map(|(i, (h, errors))| {
                    let lat = latency(h);
                    let width = interval
                        .as_secs_f64()
                        .min(secs - i as f64 * interval.as_secs_f64());
                    Tick {
                        t: i as f64 * interval.as_secs_f64(),
                        ok_per_sec: h.len() as f64 / width.max(1e-9),
                        errors: *errors,
                        p50_ms: lat.p50,
                        p99_ms: lat.p99,
                    }
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(op: Op, started: Instant, ms: u64, result: Result<(), String>) -> Step {
        Step {
            op,
            started,
            finished: started + Duration::from_millis(ms),
            outcome: Outcome {
                result,
                retries: 0,
                event: Some(Event::Rejected(Uuid::nil())),
            },
        }
    }

    #[test]
    fn warmup_is_kept_for_the_ledger_but_not_measured() {
        let t0 = Instant::now();
        let from = t0 + Duration::from_secs(1);
        let mut rec = Recorder::default();
        rec.record(from, step(Op::Transfer, t0, 5, Ok(())));
        for i in 0..100 {
            rec.record(from, step(Op::Transfer, from, 1 + i, Ok(())));
        }
        rec.record(from, step(Op::Balance, from, 1, Err("http_500".into())));
        let s = rec.summarise(Duration::from_secs(2), Duration::from_secs(1));
        assert_eq!(rec.rejected.len(), 102, "every event reaches the verifier");
        let t = &s.ops[&Op::Transfer];
        assert_eq!((t.ok, t.errors), (100, 0));
        assert_eq!(t.ok_per_sec, 50.0);
        assert!(
            (t.latency_ms.p50 - 50.0).abs() < 1.0,
            "{}",
            t.latency_ms.p50
        );
        assert!((t.latency_ms.max - 100.0).abs() < 0.2);
        assert_eq!(s.errors["balance http_500"], 1);
        assert_eq!(s.total.errors, 1);
        assert_eq!(s.timeline.len(), 2);
        assert_eq!(s.timeline[0].ok_per_sec, 100.0);
    }
}
