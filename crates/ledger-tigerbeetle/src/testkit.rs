//! Suites shared by the workspace tests (against [`SimTb`]) and `tests/live.rs` (against a
//! real cluster). Every suite asserts the backend's invariants itself and panics with the
//! seed or scenario that broke one. The Postgres ones need `DATABASE_URL`; they create their
//! own accounts, so they can share a database and (run serially) a cluster.

use std::future::Future;
use std::time::Duration;

use ledger::{
    Account as LedgerAccount, AccountId, AccountType, Entry, InMemoryLedger, LedgerEngine,
    LedgerError, Transaction,
};
use money::{Currency, Money};
use sqlx::postgres::PgPoolOptions;
use storage::{HookError, LedgerStore, PostHook, PostOptions, PostgresLedger};
use uuid::Uuid;

use crate::client::{account_flags as af, transfer_flags as tf, Account, TbClient, Transfer};
use crate::hybrid::{NoFaults, Probe, Step};
use crate::{HybridLedger, SimTb, TbConfig, TbError, TbLedger};

/// A cluster a suite can run against.
pub trait Cluster: TbClient {
    /// Lets `secs` of cluster time pass (pending timeouts run on it).
    fn pass_time(&self, secs: u64) -> impl Future<Output = ()> + Send;
    /// The pending timeout expiry is tested with: the live cluster really waits it out.
    fn short_timeout(&self) -> u32;
}

impl Cluster for SimTb {
    async fn pass_time(&self, secs: u64) {
        self.advance(secs);
    }

    fn short_timeout(&self) -> u32 {
        10
    }
}

pub fn config(cluster_id: u128) -> TbConfig {
    TbConfig {
        request_timeout: Duration::from_secs(30),
        ..TbConfig::new(cluster_id, "test")
    }
}

pub async fn postgres() -> PostgresLedger {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    postgres_at(&url).await
}

async fn postgres_at(url: &str) -> PostgresLedger {
    let pool = PgPoolOptions::new()
        .max_connections(48)
        .connect(url)
        .await
        .expect("connect to Postgres");
    let pg = PostgresLedger::new(pool);
    pg.migrate().await.expect("migrate");
    pg
}

pub async fn hybrid<C: TbClient>(tb: C, pg: &PostgresLedger, cfg: TbConfig) -> HybridLedger<C> {
    let tb = TbLedger::new(tb, cfg);
    tb.ensure_control_accounts()
        .await
        .expect("control accounts");
    HybridLedger::new(tb, pg.clone())
}

