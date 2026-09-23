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
