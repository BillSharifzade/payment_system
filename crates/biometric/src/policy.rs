use std::collections::HashMap;

use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    pub subject: Uuid,
    pub score: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchPolicy {
    pub threshold: f64,
    pub margin: f64,
}

impl Default for MatchPolicy {
    fn default() -> Self {
        Self {
            threshold: 40.0,
            margin: 10.0,
        }
    }
}

// SourceAFIS calibrates scores as score ≈ −10·log10(FMR): the default threshold 40 is the
// 1:1 operating point FMR = 0.01 %. Searching a gallery of N templates gives N chances for a
// non-mate to clear the bar, so the system-wide false-match rate is ≈ N·FMR. Raising the
// threshold by 10·log10(N) divides the per-comparison FMR by N and keeps 1:N identification
// at the 1:1 false-match rate. `scale` is the engine's points-per-decade (10 for SourceAFIS).
pub const DEFAULT_IDENTIFY_SCALE: f64 = 10.0;

pub fn identification_threshold(base: f64, scale: f64, gallery_size: u64) -> f64 {
    base + scale * (gallery_size.max(1) as f64).log10()
}

impl MatchPolicy {
    pub fn for_gallery(self, scale: f64, gallery_size: u64) -> Self {
        Self {
            threshold: identification_threshold(self.threshold, scale, gallery_size),
            ..self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    Match { subject: Uuid, score: f64 },
    NoMatch,
    Ambiguous { best: Uuid, runner_up: Uuid },
}

pub fn decide(candidates: &[Candidate], policy: MatchPolicy) -> Decision {
    let mut best_per_subject: HashMap<Uuid, f64> = HashMap::new();
    for c in candidates {
        if !c.score.is_finite() {
            continue;
        }
        let slot = best_per_subject
            .entry(c.subject)
            .or_insert(f64::NEG_INFINITY);
        if c.score > *slot {
            *slot = c.score;
        }
    }
    let mut ranked: Vec<(Uuid, f64)> = best_per_subject.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let Some(&(best, best_score)) = ranked.first() else {
        return Decision::NoMatch;
    };
    if best_score < policy.threshold {
        return Decision::NoMatch;
    }
    if let Some(&(runner_up, second)) = ranked.get(1) {
        if second >= policy.threshold && best_score - second < policy.margin {
            return Decision::Ambiguous { best, runner_up };
        }
    }
    Decision::Match {
        subject: best,
        score: best_score,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> MatchPolicy {
        MatchPolicy {
            threshold: 40.0,
            margin: 10.0,
        }
    }

    fn c(subject: Uuid, score: f64) -> Candidate {
        Candidate { subject, score }
    }

    #[test]
    fn empty_and_below_threshold_are_no_match() {
        assert_eq!(decide(&[], policy()), Decision::NoMatch);
        assert_eq!(
            decide(&[c(Uuid::new_v4(), 39.9)], policy()),
            Decision::NoMatch
        );
        assert_eq!(
            decide(&[c(Uuid::new_v4(), f64::NAN)], policy()),
            Decision::NoMatch
        );
    }

    #[test]
    fn clear_winner_matches() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(
            decide(&[c(a, 80.0), c(b, 20.0)], policy()),
            Decision::Match {
                subject: a,
                score: 80.0
            }
        );
    }

    #[test]
    fn two_subjects_above_threshold_within_margin_are_ambiguous() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(
            decide(&[c(a, 55.0), c(b, 50.0)], policy()),
            Decision::Ambiguous {
                best: a,
                runner_up: b
            }
        );
        assert_eq!(
            decide(&[c(a, 61.0), c(b, 50.0)], policy()),
            Decision::Match {
                subject: a,
                score: 61.0
            }
        );
    }

    #[test]
    fn several_fingers_of_one_subject_never_compete_with_each_other() {
        let a = Uuid::new_v4();
        assert_eq!(
            decide(&[c(a, 70.0), c(a, 68.0), c(a, 45.0)], policy()),
            Decision::Match {
                subject: a,
                score: 70.0
            }
        );
    }

    #[test]
    fn identification_threshold_grows_one_scale_per_decade_of_gallery() {
        let t = |n| identification_threshold(40.0, DEFAULT_IDENTIFY_SCALE, n);
        assert_eq!(t(0), 40.0);
        assert_eq!(t(1), 40.0);
        assert!((t(10) - 50.0).abs() < 1e-9);
        assert!((t(10_000) - 80.0).abs() < 1e-9);
        assert!((t(2) - (40.0 + 10.0 * 2f64.log10())).abs() < 1e-9);
        assert_eq!(identification_threshold(40.0, 0.0, 1_000_000), 40.0);
        let mut last = t(1);
        for n in [2, 3, 50, 999, 1_000, 65_536, 10_000_000] {
            assert!(t(n) > last, "threshold must rise with the gallery");
            last = t(n);
        }
    }

    #[test]
    fn scaled_threshold_keeps_the_system_false_match_rate_at_the_1_to_1_point() {
        let fmr = |score: f64| 10f64.powf(-score / 10.0);
        for n in [1u64, 7, 10, 1_234, 100_000, 5_000_000] {
            let system = n as f64 * fmr(identification_threshold(40.0, 10.0, n));
            assert!((system / fmr(40.0) - 1.0).abs() < 1e-9, "n={n}: {system}");
        }
    }

    #[test]
    fn gallery_policy_raises_only_the_threshold() {
        let p = policy().for_gallery(10.0, 100);
        assert_eq!(p.margin, 10.0);
        assert!((p.threshold - 60.0).abs() < 1e-9);
        let a = Uuid::new_v4();
        assert_eq!(decide(&[c(a, 55.0)], p), Decision::NoMatch);
        assert_eq!(
            decide(&[c(a, 55.0)], policy()),
            Decision::Match {
                subject: a,
                score: 55.0
            }
        );
    }

    #[test]
    fn runner_up_below_threshold_does_not_block_a_match() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(
            decide(&[c(a, 42.0), c(b, 39.0)], policy()),
            Decision::Match {
                subject: a,
                score: 42.0
            }
        );
    }
}