/// `DIFF_SEEDS=1,2,3` and `DIFF_OPS=N` widen the differential search.
pub fn seeds_from_env() -> (Vec<u64>, usize) {
    let seeds = std::env::var("DIFF_SEEDS")
        .map(|s| s.split(',').map(|x| x.trim().parse().unwrap()).collect())
        .unwrap_or_else(|_| vec![1, 2, 3, 4]);
    let ops = std::env::var("DIFF_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    (seeds, ops)
}

/// xorshift64*: deterministic per seed, no extra dependency.
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

    /// 1..=n
    fn one_to(&mut self, n: u64) -> u128 {
        1 + self.below(n) as u128
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

/// The account set of `crates/storage/tests/differential.rs`, so the op mix matches.
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
    fn new() -> Self {
        Self {
            settlement_tjs: AccountId::new(),
            settlement_usd: AccountId::new(),
            fee_tjs: AccountId::new(),
            fx_tjs: AccountId::new(),
            fx_usd: AccountId::new(),
            tjs: (0..4).map(|_| AccountId::new()).collect(),
            usd: (0..3).map(|_| AccountId::new()).collect(),
        }
    }

    fn all(&self) -> Vec<LedgerAccount> {
        let mut v = vec![
            LedgerAccount::new(
                self.settlement_tjs,
                AccountType::SystemSettlement,
                Currency::tjs(),
            ),
            LedgerAccount::new(self.settlement_usd, AccountType::SystemSettlement, usd()),
            LedgerAccount::new(self.fee_tjs, AccountType::SystemFeeRevenue, Currency::tjs()),
            LedgerAccount::new(self.fx_tjs, AccountType::SystemFxGainLoss, Currency::tjs()),
            LedgerAccount::new(self.fx_usd, AccountType::SystemFxGainLoss, usd()),
        ];
        v.extend(
            self.tjs
                .iter()
                .map(|a| LedgerAccount::new(*a, AccountType::UserWallet, Currency::tjs())),
        );
        v.extend(
            self.usd
                .iter()
                .map(|a| LedgerAccount::new(*a, AccountType::UserWallet, usd())),
        );
        v
    }

    fn ids(&self) -> Vec<AccountId> {
        self.all().iter().map(|a| a.id).collect()
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
            let (from, to, from_fx, to_fx, from_cur, to_cur, x) = if rng.below(2) == 0 {
                let x = 105 + rng.amount(40_000);
                (&b.tjs, &b.usd, b.fx_tjs, b.fx_usd, tjs, usd(), x)
            } else {
                let x = 1 + rng.amount(4_000);
                (&b.usd, &b.tjs, b.fx_usd, b.fx_tjs, usd(), tjs, x)
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
            let amount = 10_000_000 + rng.amount(1_000);
            Transaction::with_entries(vec![
                Entry::debit(rng.pick(&b.tjs), m(amount, tjs)),
                Entry::credit(rng.pick(&b.tjs), m(amount, tjs)),
            ])
        }
        80..=89 if !history.is_empty() => history[rng.below(history.len() as u64) as usize].clone(),
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

/// Balances equal the model's, nothing is held, and the book conserves money per currency.
async fn check_balances<C: TbClient>(
    tb: &TbLedger<C>,
    model: &InMemoryLedger,
    book: &Book,
    ctx: &str,
) {
    let balances = tb.balances(&book.ids()).await.unwrap();
    let mut net: std::collections::HashMap<String, i128> = Default::default();
    for (b, a) in balances.iter().zip(book.all()) {
        assert_eq!(
            b.posted,
            model.balance(a.id).unwrap(),
            "{ctx}: balance of {}",
            a.id
        );
        assert_eq!(
            b.available, b.posted,
            "{ctx}: {} still holds a reservation",
            a.id
        );
        *net.entry(a.currency.code().to_string()).or_default() += b.raw;
    }
    assert!(
        net.values().all(|v| *v == 0),
        "{ctx}: money not conserved {net:?}"
    );
}

async fn differential<C: TbClient>(
    tb: &TbLedger<C>,
    book: &Book,
    seed: u64,
    ops: usize,
    mut post: impl AsyncFnMut(&Transaction) -> crate::Result<()>,
) -> [usize; 2] {
    let mut rng = Rng::new(seed);
    let mut model = InMemoryLedger::new();
    for a in book.all() {
        model.open_account(a).unwrap();
    }
    let mut history: Vec<Transaction> = Vec::new();
    let mut outcomes = [0usize; 2];
    for i in 0..ops {
        let ctx = format!("seed {seed} op {i}");
        let txn = next_op(&mut rng, book, &history);
        let expected = model.post(&txn);
        let actual = post(&txn).await;
        match (expected, actual) {
            (Ok(()), Ok(())) => outcomes[0] += 1,
            (Err(e), Err(a)) if a.as_ledger() == Some(&e) => outcomes[1] += 1,
            (e, a) => panic!("{ctx}: model {e:?} vs tigerbeetle {a:?} for {txn:?}"),
        }
        history.push(txn);
        if i % 50 == 49 {
            check_balances(tb, &model, book, &ctx).await;
        }
    }
    check_balances(tb, &model, book, &format!("seed {seed} end")).await;
    assert!(
        outcomes[0] > ops / 4 && outcomes[1] > ops / 10,
        "seed {seed}: the mix should exercise both paths ({outcomes:?})"
    );
    outcomes
}

/// The pure-TigerBeetle path against `InMemoryLedger`: same outcome for every operation
/// (including the exact error), same balances.
pub async fn direct_differential<C: TbClient>(tb: C, seeds: &[u64], ops: usize) {
    let tb = TbLedger::new(tb, config(0));
    tb.ensure_control_accounts().await.unwrap();
    for &seed in seeds {
        let book = Book::new();
        for a in book.all() {
            tb.open_account(&a).await.unwrap();
        }
        differential(&tb, &book, seed, ops, async |txn: &Transaction| {
            tb.post_direct(txn).await
        })
        .await;
    }
}

/// The guarded protocol against `InMemoryLedger`, plus: the journal mirror sums to the
/// TigerBeetle balances and conserves money.
pub async fn hybrid_differential<C: TbClient>(
    tb: C,
    pg: &PostgresLedger,
    seeds: &[u64],
    ops: usize,
) {
    let ledger = hybrid(tb, pg, config(0)).await;
    for &seed in seeds {
        let book = Book::new();
        for a in book.all() {
            ledger.open_account(&a).await.unwrap();
        }
        differential(ledger.tb(), &book, seed, ops, async |txn: &Transaction| {
            ledger.post(txn).await
        })
        .await;
        assert_eq!(
            ledger.reconcile(&book.ids()).await.unwrap(),
            vec![],
            "seed {seed}: journal and tigerbeetle disagree"
        );
        let ids: Vec<Uuid> = book.ids().iter().map(|a| a.as_uuid()).collect();
        let unbalanced: Vec<(String, i64)> = sqlx::query_as(
            "SELECT currency, SUM(CASE direction WHEN 'credit' THEN amount_minor
                                                 ELSE -amount_minor END)::BIGINT
             FROM entries WHERE account_id = ANY($1)
             GROUP BY currency
             HAVING SUM(CASE direction WHEN 'credit' THEN amount_minor ELSE -amount_minor END) <> 0",
        )
        .bind(&ids)
        .fetch_all(ledger.pool())
        .await
        .unwrap();
        assert!(unbalanced.is_empty(), "seed {seed}: journal {unbalanced:?}");
    }
}

/// Dies at one step, or not at all.
struct CrashAt(Option<Step>);

impl Probe for CrashAt {
    async fn at(&self, step: Step) -> bool {
        self.0 == Some(step)
    }
}

/// A settlement account, a funded wallet A, a wallet B and a fee account; the payment under
/// test moves `amount` A → B plus `fee` A → F (two legs in one linked chain).
struct Scene<'a, C> {
    ledger: &'a HybridLedger<C>,
    settlement: AccountId,
    a: AccountId,
    b: AccountId,
    fee: AccountId,
    funded: i128,
}

const AMOUNT: i128 = 1_000;
const FEE: i128 = 10;

impl<'a, C: TbClient> Scene<'a, C> {
    async fn new(ledger: &'a HybridLedger<C>, funded: i128) -> Self {
        let tjs = Currency::tjs();
        let s = Self {
            ledger,
            settlement: AccountId::new(),
            a: AccountId::new(),
            b: AccountId::new(),
            fee: AccountId::new(),
            funded,
        };
        for (id, t) in [
            (s.settlement, AccountType::SystemSettlement),
            (s.a, AccountType::UserWallet),
            (s.b, AccountType::UserWallet),
            (s.fee, AccountType::SystemFeeRevenue),
        ] {
            ledger
                .open_account(&LedgerAccount::new(id, t, tjs))
                .await
                .unwrap();
        }
        ledger
            .post(&Transaction::with_entries(vec![
                Entry::debit(s.settlement, m(funded, tjs)),
                Entry::credit(s.a, m(funded, tjs)),
            ]))
            .await
            .unwrap();
        s
    }

    fn payment(&self) -> Transaction {
        let tjs = Currency::tjs();
        Transaction::with_entries(vec![
            Entry::debit(self.a, m(AMOUNT + FEE, tjs)),
            Entry::credit(self.b, m(AMOUNT, tjs)),
            Entry::credit(self.fee, m(FEE, tjs)),
        ])
    }

    fn ids(&self) -> Vec<AccountId> {
        vec![self.settlement, self.a, self.b, self.fee]
    }

    async fn held(&self) -> bool {
        let b = self.ledger.tb().balances(&[self.a]).await.unwrap();
        b[0].available != b[0].posted
    }

    async fn claimed(&self, txn: &Transaction) -> bool {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM entries WHERE transaction_id = $1)")
            .bind(txn.id.as_uuid())
            .fetch_one(self.ledger.pool())
            .await
            .unwrap()
    }

    /// The payment moved `times` (0 or 1) and nothing else happened: balances, no holds,
    /// journal = TigerBeetle, money conserved.
    async fn assert_moved(&self, times: i128, ctx: &str) {
        let b = self.ledger.tb().balances(&self.ids()).await.unwrap();
        let posted: Vec<i128> = b.iter().map(|x| x.posted.minor_units()).collect();
        assert_eq!(
            posted,
            vec![
                self.funded,
                self.funded - times * (AMOUNT + FEE),
                times * AMOUNT,
                times * FEE
            ],
            "{ctx}: balances (settlement, a, b, fee)"
        );
        assert!(
            b.iter().all(|x| x.available == x.posted),
            "{ctx}: a reservation is still held"
        );
        assert_eq!(
            b.iter().map(|x| x.raw).sum::<i128>(),
            0,
            "{ctx}: conservation"
        );
        assert_eq!(
            self.ledger.reconcile(&self.ids()).await.unwrap(),
            vec![],
            "{ctx}: journal and tigerbeetle disagree"
        );
    }
}

