//! Differential test: the same random operation sequence against the pure
//! `InMemoryLedger` (the specification) and `PostgresLedger` must produce the
//! same outcome for every operation — including the exact error — and the same
//! balances, with money conserved per currency on both sides.
//!
//! DIFF_SEEDS=1,2,3 and DIFF_OPS=N widen the search; a failure names its seed.

use ledger::{Account, AccountId, AccountType, Entry, InMemoryLedger, LedgerEngine, Transaction};
use money::{Currency, Money};
use sqlx::postgres::PgPoolOptions;
use storage::{LedgerStore, PostgresLedger, StorageError};
use uuid::Uuid;

/// xorshift64* — deterministic per seed, no extra dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn amount(&mut self, max: i128) -> i128 {
        1 + self.below(max as u64) as i128
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

fn usd() -> Currency {
    Currency::new("USD", 2).unwrap()
}

fn m(minor: i128, currency: Currency) -> Money {
    Money::from_minor(minor, currency)
}

struct Book {
    settlement_tjs: AccountId,
    settlement_usd: AccountId,
    fee_tjs: AccountId,
    fx_tjs: AccountId,
    fx_usd: AccountId,
    tjs: Vec<AccountId>,
    usd: Vec<AccountId>,
}

impl Book {
    fn all(&self) -> Vec<(AccountId, AccountType, Currency)> {
        let mut v = vec![
            (
                self.settlement_tjs,
                AccountType::SystemSettlement,
                Currency::tjs(),
            ),
            (self.settlement_usd, AccountType::SystemSettlement, usd()),
            (self.fee_tjs, AccountType::SystemFeeRevenue, Currency::tjs()),
            (self.fx_tjs, AccountType::SystemFxGainLoss, Currency::tjs()),
            (self.fx_usd, AccountType::SystemFxGainLoss, usd()),
        ];
        v.extend(
            self.tjs
                .iter()
                .map(|a| (*a, AccountType::UserWallet, Currency::tjs())),
        );
        v.extend(
            self.usd
                .iter()
                .map(|a| (*a, AccountType::UserWallet, usd())),
        );
        v
    }
}

fn next_op(rng: &mut Rng, b: &Book, history: &[Transaction]) -> Transaction {
    let tjs = Currency::tjs();
    match rng.below(100) {
        0..=17 => {
            let (settlement, wallets, currency) = if rng.below(3) == 0 {
                (b.settlement_usd, &b.usd, usd())
            } else {
                (b.settlement_tjs, &b.tjs, tjs)
            };
            let amount = rng.amount(100_000);
            Transaction::with_entries(vec![
                Entry::debit(settlement, m(amount, currency)),
                Entry::credit(rng.pick(wallets), m(amount, currency)),
            ])
        }
        18..=39 => {
            let (wallets, currency) = if rng.below(3) == 0 {
                (&b.usd, usd())
            } else {
                (&b.tjs, tjs)
            };
            let amount = rng.amount(40_000);
            Transaction::with_entries(vec![
                Entry::debit(rng.pick(wallets), m(amount, currency)),
                Entry::credit(rng.pick(wallets), m(amount, currency)),
            ])
        }
        40..=54 => {
            let amount = rng.amount(30_000);
            let fee = (amount / 100).max(1);
            Transaction::with_entries(vec![
                Entry::debit(rng.pick(&b.tjs), m(amount + fee, tjs)),
                Entry::credit(rng.pick(&b.tjs), m(amount, tjs)),
                Entry::credit(b.fee_tjs, m(fee, tjs)),
            ])
        }
        55..=69 => {
            // FX at 10.5 TJS per USD; the floored remainder stays in the
            // platform's position, so each currency leg balances on its own.
            let (from, to, from_fx, to_fx, from_cur, to_cur, x) = if rng.below(2) == 0 {
                let x = 105 + rng.amount(40_000);
                (
                    b.tjs.as_slice(),
                    b.usd.as_slice(),
                    b.fx_tjs,
                    b.fx_usd,
                    tjs,
                    usd(),
                    x,
                )
            } else {
                let x = 1 + rng.amount(4_000);
                (
                    b.usd.as_slice(),
                    b.tjs.as_slice(),
                    b.fx_usd,
                    b.fx_tjs,
                    usd(),
                    tjs,
                    x,
                )
            };
            let y = if from_cur == tjs {
                x * 10 / 105
            } else {
                x * 105 / 10
            };
            Transaction::with_entries(vec![
                Entry::debit(rng.pick(from), m(x, from_cur)),
                Entry::credit(from_fx, m(x, from_cur)),
                Entry::debit(to_fx, m(y, to_cur)),
                Entry::credit(rng.pick(to), m(y, to_cur)),
            ])
        }
        70..=79 => {
            // Far more than any wallet holds.
            let amount = 10_000_000 + rng.amount(1_000);
            Transaction::with_entries(vec![
                Entry::debit(rng.pick(&b.tjs), m(amount, tjs)),
                Entry::credit(rng.pick(&b.tjs), m(amount, tjs)),
            ])
        }
        80..=89 if !history.is_empty() => {
            // A retry with the same id: a duplicate if it posted, a fresh
            // attempt if it failed earlier.
            history[rng.below(history.len() as u64) as usize].clone()
        }
        90..=94 => {
            let amount = rng.amount(1_000);
            Transaction::with_entries(vec![
                Entry::debit(b.settlement_tjs, m(amount + 1, tjs)),
                Entry::credit(rng.pick(&b.tjs), m(amount, tjs)),
            ])
        }
        _ => {
            let amount = rng.amount(1_000);
            Transaction::with_entries(vec![
                Entry::debit(b.settlement_usd, m(amount, usd())),
                Entry::credit(rng.pick(&b.usd), m(amount, usd())),
                Entry::debit(rng.pick(&b.tjs), m(amount, tjs)),
                Entry::credit(rng.pick(&b.usd), m(amount, tjs)),
            ])
        }
    }
}

