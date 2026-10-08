//! The shared suites against `SimTb`. Those needing Postgres are ignored by default, like the
//! storage ones: `DATABASE_URL=… cargo test -p ledger-tigerbeetle -- --ignored`.
//! `tests/live.rs` (feature `native-client`) runs the same suites against a real cluster.

use std::time::Duration;

use ledger::{Account as LedgerAccount, AccountId, AccountType, Entry, LedgerError, Transaction};
use money::{Currency, Money};

use crate::client::{account_flags as af, transfer_flags as tf, Account, TbClient, Transfer};
use crate::testkit::*;
use crate::{Fault, Op, SimTb, TbError, TransferResult as R};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_path_matches_the_in_memory_model() {
    let (seeds, ops) = seeds_from_env();
    direct_differential(SimTb::new(), &seeds, ops).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL (DATABASE_URL)"]
async fn hybrid_matches_the_in_memory_model() {
    let (seeds, ops) = seeds_from_env();
    hybrid_differential(SimTb::new(), &postgres().await, &seeds, ops).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL (DATABASE_URL)"]
async fn a_crash_at_any_step_settles_to_posted_once_or_not_at_all() {
    crash_recovery(SimTb::new(), &postgres().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL (DATABASE_URL)"]
async fn the_tombstone_serialises_a_request_with_recovery() {
    tombstone_race(SimTb::new(), &postgres().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL (DATABASE_URL)"]
async fn pending_timeouts_are_a_backstop_only() {
    expiry(SimTb::new(), &postgres().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL (DATABASE_URL)"]
async fn double_spend_same_id_and_aml_races() {
    concurrency(SimTb::new(), &postgres().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL (DATABASE_URL)"]
async fn postgres_balances_cut_over_into_tigerbeetle() {
    import_cutover(SimTb::new()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL (DATABASE_URL)"]
async fn timestamps_bound_what_a_read_saw() {
    timestamps(SimTb::new(), &postgres().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL (DATABASE_URL)"]
async fn a_post_waits_for_holds_in_flight_like_for_a_row_lock() {
    holds(SimTb::new(), &postgres().await).await;
}

/// Lost replies, which only the model can inject: TigerBeetle applied a request the caller
/// never heard back about.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL (DATABASE_URL)"]
async fn lost_replies_are_settled_by_recovery() {
    let pg = postgres().await;
    let sim = std::sync::Arc::new(SimTb::new());
    let ledger = hybrid(Shared(sim.clone()), &pg, config(0)).await;
    let tjs = Currency::tjs();
    let m = |v| Money::from_minor(v, tjs);
    let [s, a, b] = [(); 3].map(|_| AccountId::new());
    for (id, t) in [
        (s, AccountType::SystemSettlement),
        (a, AccountType::UserWallet),
        (b, AccountType::UserWallet),
    ] {
        ledger
            .open_account(&LedgerAccount::new(id, t, tjs))
            .await
            .unwrap();
    }
    let pay = |amount| {
        Transaction::with_entries(vec![
            Entry::debit(s, m(amount)),
            Entry::credit(a, m(amount)),
        ])
    };
    let balance = async |id| ledger.tb().balances(&[id]).await.unwrap()[0];

    // The reservation was applied, its reply lost: the request fails, recovery voids.
    sim.inject(Fault::LoseReply(Op::CreateTransfers));
    let txn = pay(100);
    assert!(matches!(
        ledger.post(&txn).await,
        Err(TbError::Unavailable(_))
    ));
    assert_eq!(sim.open_reservations(), 1);
    recover(&ledger).await;
    assert_eq!(sim.open_reservations(), 0);
    assert_eq!(balance(a).await.posted, m(0));
    ledger.post(&txn).await.unwrap();
    assert_eq!(balance(a).await.posted, m(100));

    // The post was applied, its reply lost; or never arrived. Either way the transaction
    // committed, the caller gets success and recovery leaves it posted exactly once.
    for fault in [
        Fault::LoseReply(Op::CreateTransfers),
        Fault::Drop(Op::CreateTransfers),
    ] {
        let txn = pay(10);
        let before = balance(a).await.posted.minor_units();
        sim.inject(Fault::Pass(Op::CreateTransfers)); // the reservation goes through
        sim.inject(fault); // the post does not, or its reply is lost
        ledger.post(&txn).await.unwrap();
        let dropped = matches!(fault, Fault::Drop(_));
        assert_eq!(sim.open_reservations(), dropped as usize, "{fault:?}");
        recover(&ledger).await;
        assert_eq!(sim.open_reservations(), 0);
        assert_eq!(
            balance(a).await.posted.minor_units(),
            before + 10,
            "{fault:?}"
        );
        assert_eq!(
            ledger.post(&txn).await.unwrap_err().as_ledger(),
            Some(&LedgerError::DuplicateTransaction(txn.id))
        );
    }
    assert_eq!(ledger.reconcile(&[s, a, b]).await.unwrap(), vec![]);
}

async fn recover<C: TbClient>(ledger: &crate::HybridLedger<C>) {
    ledger
        .recover_probed(Duration::ZERO, &crate::hybrid::NoFaults)
        .await
        .unwrap();
}

/// Lets a test keep a handle on the model the ledger owns.
struct Shared(std::sync::Arc<SimTb>);

impl TbClient for Shared {
    async fn create_accounts(
        &self,
        a: Vec<Account>,
    ) -> Result<Vec<crate::AccountResult>, crate::ClientError> {
        self.0.create_accounts(a).await
    }
    async fn create_transfers(&self, t: Vec<Transfer>) -> Result<Vec<R>, crate::ClientError> {
        self.0.create_transfers(t).await
    }
    async fn lookup_accounts(&self, ids: Vec<u128>) -> Result<Vec<Account>, crate::ClientError> {
        self.0.lookup_accounts(ids).await
    }
    async fn lookup_transfers(&self, ids: Vec<u128>) -> Result<Vec<Transfer>, crate::ClientError> {
        self.0.lookup_transfers(ids).await
    }
    async fn query_transfers(
        &self,
        f: crate::QueryFilter,
    ) -> Result<Vec<Transfer>, crate::ClientError> {
        self.0.query_transfers(f).await
    }
    fn cluster_id(&self) -> u128 {
        self.0.cluster_id()
    }
}

/// The model's own semantics, on the cases the protocol leans on hardest.
#[tokio::test]
async fn sim_chains_limits_transient_ids_and_expiry() {
    let sim = SimTb::new();
    let acct = |id, flags| Account {
        id,
        ledger: 7,
        code: 1,
        flags,
        ..Account::default()
    };
    let r = sim
        .create_accounts(vec![
            acct(1, af::DEBITS_MUST_NOT_EXCEED_CREDITS),
            acct(2, 0),
            acct(1, af::DEBITS_MUST_NOT_EXCEED_CREDITS),
            acct(2, af::HISTORY),
        ])
        .await
        .unwrap();
    assert_eq!(
        r.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![0, 0, 21, 15] // ok, ok, exists, exists_with_different_flags
    );
    let t = |id, dr, cr, amount, flags| Transfer {
        id,
        debit_account_id: dr,
        credit_account_id: cr,
        amount,
        ledger: 7,
        code: 1,
        flags,
        ..Transfer::default()
    };
    let r = sim
        .create_transfers(vec![
            t(10, 2, 1, 100, tf::LINKED),
            t(11, 1, 2, 500, 0), // exceeds_credits: the chain rolls back
            t(12, 2, 1, 100, 0),
        ])
        .await
        .unwrap();
    assert_eq!(r, vec![R::LINKED_EVENT_FAILED, R::EXCEEDS_CREDITS, R::OK]);
    let r = sim
        .create_transfers(vec![t(10, 2, 1, 100, 0), t(11, 1, 2, 50, 0)])
        .await
        .unwrap();
    assert_eq!(
        r,
        vec![R::OK, R::ID_ALREADY_FAILED],
        "only the failing id is burned"
    );

    let mut p = t(20, 1, 2, 150, tf::PENDING);
    p.timeout = 5;
    let r = sim
        .create_transfers(vec![p, t(21, 1, 2, 100, 0)])
        .await
        .unwrap();
    assert_eq!(r, vec![R::OK, R::EXCEEDS_CREDITS], "pending debits count");
    sim.advance(6);
    let post = Transfer {
        id: 22,
        pending_id: 20,
        amount: crate::AMOUNT_MAX,
        flags: tf::POST_PENDING_TRANSFER,
        ..Transfer::default()
    };
    let r = sim
        .create_transfers(vec![post, t(23, 1, 2, 200, 0)])
        .await
        .unwrap();
    assert_eq!(
        r,
        vec![R::PENDING_TRANSFER_EXPIRED, R::OK],
        "expiry released the hold"
    );
    assert_eq!(sim.open_reservations(), 0);
    // The request size limit the live cluster negotiated.
    assert!(sim
        .create_transfers(vec![Transfer::default(); 254])
        .await
        .is_err());
    let ids = vec![1; crate::client::LOOKUP_BATCH];
    assert!(sim.lookup_transfers(ids).await.is_ok());
}