/// A recovery pass that does not leave young reservations to their requests (for tests with
/// nothing else in flight).
pub async fn recover_all<C: TbClient>(ledger: &HybridLedger<C>) -> crate::RecoveryReport {
    ledger
        .recover_probed(Duration::ZERO, &NoFaults)
        .await
        .expect("recovery pass")
}

/// A post refused only because other requests hold the funds waits for them, as a Postgres
/// post waits for the wallet's row lock: it goes through once they void, fails as retryable
/// if they outlast the wait, and a balance that is short even without them fails at once.
pub async fn holds<C: TbClient>(tb: C, pg: &PostgresLedger) {
    let wait = Duration::from_millis(500);
    let cfg = TbConfig {
        hold_wait: wait,
        ..config(0)
    };
    let ledger = hybrid(tb, pg, cfg).await;
    let scene = Scene::new(&ledger, AMOUNT + FEE).await;
    // A request died holding the whole balance; recovery has not run.
    post_and_crash(&ledger, &scene.payment(), CrashPoint::Reserved).await;

    let t = std::time::Instant::now();
    let r = ledger.post(&scene.payment()).await;
    assert!(matches!(r, Err(TbError::Retry(_))), "outlasted: {r:?}");
    assert!(t.elapsed() >= wait);

    let waiting = {
        let (ledger, payment) = (ledger.clone(), scene.payment());
        tokio::spawn(async move { ledger.post(&payment).await })
    };
    tokio::time::sleep(wait / 5).await;
    assert_eq!(recover_all(&ledger).await.voided, 1);
    waiting
        .await
        .unwrap()
        .expect("the hold voided while the post waited");
    scene.assert_moved(1, "after the hold voided").await;

    let t = std::time::Instant::now();
    let r = ledger.post(&scene.payment()).await;
    assert!(
        matches!(
            r.as_ref().map_err(TbError::as_ledger),
            Err(Some(LedgerError::InsufficientFunds { .. }))
        ),
        "short without holds: {r:?}"
    );
    assert!(t.elapsed() < wait, "no wait for a genuinely short balance");
}

/// What reconciliation leans on to tell a post in flight from drift: the cluster's newest
/// timestamp bounds every transfer applied before the call (an unfiltered query), and each
/// transaction's post is found under its commit-record id, absent until it is applied.
pub async fn timestamps<C: TbClient>(tb: C, pg: &PostgresLedger) {
    let ledger = hybrid(tb, pg, config(0)).await;
    let scene = Scene::new(&ledger, 10_000).await;
    let first = scene.payment();
    ledger.post(&first).await.unwrap();
    let n0 = ledger.tb().newest_timestamp().await.unwrap();
    let at = ledger.tb().posted_at(&[first.id.as_uuid()]).await.unwrap();
    let posted = at[&first.id.as_uuid()];
    assert!(0 < posted && posted <= n0, "{posted} <= {n0}");

    let stranded = scene.payment();
    post_and_crash(&ledger, &stranded, CrashPoint::Committed).await;
    let none = ledger.tb().posted_at(&[stranded.id.as_uuid()]).await;
    assert!(none.unwrap().is_empty(), "committed, not posted");
    assert!(recover_all(&ledger).await.posted >= 1);
    let later = ledger
        .tb()
        .posted_at(&[stranded.id.as_uuid()])
        .await
        .unwrap();
    assert!(later[&stranded.id.as_uuid()] > n0);
    assert!(ledger.tb().newest_timestamp().await.unwrap() >= later[&stranded.id.as_uuid()]);
    assert!(!scene.held().await, "no reservation left");
    assert_eq!(ledger.reconcile(&scene.ids()).await.unwrap(), vec![]);
}

/// Where [`post_and_crash`] kills a post.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    /// The reservation is in the cluster; Postgres never saw the transaction.
    Reserved,
    /// Postgres committed the transaction (journal, outbox, idempotency record); its post
    /// was never sent.
    Committed,
}

/// Posts `txn` and "dies" at `at`, leaving the cluster and Postgres exactly as a crash there
/// would: for other crates' tests of recovery and reconciliation.
pub async fn post_and_crash<C: TbClient>(
    ledger: &HybridLedger<C>,
    txn: &Transaction,
    at: CrashPoint,
) {
    let step = match at {
        CrashPoint::Reserved => Step::Reserved,
        CrashPoint::Committed => Step::Committed,
    };
    let mut conn = ledger.pool().acquire().await.expect("connection");
    let r = ledger
        .post_probed(&mut conn, txn, PostOptions::default(), &CrashAt(Some(step)))
        .await;
    assert!(injected(&r), "a crash at {at:?} was injected: {r:?}");
}

fn injected(r: &crate::Result<()>) -> bool {
    matches!(r, Err(TbError::Unavailable(m)) if m.starts_with("injected"))
}

