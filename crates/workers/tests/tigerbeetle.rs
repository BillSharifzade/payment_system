//! The workers' `LEDGER_BACKEND=tigerbeetle` duties: the recovery loop, and reconciliation
//! telling a committed post still in flight from real drift. Each test has a scratch database.
//! Against the in-process cluster model by default; against a live cluster with
//! TEST_LEDGER_BACKEND=tigerbeetle (TIGERBEETLE_ADDRESSES, TIGERBEETLE_CLUSTER_ID; build with
//! `--features tigerbeetle`).

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::scratch;
use ledger::{Account, AccountId, AccountType, Entry, Transaction};
use ledger_tigerbeetle::testkit::{post_and_crash, recover_all, CrashPoint};
use ledger_tigerbeetle::{AnyTb, HybridLedger, SimTb, TbClient, TbConfig, TbLedger};
use money::{Currency, Money};
use storage::PostgresLedger;
use tokio::sync::watch;
use uuid::Uuid;
use workers::{confirm, reconcile_incremental, recovery_loop, Balances, Derive, ReconcileConfig};

fn cluster() -> AnyTb {
    match std::env::var("TEST_LEDGER_BACKEND").as_deref() {
        Ok("tigerbeetle") => live(),
        Err(_) | Ok("postgres") => AnyTb::Sim(Arc::new(SimTb::new())),
        Ok(other) => panic!("TEST_LEDGER_BACKEND={other:?}: postgres or tigerbeetle"),
    }
}

#[cfg(feature = "tigerbeetle")]
fn live() -> AnyTb {
    AnyTb::Live(Arc::new(ledger_tigerbeetle::LiveTb::from_test_env()))
}

#[cfg(not(feature = "tigerbeetle"))]
fn live() -> AnyTb {
    panic!("TEST_LEDGER_BACKEND=tigerbeetle needs `--features tigerbeetle`")
}

async fn hybrid(pg: PostgresLedger, recovery_grace: Duration) -> HybridLedger<AnyTb> {
    let tb = cluster();
    let cfg = TbConfig {
        recovery_grace,
        ..TbConfig::new(tb.cluster_id(), "test")
    };
    let ledger = HybridLedger::new(TbLedger::new(tb, cfg), pg);
    ledger.tb().ensure_control_accounts().await.unwrap();
    ledger
}

fn tjs(minor: i128) -> Money {
    Money::from_minor(minor, Currency::tjs())
}

fn transfer(from: AccountId, to: AccountId, minor: i128) -> Transaction {
    Transaction::with_entries(vec![
        Entry::debit(from, tjs(minor)),
        Entry::credit(to, tjs(minor)),
    ])
}

struct Books {
    settlement: AccountId,
    alice: AccountId,
    bob: AccountId,
}

const FUNDED: i128 = 10_000;

async fn books(ledger: &HybridLedger<AnyTb>) -> Books {
    let b = Books {
        settlement: AccountId::new(),
        alice: AccountId::new(),
        bob: AccountId::new(),
    };
    for (id, t) in [
        (b.settlement, AccountType::SystemSettlement),
        (b.alice, AccountType::UserWallet),
        (b.bob, AccountType::UserWallet),
    ] {
        ledger
            .open_account(&Account::new(id, t, Currency::tjs()))
            .await
            .unwrap();
    }
    ledger
        .post(&transfer(b.settlement, b.alice, FUNDED))
        .await
        .unwrap();
    b
}

/// (posted, available) of `account`, in minor units.
async fn balance(ledger: &HybridLedger<AnyTb>, account: AccountId) -> (i128, i128) {
    let b = ledger.tb().balances(&[account]).await.unwrap()[0];
    (b.posted.minor_units(), b.available.minor_units())
}