async fn check_balances(pg: &PostgresLedger, model: &InMemoryLedger, b: &Book, ctx: &str) {
    for (id, ..) in b.all() {
        let expected = model.balance(id).unwrap().minor_units();
        let actual = pg.balance(id).await.unwrap().minor_units();
        assert_eq!(actual, expected, "{ctx}: balance of {id}");
    }
}

async fn run(pg: &PostgresLedger, seed: u64, ops: usize) -> [usize; 2] {
    let mut rng = Rng::new(seed);
    let b = Book {
        settlement_tjs: AccountId::new(),
        settlement_usd: AccountId::new(),
        fee_tjs: AccountId::new(),
        fx_tjs: AccountId::new(),
        fx_usd: AccountId::new(),
        tjs: (0..4).map(|_| AccountId::new()).collect(),
        usd: (0..3).map(|_| AccountId::new()).collect(),
    };
    let mut model = InMemoryLedger::new();
    for (id, ty, currency) in b.all() {
        model.open_account(Account::new(id, ty, currency)).unwrap();
        pg.open_account(&Account::new(id, ty, currency))
            .await
            .unwrap();
    }

    let mut history: Vec<Transaction> = Vec::new();
    let mut outcomes = [0usize; 2];
    for i in 0..ops {
        let ctx = format!("seed {seed} op {i}");
        let txn = next_op(&mut rng, &b, &history);
        let expected = model.post(&txn);
        let actual = pg.post(&txn).await;
        match (expected, actual) {
            (Ok(()), Ok(())) => outcomes[0] += 1,
            (Err(e), Err(StorageError::Ledger(a))) => {
                assert_eq!(a, e, "{ctx}: {txn:?}");
                outcomes[1] += 1;
            }
            (e, a) => panic!("{ctx}: model {e:?} vs postgres {a:?} for {txn:?}"),
        }
        history.push(txn);
        if i % 50 == 49 {
            check_balances(pg, &model, &b, &ctx).await;
        }
    }
    check_balances(pg, &model, &b, &format!("seed {seed} end")).await;

    assert!(model.is_conserved(), "seed {seed}: model conservation");
    let ids: Vec<Uuid> = b.all().iter().map(|(id, ..)| id.as_uuid()).collect();
    let imbalanced: Vec<(String, i64)> = sqlx::query_as(
        "SELECT a.currency, SUM(b.raw_minor)::BIGINT
         FROM balances b JOIN accounts a ON a.id = b.account_id
         WHERE a.id = ANY($1)
         GROUP BY a.currency HAVING SUM(b.raw_minor) <> 0",
    )
    .bind(&ids)
    .fetch_all(pg.pool())
    .await
    .unwrap();
    assert!(imbalanced.is_empty(), "seed {seed}: {imbalanced:?}");
    let drifted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM balances b
         WHERE b.account_id = ANY($1)
           AND b.raw_minor <> COALESCE((
               SELECT SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                           ELSE -e.amount_minor END)
               FROM entries e WHERE e.account_id = b.account_id), 0)",
    )
    .bind(&ids)
    .fetch_one(pg.pool())
    .await
    .unwrap();
    assert_eq!(drifted, 0, "seed {seed}: balances match their entries");
    outcomes
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn postgres_ledger_matches_the_in_memory_model() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    let pg = PostgresLedger::new(pool);
    pg.migrate().await.unwrap();

    let seeds: Vec<u64> = std::env::var("DIFF_SEEDS")
        .map(|s| s.split(',').map(|x| x.trim().parse().unwrap()).collect())
        .unwrap_or_else(|_| vec![1, 2, 3, 4]);
    let ops: usize = std::env::var("DIFF_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    for seed in seeds {
        let [ok, err] = run(&pg, seed, ops).await;
        assert!(
            ok > ops / 4 && err > ops / 10,
            "seed {seed}: the mix should exercise both paths ({ok} ok, {err} rejected)"
        );
    }
}