/// Kills the protocol at every step and proves recovery leaves exactly one of two states —
/// the payment posted once, or not at all — with no hold left, TigerBeetle equal to the
/// journal, and a retry of the same transaction completing it exactly once.
pub async fn crash_recovery<C: TbClient>(tb: C, pg: &PostgresLedger) {
    let ledger = hybrid(tb, pg, config(0)).await;
    for (step, commits, held) in [
        (Step::Reserved, false, true),
        (Step::Mirrored, false, true),
        (Step::CommitLost, false, false),
        (Step::CommitUnacked, true, false),
        (Step::Committed, true, true),
    ] {
        let ctx = format!("crash at {step:?}");
        let s = Scene::new(&ledger, 50_000).await;
        let txn = s.payment();
        let mut conn = ledger.pool().acquire().await.unwrap();
        let r = ledger
            .post_probed(
                &mut conn,
                &txn,
                PostOptions::default(),
                &CrashAt(Some(step)),
            )
            .await;
        drop(conn);
        match step {
            // An unknown COMMIT is settled inline through the tombstone.
            Step::CommitUnacked => assert!(r.is_ok(), "{ctx}: {r:?}"),
            Step::CommitLost => assert!(matches!(r, Err(TbError::Unavailable(_))), "{ctx}: {r:?}"),
            _ => assert!(injected(&r), "{ctx}: {r:?}"),
        }
        assert_eq!(s.held().await, held, "{ctx}: hold before recovery");
        assert_eq!(
            s.claimed(&txn).await,
            commits,
            "{ctx}: journal before recovery"
        );

        let report = recover_all(&ledger).await;
        if held {
            assert!(
                report.posted + report.voided >= 1,
                "{ctx}: recovery settled nothing: {report:?}"
            );
        }
        s.assert_moved(commits as i128, &format!("{ctx}, after recovery"))
            .await;

        let retry = ledger.post(&txn).await;
        match commits {
            true => assert_eq!(
                retry.unwrap_err().as_ledger(),
                Some(&LedgerError::DuplicateTransaction(txn.id)),
                "{ctx}: retry"
            ),
            false => retry.unwrap(),
        }
        s.assert_moved(1, &format!("{ctx}, after the retry")).await;
        let again = recover_all(&ledger).await;
        assert_eq!(
            again.posted + again.voided + again.forced,
            0,
            "{ctx}: {again:?}"
        );
    }

    // Recovery itself dies between deciding and acting, for an uncommitted and a committed
    // attempt; the next pass finishes the job from the recorded outcome.
    for (step, commits) in [(Step::Reserved, false), (Step::Committed, true)] {
        let ctx = format!("recovery crash after a request crash at {step:?}");
        let s = Scene::new(&ledger, 50_000).await;
        let txn = s.payment();
        let mut conn = ledger.pool().acquire().await.unwrap();
        let r = ledger
            .post_probed(
                &mut conn,
                &txn,
                PostOptions::default(),
                &CrashAt(Some(step)),
            )
            .await;
        drop(conn);
        assert!(injected(&r), "{ctx}: {r:?}");
        let dead = ledger
            .recover_probed(Duration::ZERO, &CrashAt(Some(Step::Tombstoned)))
            .await;
        assert!(
            matches!(dead, Err(TbError::Unavailable(_))),
            "{ctx}: {dead:?}"
        );
        assert!(s.held().await, "{ctx}: the dead pass must not have acted");
        recover_all(&ledger).await;
        s.assert_moved(commits as i128, &ctx).await;
    }

    // A guard rejection voids at once and changes nothing; a later attempt succeeds.
    let s = Scene::new(&ledger, 50_000).await;
    let txn = s.payment();
    let mut conn = ledger.pool().acquire().await.unwrap();
    let reject: PostHook = Box::new(|_: &mut sqlx::PgConnection| {
        Box::pin(async {
            Err(HookError::Rejected {
                rule: "test".into(),
                message: "rejected by the test guard".into(),
            })
        })
    });
    let r = ledger
        .post_on(
            &mut conn,
            &txn,
            PostOptions {
                guard: Some(reject),
                ..PostOptions::default()
            },
        )
        .await;
    assert!(
        matches!(
            r,
            Err(TbError::Storage(storage::StorageError::Rejected { .. }))
        ),
        "{r:?}"
    );
    assert!(!s.held().await, "a rejected attempt must not keep its hold");
    s.assert_moved(0, "guard rejection").await;
    ledger
        .post_on(&mut conn, &txn, PostOptions::default())
        .await
        .unwrap();
    s.assert_moved(1, "after a guard rejection").await;
}

/// Runs recovery at a chosen step of a request.
struct RecoverAt<C> {
    ledger: HybridLedger<C>,
    step: Step,
    concurrent: bool,
}

impl<C: TbClient> Probe for RecoverAt<C> {
    async fn at(&self, step: Step) -> bool {
        if step == self.step {
            if self.concurrent {
                let ledger = self.ledger.clone();
                let pass = tokio::spawn(async move { recover_all(&ledger).await });
                // Long enough for the pass to queue on the intent's key; if it is slower it
                // sees the committed outcome instead, which settles the same way.
                tokio::time::sleep(Duration::from_millis(300)).await;
                std::mem::drop(pass);
            } else {
                recover_all(&self.ledger).await;
            }
        }
        false
    }
}

/// The tombstone serialises a request with a recovery pass that treats its reservation as
/// abandoned: either recovery's `void` lands first (the request fails with `Retry`, nothing
/// moves) or the request's `commit` does (recovery waits, then posts). Never both.
pub async fn tombstone_race<C: TbClient>(tb: C, pg: &PostgresLedger) {
    let ledger = hybrid(tb, pg, config(0)).await;

    let s = Scene::new(&ledger, 50_000).await;
    let txn = s.payment();
    let mut conn = ledger.pool().acquire().await.unwrap();
    let probe = RecoverAt {
        ledger: ledger.clone(),
        step: Step::Reserved,
        concurrent: false,
    };
    let r = ledger
        .post_probed(&mut conn, &txn, PostOptions::default(), &probe)
        .await;
    assert!(matches!(r, Err(TbError::Retry(_))), "recovery first: {r:?}");
    s.assert_moved(0, "recovery won").await;
    ledger
        .post_on(&mut conn, &txn, PostOptions::default())
        .await
        .unwrap();
    s.assert_moved(1, "retry after recovery won").await;

    let s = Scene::new(&ledger, 50_000).await;
    let txn = s.payment();
    let probe = RecoverAt {
        ledger: ledger.clone(),
        step: Step::Mirrored,
        concurrent: true,
    };
    let r = ledger
        .post_probed(&mut conn, &txn, PostOptions::default(), &probe)
        .await;
    assert!(r.is_ok(), "commit first: {r:?}");
    // The spawned pass may still be finishing its post; a full pass afterwards settles nothing.
    tokio::time::sleep(Duration::from_millis(200)).await;
    recover_all(&ledger).await;
    s.assert_moved(1, "commit won").await;
}