async fn outcome(pool: &sqlx::PgPool, txn: &Transaction) -> Option<String> {
    sqlx::query_scalar("SELECT outcome FROM tb_intents WHERE transaction_id = $1")
        .bind(txn.id.as_uuid())
        .fetch_optional(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL (DATABASE_URL)"]
async fn the_recovery_loop_settles_abandoned_reservations_while_leader() {
    let s = scratch("tb_recovery_loop").await;
    let ledger = hybrid(s.ledger.clone(), Duration::from_secs(1)).await;
    let b = books(&ledger).await;

    // One request died after its COMMIT, one before it: both left their reservation held.
    let committed = transfer(b.alice, b.bob, 100);
    post_and_crash(&ledger, &committed, CrashPoint::Committed).await;
    let abandoned = transfer(b.alice, b.bob, 50);
    post_and_crash(&ledger, &abandoned, CrashPoint::Reserved).await;
    assert_eq!(balance(&ledger, b.alice).await, (FUNDED, FUNDED - 150));

    let (leader_tx, leader) = watch::channel(false);
    let (stop_tx, stop) = watch::channel(false);
    let beats = Arc::new(AtomicUsize::new(0));
    let task = tokio::spawn(recovery_loop(
        ledger.clone(),
        Duration::from_millis(100),
        leader,
        stop,
        {
            let beats = beats.clone();
            move || {
                beats.fetch_add(1, Ordering::Relaxed);
            }
        },
    ));

    // Past the grace, but not the leader: nothing is settled, the loop is alive.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(balance(&ledger, b.alice).await, (FUNDED, FUNDED - 150));
    assert!(beats.load(Ordering::Relaxed) >= 5);

    leader_tx.send_replace(true);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while balance(&ledger, b.alice).await != (FUNDED - 100, FUNDED - 100) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "recovery never settled: {:?}",
            balance(&ledger, b.alice).await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The committed one posted exactly once, the other was voided; the cluster equals the
    // journal, which holds the first and not the second.
    assert_eq!(balance(&ledger, b.bob).await, (100, 100));
    assert_eq!(
        outcome(&s.pool, &committed).await.as_deref(),
        Some("commit")
    );
    assert_eq!(outcome(&s.pool, &abandoned).await.as_deref(), Some("void"));
    let ids = [b.settlement, b.alice, b.bob];
    assert_eq!(ledger.reconcile(&ids).await.unwrap(), vec![]);

    stop_tx.send_replace(true);
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the loop stops")
        .unwrap();
    s.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL (DATABASE_URL)"]
async fn reconciliation_tells_a_post_in_flight_from_drift() {
    let s = scratch("tb_reconcile").await;
    // The default grace: recovery leaves the young reservation below to its request.
    let ledger = hybrid(s.ledger.clone(), Duration::from_secs(10)).await;
    let b = books(&ledger).await;
    let cfg = ReconcileConfig::default();
    let tb = Balances::TigerBeetle(&ledger);

    // Committed, its post not applied yet: the cluster lags the journal by exactly it, which
    // the naive comparison flags and the re-check clears.
    let in_flight = transfer(b.alice, b.bob, 100);
    post_and_crash(&ledger, &in_flight, CrashPoint::Committed).await;
    assert_eq!(balance(&ledger, b.bob).await, (0, 0));
    let pass = reconcile_incremental(&s.pool, &cfg, &[], tb).await.unwrap();
    assert!(pass.is_healthy(), "{pass:?}");
    assert_eq!(pass.accounts_checked, 3);
    assert!(pass.inconclusive.is_empty(), "{pass:?}");
    assert_eq!(pass.recovered.posted, 0, "too young for recovery");

    // Drift: money moved in the cluster behind the journal's back.
    ledger
        .tb()
        .post_direct(&transfer(b.settlement, b.bob, 7))
        .await
        .unwrap();
    let all = [b.settlement, b.alice, b.bob].map(|a| a.as_uuid());
    let found = confirm(&s.pool, &ledger, &all, Derive::Folded)
        .await
        .unwrap();
    // Reported as read: bob's cluster balance (the drift's 7) against his journal (the
    // in-flight 100). Flagged for the 7 alone: with the 100 subtracted they would agree.
    let mut drifted: Vec<(Uuid, i64, i64)> = found
        .mismatches
        .iter()
        .map(|m| (m.account_id, m.stored_minor, m.derived_minor))
        .collect();
    drifted.sort();
    let mut expected = vec![
        (
            b.settlement.as_uuid(),
            -(FUNDED as i64) - 7,
            -(FUNDED as i64),
        ),
        (b.bob.as_uuid(), 7, 100),
    ];
    expected.sort();
    assert_eq!(drifted, expected, "{found:?}");
    assert!(found.inconclusive.is_empty());

    // Once recovery posted the committed transaction, the drift alone remains.
    assert_eq!(recover_all(&ledger).await.posted, 1);
    assert_eq!(balance(&ledger, b.bob).await, (107, 107));
    let pass = reconcile_incremental(&s.pool, &cfg, &all, tb)
        .await
        .unwrap();
    let mut flagged: Vec<(Uuid, i64)> = pass
        .balance_mismatches
        .iter()
        .map(|m| (m.account_id, m.stored_minor - m.derived_minor))
        .collect();
    flagged.sort();
    let mut drift = vec![(b.settlement.as_uuid(), -7), (b.bob.as_uuid(), 7)];
    drift.sort();
    assert_eq!(flagged, drift, "{pass:?}");

    ledger
        .tb()
        .post_direct(&transfer(b.bob, b.settlement, 7))
        .await
        .unwrap();
    let pass = reconcile_incremental(&s.pool, &cfg, &all, tb)
        .await
        .unwrap();
    assert!(pass.is_healthy(), "{pass:?}");
    s.drop_db().await;
}
