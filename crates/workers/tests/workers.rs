//! Integration tests for the sealer + verifier and reconciliation, against a
//! real PostgreSQL.
//!
//! ```bash
//! DATABASE_URL=postgres://payment:payment_dev_pw@localhost:5432/payment \
//!   cargo test -p workers -- --include-ignored
//! ```

use crypto::Sealer;
use ledger::{Account, AccountId, AccountType, Entry, Transaction};
use money::{Currency, Money};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::{Arc, Mutex};
use storage::{LedgerStore, PostgresLedger};
use uuid::Uuid;
use workers::{
    reconcile, relay_all, seal_all, verify_chain, EventPublisher, NatsPublisher, OutboxEvent,
    PublishError, WorkerError,
};

/// A test publisher that records every event it is asked to publish.
struct CapturingPublisher(Arc<Mutex<Vec<OutboxEvent>>>);

impl EventPublisher for CapturingPublisher {
    async fn publish(&self, event: &OutboxEvent) -> Result<(), PublishError> {
        self.0.lock().unwrap().push(event.clone());
        Ok(())
    }
}

fn tjs() -> Currency {
    Currency::tjs()
}

async fn setup() -> (PgPool, PostgresLedger) {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .unwrap();
    let ledger = PostgresLedger::new(pool.clone());
    ledger.migrate().await.unwrap();
    (pool, ledger)
}

/// Seal everything, verify the chain, reconcile — then tamper with a committed
/// entry and prove BOTH the cryptographic verifier and reconciliation detect it,
/// and that restoring the data restores integrity.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn seal_verify_and_detect_tampering() {
    let (pool, ledger) = setup().await;

    // Create some activity to seal: a settlement account funds a user, who pays
    // another user. Unique accounts so the test is independent of prior data.
    let settlement = AccountId::new();
    let alice = AccountId::new();
    let bob = AccountId::new();
    ledger
        .open_account(&Account::new(
            settlement,
            AccountType::SystemSettlement,
            tjs(),
        ))
        .await
        .unwrap();
    ledger
        .open_account(&Account::new(alice, AccountType::UserWallet, tjs()))
        .await
        .unwrap();
    ledger
        .open_account(&Account::new(bob, AccountType::UserWallet, tjs()))
        .await
        .unwrap();

    ledger
        .post(&Transaction::with_entries(vec![
            Entry::debit(settlement, Money::from_minor(50_000, tjs())),
            Entry::credit(alice, Money::from_minor(50_000, tjs())),
        ]))
        .await
        .unwrap();

    // A transaction we will later tamper with.
    let target = Transaction::with_entries(vec![
        Entry::debit(alice, Money::from_minor(12_345, tjs())),
        Entry::credit(bob, Money::from_minor(12_345, tjs())),
    ]);
    ledger.post(&target).await.unwrap();

    // Seal everything outstanding and verify the chain is sound.
    seal_all(&pool, &Sealer::generate(), 500).await.unwrap();
    let report = verify_chain(&pool).await.expect("chain should verify");
    assert!(report.checkpoints_verified >= 1);

    // Reconciliation is healthy.
    assert!(reconcile(&pool).await.unwrap().is_healthy());

    // --- Tamper: inflate the credit to Bob directly in the database. ---
    let row = sqlx::query(
        "SELECT id, amount_minor FROM entries
         WHERE transaction_id = $1 AND direction = 'credit'",
    )
    .bind(target.id.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    let entry_id: Uuid = row.try_get("id").unwrap();
    let original: i64 = row.try_get("amount_minor").unwrap();

    sqlx::query("UPDATE entries SET amount_minor = $2 WHERE id = $1")
        .bind(entry_id)
        .bind(original + 1) // forge one extra diram
        .execute(&pool)
        .await
        .unwrap();

    // The cryptographic verifier detects the ledger tampering.
    match verify_chain(&pool).await {
        Err(WorkerError::ChainBroken { .. }) => {}
        other => panic!("expected ChainBroken, got {other:?}"),
    }

    // And reconciliation flags the now-inconsistent balances.
    let tampered_report = reconcile(&pool).await.unwrap();
    assert!(!tampered_report.is_healthy());

    // --- Restore the original value; integrity returns. ---
    sqlx::query("UPDATE entries SET amount_minor = $2 WHERE id = $1")
        .bind(entry_id)
        .bind(original)
        .execute(&pool)
        .await
        .unwrap();

    verify_chain(&pool)
        .await
        .expect("chain verifies again after restore");
    assert!(reconcile(&pool).await.unwrap().is_healthy());
}