/// The timeout backstop: an abandoned reservation expires by itself, a committed one that
/// expired before recovery ran is re-posted, and a commit too slow for its reservation is
/// refused before it can outlive it.
pub async fn expiry<C: Cluster>(tb: C, pg: &PostgresLedger) {
    let timeout = tb.short_timeout();
    let ledger = hybrid(
        tb,
        pg,
        TbConfig {
            pending_timeout_secs: timeout,
            ..config(0)
        },
    )
    .await;
    let wait = timeout as u64 + 2;

    let s = Scene::new(&ledger, 50_000).await;
    let txn = s.payment();
    let mut conn = ledger.pool().acquire().await.unwrap();
    let probe = CrashAt(Some(Step::Reserved));
    assert!(injected(
        &ledger
            .post_probed(&mut conn, &txn, PostOptions::default(), &probe)
            .await
    ));
    ledger.tb().client().pass_time(wait).await;
    assert!(!s.held().await, "an expired reservation releases its hold");
    recover_all(&ledger).await;
    s.assert_moved(0, "abandoned reservation expired").await;

    let s = Scene::new(&ledger, 50_000).await;
    let txn = s.payment();
    let probe = CrashAt(Some(Step::Committed));
    assert!(injected(
        &ledger
            .post_probed(&mut conn, &txn, PostOptions::default(), &probe)
            .await
    ));
    ledger.tb().client().pass_time(wait).await;
    let report = recover_all(&ledger).await;
    assert!(report.forced >= 1, "{report:?}");
    s.assert_moved(1, "committed reservation expired, re-posted")
        .await;
    let retry = ledger.post(&txn).await.unwrap_err();
    assert_eq!(
        retry.as_ledger(),
        Some(&LedgerError::DuplicateTransaction(txn.id))
    );

    // Budget = timeout / 2, on the request's own clock.
    struct Slow(Duration);
    impl Probe for Slow {
        async fn at(&self, step: Step) -> bool {
            if step == Step::Mirrored {
                tokio::time::sleep(self.0).await;
            }
            false
        }
    }
    let slow = HybridLedger::new(
        ledger.tb().with_config(TbConfig {
            pending_timeout_secs: 2,
            ..config(0)
        }),
        pg.clone(),
    );
    let s = Scene::new(&slow, 50_000).await;
    let txn = s.payment();
    let r = slow
        .post_probed(
            &mut conn,
            &txn,
            PostOptions::default(),
            &Slow(Duration::from_millis(1_100)),
        )
        .await;
    assert!(matches!(r, Err(TbError::Retry(_))), "slow commit: {r:?}");
    s.assert_moved(0, "slow commit refused").await;
}

/// A per-user daily limit for one payment.
#[derive(Clone, Copy)]
struct Limit {
    user: Uuid,
    amount: i64,
    limit: i64,
}

/// The guard of `payments::aml_guard`, reduced to its concurrency-relevant shape: lock the
/// user's row, then sum the user's debits in the window in a new statement.
fn daily_limit(
    Limit {
        user,
        amount,
        limit,
    }: Limit,
) -> PostHook {
    Box::new(move |conn: &mut sqlx::PgConnection| {
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
            if spent + amount > limit {
                return Err(HookError::Rejected {
                    rule: "daily_limit".into(),
                    message: "amount exceeds the rolling 24h limit".into(),
                });
            }
            Ok(())
        })
    })
}

