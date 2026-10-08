use crate::args::{Config, Mix, MixOp, Workload};

/// SplitMix64: tiny, fast, seedable; plenty for picking accounts and amounts.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    pub fn range(&mut self, (lo, hi): (i64, i64)) -> i64 {
        lo + (self.next_u64() % (hi - lo + 1) as u64) as i64
    }

    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// P(rank k) ∝ 1 / k^s over n ranks, by inverse CDF.
pub struct Zipf {
    cdf: Vec<f64>,
}

impl Zipf {
    pub fn new(n: usize, s: f64) -> Self {
        let mut cdf = Vec::with_capacity(n);
        let mut acc = 0.0;
        for k in 1..=n {
            acc += 1.0 / (k as f64).powf(s);
            cdf.push(acc);
        }
        cdf.iter_mut().for_each(|c| *c /= acc);
        Self { cdf }
    }

    pub fn sample(&self, rng: &mut Rng) -> usize {
        let u = rng.unit();
        self.cdf.partition_point(|c| *c < u).min(self.cdf.len() - 1)
    }
}

/// One step a worker performs. Users are indices into the run's wallet list.
#[derive(Clone, Copy, Debug)]
pub enum Planned {
    Transfer {
        from: usize,
        to: usize,
        amount: i64,
    },
    Balance {
        user: usize,
    },
    Statement {
        user: usize,
    },
    Fx {
        user: usize,
        to_usd: bool,
        amount: i64,
    },
    Check {
        merchant: usize,
        payer: usize,
        amount: i64,
    },
}

pub struct Planner {
    workload: Workload,
    mix: Mix,
    users: usize,
    merchants: usize,
    zipf: Option<Zipf>,
    amount: (i64, i64),
}

impl Planner {
    pub fn new(cfg: &Config) -> Self {
        Self {
            workload: cfg.workload,
            mix: cfg.mix.clone(),
            users: cfg.users,
            merchants: cfg.merchants,
            zipf: (cfg.workload == Workload::Hot).then(|| Zipf::new(cfg.merchants, cfg.zipf)),
            amount: cfg.amount,
        }
    }

    /// (payer, payee) for the workload: uniform distinct pairs; many payers to a few
    /// Zipf-ranked merchants (users 0..merchants); or everyone paid by user 0.
    pub fn pair(&self, rng: &mut Rng) -> (usize, usize) {
        match (self.workload, &self.zipf) {
            (Workload::Hot, Some(z)) => (
                self.merchants + rng.below(self.users - self.merchants),
                z.sample(rng),
            ),
            (Workload::Contention, _) => (0, 1 + rng.below(self.users - 1)),
            _ => {
                let a = rng.below(self.users);
                let b = rng.below(self.users - 1);
                (a, if b >= a { b + 1 } else { b })
            }
        }
    }

    pub fn next(&self, rng: &mut Rng) -> Planned {
        let op = self.mix.pick(rng.below(self.mix.total() as usize) as u32);
        let amount = rng.range(self.amount);
        match op {
            MixOp::Transfer => {
                let (from, to) = self.pair(rng);
                Planned::Transfer { from, to, amount }
            }
            MixOp::Balance => Planned::Balance {
                user: self.pair(rng).0,
            },
            MixOp::Statement => Planned::Statement {
                user: self.pair(rng).1,
            },
            MixOp::Fx => Planned::Fx {
                user: rng.below(self.users),
                to_usd: rng.below(2) == 0,
                amount,
            },
            MixOp::Check => {
                let (payer, merchant) = self.pair(rng);
                Planned::Check {
                    merchant,
                    payer,
                    amount,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zipf_is_skewed_and_in_range() {
        let z = Zipf::new(10, 1.1);
        let mut rng = Rng::new(7);
        let mut counts = [0usize; 10];
        for _ in 0..100_000 {
            counts[z.sample(&mut rng)] += 1;
        }
        assert!(counts[0] > counts[1] && counts[1] > counts[9]);
        // rank 1 carries 1 / H(10, 1.1) ≈ 0.37 of the mass.
        assert!((33_000..41_000).contains(&counts[0]), "{counts:?}");
    }

    fn planner(workload: Workload) -> Planner {
        Planner {
            workload,
            mix: Mix(vec![(MixOp::Transfer, 1)]),
            users: 5,
            merchants: 2,
            zipf: (workload == Workload::Hot).then(|| Zipf::new(2, 1.0)),
            amount: (3, 5),
        }
    }

    #[test]
    fn pairs_follow_the_workload() {
        let mut rng = Rng::new(1);
        let (uniform, hot, contention) = (
            planner(Workload::Uniform),
            planner(Workload::Hot),
            planner(Workload::Contention),
        );
        let mut paid = [false; 5];
        for _ in 0..1000 {
            let (a, b) = uniform.pair(&mut rng);
            assert!(a != b && a < 5 && b < 5);
            paid[b] = true;
            let (a, b) = hot.pair(&mut rng);
            assert!((2..5).contains(&a) && b < 2, "hot pays merchants 0..2");
            let (a, b) = contention.pair(&mut rng);
            assert!(a == 0 && (1..5).contains(&b));
            let Planned::Transfer { amount, .. } = uniform.next(&mut rng) else {
                panic!("transfer-only mix");
            };
            assert!((3..=5).contains(&amount));
        }
        assert!(paid.iter().all(|p| *p));
    }
}
