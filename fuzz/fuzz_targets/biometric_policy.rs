//! `biometric::decide` against the DESIGN §20.4 rule, and the gallery-scaled threshold:
//! - never Match below the threshold, and a NaN threshold or margin fails closed;
//! - ambiguity: Match only when no other person is at or above the threshold within the
//!   margin of the best — and never on an exact tie between two people;
//! - candidate order is irrelevant; raising the threshold never turns a Match into a Match
//!   for someone else or into an Ambiguous, lowering it never loses the matched person;
//! - `identification_threshold` is monotonic in gallery size and scale, and equals the base
//!   for galleries of 0 and 1.
#![no_main]

use std::collections::BTreeMap;

use arbitrary::Arbitrary;
use biometric::{decide, identification_threshold, Candidate, Decision, MatchPolicy};
use libfuzzer_sys::fuzz_target;
use uuid::Uuid;

/// Small integers make ties and exact-threshold scores common; raw bits reach NaN, ±inf and
/// subnormals.
#[derive(Arbitrary, Debug, Clone, Copy)]
enum Num {
    Grid(i8),
    Raw(f64),
}

impl Num {
    fn get(self) -> f64 {
        match self {
            Num::Grid(k) => k as f64,
            Num::Raw(f) => f,
        }
    }
}

#[derive(Arbitrary, Debug)]
struct Input {
    candidates: Vec<(u8, Num)>,
    threshold: Num,
    margin: Num,
    higher_by: Num,
    base: Num,
    scales: (Num, Num),
    galleries: (u64, u64),
}

fn spec(candidates: &[Candidate], p: MatchPolicy) -> Decision {
    let mut best: BTreeMap<Uuid, f64> = BTreeMap::new();
    for c in candidates.iter().filter(|c| c.score.is_finite()) {
        let b = best.entry(c.subject).or_insert(c.score);
        *b = b.max(c.score);
    }
    let mut ranked: Vec<(Uuid, f64)> = best.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let Some(&(top, score)) = ranked.first() else {
        return Decision::NoMatch;
    };
    // Scores are finite here; a NaN policy value must refuse (threshold) or doubt (margin).
    if p.threshold.is_nan() || score < p.threshold {
        return Decision::NoMatch;
    }
    if let Some(&(runner_up, second)) = ranked.get(1) {
        let close = p.margin.is_nan() || score - second < p.margin;
        if second >= p.threshold && (second == score || close) {
            return Decision::Ambiguous {
                best: top,
                runner_up,
            };
        }
    }
    Decision::Match {
        subject: top,
        score,
    }
}

fn sane(p: MatchPolicy) -> bool {
    p.threshold.is_finite() && p.margin.is_finite() && p.margin >= 0.0
}

fn check_monotone(candidates: &[Candidate], low: MatchPolicy, high: MatchPolicy) {
    let (lo, hi) = (decide(candidates, low), decide(candidates, high));
    if let Decision::Match { subject, .. } = lo {
        assert!(
            matches!(hi, Decision::NoMatch)
                || matches!(hi, Decision::Match { subject: s, .. } if s == subject),
            "raising the threshold {low:?} -> {high:?} turned {lo:?} into {hi:?}"
        );
    }
    if let Decision::Match { subject, .. } = hi {
        assert!(
            matches!(lo, Decision::Match { subject: s, .. } if s == subject)
                || matches!(lo, Decision::Ambiguous { best, .. } if best == subject),
            "lowering the threshold {high:?} -> {low:?} turned {hi:?} into {lo:?}"
        );
    }
}

fn check_threshold(base: f64, scales: (f64, f64), galleries: (u64, u64)) {
    let (s1, s2) = (scales.0.min(scales.1), scales.0.max(scales.1));
    let (g1, g2) = (galleries.0.min(galleries.1), galleries.0.max(galleries.1));
    for s in [s1, s2] {
        let t = |g| identification_threshold(base, s, g);
        // Any input, even one the config rejects, must not panic.
        let _ = t(g1);
        if !(base.is_finite() && s.is_finite() && s >= 0.0) {
            continue;
        }
        assert_eq!(t(0), base);
        assert_eq!(t(1), base);
        assert!(
            t(g1) <= t(g2),
            "threshold fell from {} to {} ({g1} -> {g2})",
            t(g1),
            t(g2)
        );
        assert!(t(g2) >= base);
        let p = MatchPolicy {
            threshold: base,
            margin: 7.0,
        }
        .for_gallery(s, g2);
        assert_eq!((p.threshold, p.margin), (t(g2), 7.0));
    }
    if base.is_finite() && s1.is_finite() && s1 >= 0.0 && s2.is_finite() {
        for g in [g1, g2] {
            assert!(identification_threshold(base, s1, g) <= identification_threshold(base, s2, g));
        }
    }
}

fuzz_target!(|input: Input| {
    let candidates: Vec<Candidate> = input
        .candidates
        .iter()
        .map(|(s, score)| Candidate {
            subject: Uuid::from_u128((s % 6) as u128),
            score: score.get(),
        })
        .collect();
    let policy = MatchPolicy {
        threshold: input.threshold.get(),
        margin: input.margin.get(),
    };

    let got = decide(&candidates, policy);
    assert_eq!(got, spec(&candidates, policy), "{candidates:?} {policy:?}");
    if let Decision::Match { score, .. } = got {
        assert!(score >= policy.threshold, "Match below the threshold");
    }
    let reversed: Vec<Candidate> = candidates.iter().rev().copied().collect();
    assert_eq!(
        decide(&reversed, policy),
        got,
        "candidate order changed the decision"
    );

    let higher = MatchPolicy {
        threshold: policy.threshold + input.higher_by.get().abs(),
        ..policy
    };
    if sane(policy) && sane(higher) {
        check_monotone(&candidates, policy, higher);
    }

    check_threshold(
        input.base.get(),
        (input.scales.0.get(), input.scales.1.get()),
        input.galleries,
    );
});