async fn spawn_all<C: TbClient, T: Send + 'static>(
    ledger: &HybridLedger<C>,
    jobs: Vec<(Transaction, Option<Limit>)>,
    then: impl Fn(crate::Result<()>) -> T + Send + Sync + Copy + 'static,
) -> Vec<T> {
    let start = std::sync::Arc::new(tokio::sync::Barrier::new(jobs.len()));
    let handles: Vec<_> = jobs
        .into_iter()
        .map(|(txn, guard)| {
            let (ledger, start) = (ledger.clone(), start.clone());
            tokio::spawn(async move {
                let mut conn = ledger.pool().acquire().await.unwrap();
                start.wait().await;
                let opts = PostOptions {
                    guard: guard.map(daily_limit),
                    ..PostOptions::default()
                };
                then(ledger.post_on(&mut conn, &txn, opts).await)
            })
        })
        .collect();
    let mut out = Vec::new();
    for h in handles {
        out.push(h.await.unwrap());
    }
    out
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
enum Outcome {
    Posted,
    Duplicate,
    Insufficient,
    Rejected,
}

fn outcome(r: crate::Result<()>) -> Outcome {
    match r {
        Ok(()) => Outcome::Posted,
        Err(e) => match e.as_ledger() {
            Some(LedgerError::DuplicateTransaction(_)) => Outcome::Duplicate,
            Some(LedgerError::InsufficientFunds { .. }) => Outcome::Insufficient,
            _ if matches!(e, TbError::Storage(storage::StorageError::Rejected { .. })) => {
                Outcome::Rejected
            }
            _ => panic!("unexpected {e:?}"),
        },
    }
}

fn count(outcomes: &[Outcome], o: Outcome) -> usize {
    outcomes.iter().filter(|x| **x == o).count()
}

/// Races: a double spend (only TigerBeetle's balance limit stands between the requests), one
/// transaction id submitted many times at once, and the per-user AML window, which stays exact
/// because the guard still runs under the Postgres user-row lock.
pub async fn concurrency<C: TbClient>(tb: C, pg: &PostgresLedger) {
    let ledger = hybrid(tb, pg, config(0)).await;
    let tjs = Currency::tjs();

    let s = Scene::new(&ledger, AMOUNT + FEE).await;
    let jobs = (0..16).map(|_| (s.payment(), None)).collect();
    let outcomes = spawn_all(&ledger, jobs, outcome).await;
    assert_eq!(
        count(&outcomes, Outcome::Posted),
        1,
        "double spend: {outcomes:?}"
    );
    assert_eq!(count(&outcomes, Outcome::Insufficient), 15, "{outcomes:?}");
    s.assert_moved(1, "double spend").await;

    let s = Scene::new(&ledger, 50_000).await;
    let txn = s.payment();
    let jobs = (0..16).map(|_| (txn.clone(), None)).collect();
    let outcomes = spawn_all(&ledger, jobs, outcome).await;
    assert_eq!(
        count(&outcomes, Outcome::Posted),
        1,
        "same id: {outcomes:?}"
    );
    assert_eq!(count(&outcomes, Outcome::Duplicate), 15, "{outcomes:?}");
    s.assert_moved(1, "same id").await;

    // One user, two wallets, a 10 000 limit and 40 payments of 700 from both wallets at once:
    // exactly 14 fit.
    let user = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, phone, password_hash) VALUES ($1, $2, 'x')")
        .bind(user)
        .bind(format!("tb-test-{user}"))
        .execute(ledger.pool())
        .await
        .unwrap();
    let (settlement, recipient) = (AccountId::new(), AccountId::new());
    let wallets = [AccountId::new(), AccountId::new()];
    ledger
        .open_account(&LedgerAccount::new(
            settlement,
            AccountType::SystemSettlement,
            tjs,
        ))
        .await
        .unwrap();
    ledger
        .open_account(&LedgerAccount::new(recipient, AccountType::UserWallet, tjs))
        .await
        .unwrap();
    for w in wallets {
        let mut conn = ledger.pool().acquire().await.unwrap();
        ledger
            .open_account_owned_on(
                &mut conn,
                &LedgerAccount::new(w, AccountType::UserWallet, tjs),
                Some(user),
            )
            .await
            .unwrap();
        ledger
            .post(&Transaction::with_entries(vec![
                Entry::debit(settlement, m(100_000, tjs)),
                Entry::credit(w, m(100_000, tjs)),
            ]))
            .await
            .unwrap();
    }
    let jobs = (0..40)
        .map(|i| {
            (
                Transaction::with_entries(vec![
                    Entry::debit(wallets[i % 2], m(700, tjs)),
                    Entry::credit(recipient, m(700, tjs)),
                ]),
                Some(Limit {
                    user,
                    amount: 700,
                    limit: 10_000,
                }),
            )
        })
        .collect();
    let outcomes = spawn_all(&ledger, jobs, outcome).await;
    assert_eq!(count(&outcomes, Outcome::Posted), 14, "aml: {outcomes:?}");
    assert_eq!(count(&outcomes, Outcome::Rejected), 26, "aml: {outcomes:?}");
    let all = [settlement, recipient, wallets[0], wallets[1]];
    let b = ledger.tb().balances(&all).await.unwrap();
    assert_eq!(b[1].posted.minor_units(), 14 * 700, "recipient");
    assert_eq!(
        b[2].posted.minor_units() + b[3].posted.minor_units(),
        200_000 - 14 * 700
    );
    assert!(b.iter().all(|x| x.available == x.posted), "no hold left");
    assert_eq!(ledger.reconcile(&all).await.unwrap(), vec![]);
}