/// Posting writes an outbox row in the same transaction; the relay then
/// publishes it exactly once and marks it sent.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn outbox_relay_publishes_posted_events() {
    let (pool, ledger) = setup().await;

    let settlement = AccountId::new();
    let alice = AccountId::new();
    ledger
        .open_account(&Account::new(
            settlement,
            AccountType::SystemSettlement,
            tjs(),
        ))
        .await
        .unwrap();
    ledger
        .open_account(&Account::new(alice, AccountType::UserWallet, tjs()))
        .await
        .unwrap();

    let txn = Transaction::with_entries(vec![
        Entry::debit(settlement, Money::from_minor(7_777, tjs())),
        Entry::credit(alice, Money::from_minor(7_777, tjs())),
    ]);
    ledger.post(&txn).await.unwrap();

    // The outbox row exists and is unsent immediately after posting (atomic write).
    let unsent: bool =
        sqlx::query("SELECT sent_at IS NULL AS unsent FROM outbox WHERE aggregate_id = $1")
            .bind(txn.id.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap()
            .try_get("unsent")
            .unwrap();
    assert!(unsent, "outbox row should exist and be unsent after post");

    // Relay publishes it; our event must appear, with the right type and payload.
    let captured = Arc::new(Mutex::new(Vec::new()));
    let publisher = CapturingPublisher(captured.clone());
    let n = relay_all(&pool, &publisher, 100).await.unwrap();
    assert!(n >= 1);

    {
        let events = captured.lock().unwrap();
        let ours = events
            .iter()
            .find(|e| e.aggregate_id == txn.id.as_uuid())
            .expect("our event was relayed");
        assert_eq!(ours.event_type, "transaction.posted");
        assert_eq!(ours.payload["transaction_id"], txn.id.as_uuid().to_string());
        assert_eq!(ours.payload["entries"].as_array().unwrap().len(), 2);
    }

    // It is now marked sent, and a second relay does not re-publish it.
    let sent: bool =
        sqlx::query("SELECT sent_at IS NOT NULL AS sent FROM outbox WHERE aggregate_id = $1")
            .bind(txn.id.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap()
            .try_get("sent")
            .unwrap();
    assert!(sent, "outbox row should be marked sent after relay");

    let captured2 = Arc::new(Mutex::new(Vec::new()));
    relay_all(&pool, &CapturingPublisher(captured2.clone()), 100)
        .await
        .unwrap();
    assert!(
        !captured2
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.aggregate_id == txn.id.as_uuid()),
        "already-sent event must not be relayed again"
    );
}

/// `NatsPublisher` delivers an event to the right subject and a subscriber
/// receives the JSON payload. Self-contained (no DB): the relay→outbox path is
/// covered by `outbox_relay_publishes_posted_events`; here we isolate the NATS
/// transport so concurrent tests draining the shared outbox can't interfere.
/// Requires a NATS server; set NATS_URL (defaults to localhost).
#[tokio::test]
#[ignore = "requires NATS (docker compose up -d)"]
async fn nats_publisher_delivers_events() {
    use futures::StreamExt;
    use std::time::Duration;
    use workers::OutboxEvent;

    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());

    // A unique subject prefix so this test is isolated from any other traffic.
    let prefix = format!("test-{}", Uuid::new_v4().simple());
    let subject = format!("{prefix}.transaction.posted");

    let client = async_nats::connect(&nats_url).await.unwrap();
    let mut sub = client.subscribe(subject).await.unwrap();
    client.flush().await.unwrap();

    let txn_id = Uuid::new_v4();
    let event = OutboxEvent {
        id: Uuid::new_v4(),
        aggregate_id: txn_id,
        event_type: "transaction.posted".to_string(),
        payload: serde_json::json!({
            "transaction_id": txn_id.to_string(),
            "entries": [{"account_id": Uuid::new_v4().to_string(), "direction": "debit",
                         "amount_minor": 4_242, "currency": "TJS"}],
        }),
    };

    let publisher = NatsPublisher::connect(&nats_url, &prefix).await.unwrap();
    publisher.publish(&event).await.unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("a message should arrive within 5s")
        .expect("subscription open");
    let payload: serde_json::Value = serde_json::from_slice(&msg.payload).unwrap();
    assert_eq!(payload["transaction_id"], txn_id.to_string());
    assert!(payload["entries"].is_array());
}