/// The cut-over: balances built by `PostgresLedger` move into TigerBeetle (idempotently),
/// then guarded posting continues and TigerBeetle still equals the journal. Uses a fresh
/// database, because import reads every account there is.
pub async fn import_cutover<C: TbClient>(tb: C) {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let name = format!("payment_tb_import_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let (base, _) = url.rsplit_once('/').expect("DATABASE_URL ends in /dbname");
    let pg = postgres_at(&format!("{base}/{name}")).await;

    let tjs = Currency::tjs();
    let (settlement, fee, fx_tjs, fx_usd, settlement_usd) = (
        AccountId::new(),
        AccountId::new(),
        AccountId::new(),
        AccountId::new(),
        AccountId::new(),
    );
    let wallets: Vec<AccountId> = (0..3).map(|_| AccountId::new()).collect();
    let usd_wallet = AccountId::new();
    let mut accounts = vec![
        LedgerAccount::new(settlement, AccountType::SystemSettlement, tjs),
        LedgerAccount::new(fee, AccountType::SystemFeeRevenue, tjs),
        LedgerAccount::new(fx_tjs, AccountType::SystemFxGainLoss, tjs),
        LedgerAccount::new(fx_usd, AccountType::SystemFxGainLoss, usd()),
        LedgerAccount::new(settlement_usd, AccountType::SystemSettlement, usd()),
        LedgerAccount::new(usd_wallet, AccountType::UserWallet, usd()),
    ];
    accounts.extend(
        wallets
            .iter()
            .map(|w| LedgerAccount::new(*w, AccountType::UserWallet, tjs)),
    );
    for a in &accounts {
        pg.open_account(a).await.unwrap();
    }
    let txns = [
        vec![
            Entry::debit(settlement, m(90_000, tjs)),
            Entry::credit(wallets[0], m(90_000, tjs)),
        ],
        vec![
            Entry::debit(wallets[0], m(10_100, tjs)),
            Entry::credit(wallets[1], m(10_000, tjs)),
            Entry::credit(fee, m(100, tjs)),
        ],
        vec![
            Entry::debit(wallets[1], m(1_050, tjs)),
            Entry::credit(fx_tjs, m(1_050, tjs)),
            Entry::debit(fx_usd, m(100, usd())),
            Entry::credit(usd_wallet, m(100, usd())),
        ],
    ];
    for entries in txns {
        pg.post(&Transaction::with_entries(entries)).await.unwrap();
    }

    let ledger = hybrid(tb, &pg, config(0)).await;
    let refused = |r: crate::Result<()>, why: &str| match r {
        Err(TbError::Storage(storage::StorageError::DataIntegrity(m))) => {
            assert!(m.contains(why), "{m}")
        }
        other => panic!("expected a refusal ({why}), got {other:?}"),
    };
    refused(ledger.prepare().await, "never imported");
    let first = ledger.import_from_postgres().await.unwrap();
    ledger
        .prepare()
        .await
        .expect("imported: the cluster may serve");
    let ids: Vec<AccountId> = accounts.iter().map(|a| a.id).collect();
    let before: Vec<i128> = ledger
        .tb()
        .balances(&ids)
        .await
        .unwrap()
        .iter()
        .map(|b| b.posted.minor_units())
        .collect();
    for (id, tb_balance) in ids.iter().zip(&before) {
        assert_eq!(
            *tb_balance,
            pg.balance(*id).await.unwrap().minor_units(),
            "{id}"
        );
    }
    let again = ledger.import_from_postgres().await.unwrap();
    assert_eq!(first, again, "a re-run is a no-op");
    assert_eq!(first.opening_balances, 7, "{first:?}");
    let opening = ledger
        .tb()
        .client()
        .lookup_accounts(vec![crate::ids::opening_account(crate::ids::ledger_of(
            tjs,
        ))])
        .await
        .unwrap();
    assert_eq!(
        opening[0].debits_posted, opening[0].credits_posted,
        "opening nets to zero"
    );

    ledger
        .post(&Transaction::with_entries(vec![
            Entry::debit(wallets[1], m(500, tjs)),
            Entry::credit(wallets[2], m(500, tjs)),
        ]))
        .await
        .unwrap();
    assert_eq!(ledger.reconcile(&ids).await.unwrap(), vec![]);
    assert_eq!(
        ledger.tb().balance(wallets[2]).await.unwrap().minor_units(),
        500
    );

    // The way back: a committed transaction whose post never happened is settled first,
    // then `balances` is rebuilt from the journal and equals the cluster account for account.
    let stranded = Transaction::with_entries(vec![
        Entry::debit(wallets[0], m(7, tjs)),
        Entry::credit(wallets[2], m(7, tjs)),
    ]);
    post_and_crash(&ledger, &stranded, CrashPoint::Committed).await;
    let back = ledger.rollback_to_postgres().await.unwrap();
    assert_eq!(back.settled.posted, 1, "{back:?}");
    assert_eq!(
        back.balances_rewritten, 3,
        "wallets 0, 1 and 2 moved since the cut-over"
    );
    let after = ledger.tb().balances(&ids).await.unwrap();
    for b in &after {
        assert_eq!(
            b.posted,
            pg.balance(b.account).await.unwrap(),
            "{}",
            b.account
        );
    }
    pg.post(&Transaction::with_entries(vec![
        Entry::debit(wallets[2], m(507, tjs)),
        Entry::credit(wallets[0], m(507, tjs)),
    ]))
    .await
    .expect("Postgres posts on the rebuilt balances");
    refused(ledger.prepare().await, "rolled back");
    refused(
        ledger.import_from_postgres().await.map(|_| ()),
        "rolled back",
    );

    pg.pool().close().await;
    sqlx::query(&format!("DROP DATABASE {name} WITH (FORCE)"))
        .execute(&admin)
        .await
        .unwrap();
}

/// Replays the same TigerBeetle operations against two clusters (the model and a real one)
/// and requires identical results and resulting state: the scripted part walks every code the
/// backend depends on, the random part mixes flags, limits, chains and two-phase transfers.
pub async fn fidelity<A: Cluster, B: Cluster>(a: &A, b: &B, seed: u64) {
    let base = Uuid::now_v7().as_u128() & !0xFFFF_FFFF;
    let id = |n: u128| base + n;
    let (ledger, other) = (0x5453_4A02, 0x5553_4402); // any two non-zero ledgers
    let acct = |n: u128, ledger: u32, code: u16, flags: u16| Account {
        id: id(n),
        ledger,
        code,
        flags,
        ..Account::default()
    };
    let (dmnec, cmned) = (
        af::DEBITS_MUST_NOT_EXCEED_CREDITS,
        af::CREDITS_MUST_NOT_EXCEED_DEBITS,
    );
    let accounts = vec![
        acct(1, ledger, 1, dmnec),
        acct(2, ledger, 1, dmnec),
        acct(3, ledger, 2, 0),
        acct(4, ledger, 2, cmned),
        acct(5, ledger, 3, 0),
        acct(6, other, 1, 0),
    ];
    let mut bad = accounts.clone();
    bad[0].flags = 0; // exists_with_different_flags
    bad.push(acct(7, 0, 1, 0)); // ledger_must_not_be_zero
    bad.push(acct(8, ledger, 1, dmnec | cmned)); // flags_are_mutually_exclusive
    for batch in [accounts, bad] {
        let (ra, rb) = (
            a.create_accounts(batch.clone()).await.unwrap(),
            b.create_accounts(batch).await.unwrap(),
        );
        assert_eq!(ra, rb, "create_accounts");
    }

    let t = |n: u128, dr: u128, cr: u128, amount: u128| Transfer {
        id: id(n),
        debit_account_id: id(dr),
        credit_account_id: id(cr),
        amount,
        ledger,
        code: 1,
        ..Transfer::default()
    };
    let with = |mut x: Transfer, flags: u16, timeout: u32| {
        x.flags |= flags;
        x.timeout = timeout;
        x
    };
    let pending = |x| with(x, tf::PENDING, 0);
    let resolve = |n: u128, p: u128, flags: u16, amount: u128| Transfer {
        id: id(n),
        pending_id: id(p),
        amount,
        flags,
        ..Transfer::default()
    };
    let post = |n, p| resolve(n, p, tf::POST_PENDING_TRANSFER, crate::AMOUNT_MAX);
    let void = |n, p| resolve(n, p, tf::VOID_PENDING_TRANSFER, 0);
    let script: Vec<Vec<Transfer>> = vec![
        vec![t(100, 3, 1, 10_000), t(101, 3, 2, 500)],
        vec![t(102, 1, 2, 20_000)], // exceeds_credits
        vec![t(102, 1, 2, 100)],    // id_already_failed
        vec![t(103, 1, 2, 100), t(103, 1, 2, 100), t(103, 1, 2, 101)], // exists, ..._amount
        vec![
            with(t(105, 1, 2, 1), tf::LINKED, 0),
            with(t(106, 2, 9, 1), tf::LINKED, 0), // credit_account_not_found mid-chain
            t(107, 1, 2, 1),
        ],
        vec![t(105, 1, 2, 1)], // linked_event_failed did not burn the id
        vec![with(t(108, 1, 2, 1), tf::LINKED, 0)], // linked_event_chain_open
        vec![
            t(109, 1, 1, 1),
            t(110, 1, 6, 1),
            t(111, 0, 1, 1),
            t(112, 1, 2, 0),
        ],
        vec![pending(t(120, 1, 2, 3_000)), pending(t(121, 1, 2, 3_000))],
        vec![pending(t(122, 1, 2, 9_000))], // pending debits count against the limit
        vec![post(123, 120)],
        vec![void(124, 121)],
        vec![void(125, 120)], // already_posted
        vec![post(126, 121)], // already_voided
        vec![post(123, 120)], // exists
        vec![post(127, 999)], // pending_transfer_not_found
        vec![
            pending(t(128, 1, 2, 10)),
            resolve(129, 128, tf::POST_PENDING_TRANSFER, 11), // exceeds_pending_transfer_amount
        ],
        vec![resolve(130, 128, tf::POST_PENDING_TRANSFER, 4)], // partial post
        vec![t(131, 4, 3, 10), t(132, 3, 4, 20)],              // exceeds_debits
        vec![with(t(140, 1, 2, 5), tf::PENDING, 1)],
    ];
    let mut seen = std::collections::BTreeSet::new();
    for (i, batch) in script.into_iter().enumerate() {
        let (ra, rb) = (
            a.create_transfers(batch.clone()).await.unwrap(),
            b.create_transfers(batch).await.unwrap(),
        );
        assert_eq!(ra, rb, "scripted batch {i}");
        seen.extend(ra.iter().map(|r| r.0));
    }
    a.pass_time(3).await;
    b.pass_time(3).await;
    let after_expiry = vec![post(141, 140)];
    let (ra, rb) = (
        a.create_transfers(after_expiry.clone()).await.unwrap(),
        b.create_transfers(after_expiry).await.unwrap(),
    );
    assert_eq!(ra, rb, "post after expiry");
    seen.extend(ra.iter().map(|r| r.0));

    let mut rng = Rng::new(seed);
    let mut pendings: Vec<u128> = vec![120, 121, 128];
    let mut used: Vec<u128> = vec![100, 101, 103];
    let mut next = 1_000u128;
    for round in 0..300 {
        let n = 1 + rng.below(4) as usize;
        let mut batch = Vec::with_capacity(n);
        for k in 0..n {
            next += 1;
            let (dr, cr, amount) = (rng.one_to(5), rng.one_to(5), rng.one_to(3_000) - 1);
            let mut x = match rng.below(10) {
                0..=3 => t(next, dr, cr, amount),
                4..=5 => {
                    pendings.push(next);
                    pending(t(next, dr, cr, amount))
                }
                6 if rng.below(2) == 0 => post(next, rng.pick(&pendings)),
                6 => resolve(
                    next,
                    rng.pick(&pendings),
                    tf::POST_PENDING_TRANSFER,
                    rng.below(2_000) as u128,
                ),
                7 => void(next, rng.pick(&pendings)),
                8 => t(rng.pick(&used), 3, 1, [10_000, 1][rng.below(2) as usize]),
                _ => t(next, rng.one_to(6), rng.one_to(6), rng.below(100) as u128),
            };
            if k + 1 < n && rng.below(2) == 0 {
                x.flags |= tf::LINKED;
            }
            used.push(next);
            batch.push(x);
        }
        let (ra, rb) = (
            a.create_transfers(batch.clone()).await.unwrap(),
            b.create_transfers(batch.clone()).await.unwrap(),
        );
        assert_eq!(ra, rb, "random round {round}: {batch:?}");
        seen.extend(ra.iter().map(|r| r.0));
    }
    type R = crate::TransferResult;
    for r in [
        R::OK,
        R::LINKED_EVENT_FAILED,
        R::LINKED_EVENT_CHAIN_OPEN,
        R::ACCOUNTS_MUST_BE_DIFFERENT,
        R::DEBIT_ACCOUNT_NOT_FOUND,
        R::CREDIT_ACCOUNT_NOT_FOUND,
        R::ACCOUNTS_MUST_HAVE_THE_SAME_LEDGER,
        R::PENDING_TRANSFER_NOT_FOUND,
        R::EXCEEDS_PENDING_TRANSFER_AMOUNT,
        R::PENDING_TRANSFER_ALREADY_POSTED,
        R::PENDING_TRANSFER_ALREADY_VOIDED,
        R::PENDING_TRANSFER_EXPIRED,
        R::EXISTS,
        R::EXISTS_WITH_DIFFERENT_AMOUNT,
        R::EXCEEDS_CREDITS,
        R::EXCEEDS_DEBITS,
        R::ID_ALREADY_FAILED,
    ] {
        assert!(
            seen.contains(&r.0),
            "seed {seed}: the replay never produced {r:?}"
        );
    }

    let ids: Vec<u128> = (1..=8).map(id).collect();
    let strip = |mut v: Vec<Account>| {
        v.iter_mut().for_each(|x| x.timestamp = 0);
        v
    };
    assert_eq!(
        strip(a.lookup_accounts(ids.clone()).await.unwrap()),
        strip(b.lookup_accounts(ids).await.unwrap()),
        "final balances"
    );
    let transfer_ids: Vec<u128> = (0..next + 1).map(id).collect();
    let strip = |mut v: Vec<Transfer>| {
        v.iter_mut().for_each(|x| x.timestamp = 0);
        v
    };
    for chunk in transfer_ids.chunks(crate::client::LOOKUP_BATCH) {
        assert_eq!(
            strip(a.lookup_transfers(chunk.to_vec()).await.unwrap()),
            strip(b.lookup_transfers(chunk.to_vec()).await.unwrap()),
            "stored transfers"
        );
    }
}
