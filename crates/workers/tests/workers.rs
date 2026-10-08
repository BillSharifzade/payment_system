use crypto::{Sealer, TrustedKeys};
use ledger::{Account, AccountId, AccountType, Entry, Transaction};
use money::{Currency, Money};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::{Arc, Mutex};
use storage::{LedgerStore, PostgresLedger};
use uuid::Uuid;
use workers::{
    prune_expired, reconcile, reconcile_full, reconcile_incremental, relay_all, seal_all,
    verify_chain, verify_chain_from, EventPublisher, FullReconciliation, LeaderLock, NatsPublisher,
    OutboxEvent, PublishError, ReconcileConfig, RetentionConfig, VerifyState, WorkerError,
};

// The tests share one database and its single checkpoint chain, so they run
// with --test-threads=1 and seal with fixed keys: A is the original signing
// key, B the key it was rotated to.
fn key_a() -> Sealer {
    Sealer::from_secret_bytes(&[0xA1; 32])
}

fn key_b() -> Sealer {
    Sealer::from_secret_bytes(&[0xB2; 32])
}

fn trusted() -> TrustedKeys {
    TrustedKeys::new()
        .with(key_a().public_key_bytes())
        .unwrap()
        .with(key_b().public_key_bytes())
        .unwrap()
}

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

/// Runs `sql` with triggers disabled — what a superuser tampering with the
/// database can do and the runtime role cannot.
async fn as_superuser(pool: &PgPool, sql: &str, binds: &[Uuid], value: Option<i64>) {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    let mut q = sqlx::query(sql);
    for b in binds {
        q = q.bind(*b);
    }
    if let Some(v) = value {
        q = q.bind(v);
    }
    q.execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

struct Books {
    settlement: AccountId,
    alice: AccountId,
    bob: AccountId,
}

async fn open_books(ledger: &PostgresLedger) -> Books {
    let books = Books {
        settlement: AccountId::new(),
        alice: AccountId::new(),
        bob: AccountId::new(),
    };
    ledger
        .open_account(&Account::new(
            books.settlement,
            AccountType::SystemSettlement,
            tjs(),
        ))
        .await
        .unwrap();
    for id in [books.alice, books.bob] {
        ledger
            .open_account(&Account::new(id, AccountType::UserWallet, tjs()))
            .await
            .unwrap();
    }
    books
}

fn transfer(from: AccountId, to: AccountId, minor: i128) -> Transaction {
    Transaction::with_entries(vec![
        Entry::debit(from, Money::from_minor(minor, tjs())),
        Entry::credit(to, Money::from_minor(minor, tjs())),
    ])
}

fn broken_reason(result: workers::Result<impl std::fmt::Debug>) -> (i64, String) {
    match result {
        Err(WorkerError::ChainBroken { seq, reason }) => (seq, reason),
        other => panic!("expected ChainBroken, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn seal_verify_and_detect_tampering() {
    let (pool, ledger) = setup().await;
    let b = open_books(&ledger).await;

    ledger
        .post(&transfer(b.settlement, b.alice, 50_000))
        .await
        .unwrap();
    let target = transfer(b.alice, b.bob, 12_345);
    ledger.post(&target).await.unwrap();

    seal_all(&pool, &key_a(), 500).await.unwrap();
    let report = verify_chain(&pool, &trusted())
        .await
        .expect("chain should verify");
    assert!(report.checkpoints_verified >= 1);
    assert!(reconcile(&pool).await.unwrap().is_healthy());

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

    let err = sqlx::query("UPDATE entries SET amount_minor = amount_minor + 1 WHERE id = $1")
        .bind(entry_id)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("append-only"), "{err}");

    let tamper = "UPDATE entries SET amount_minor = $2 WHERE id = $1";
    as_superuser(&pool, tamper, &[entry_id], Some(original + 1)).await;

    let (_, reason) = broken_reason(verify_chain(&pool, &trusted()).await);
    assert!(reason.contains("Merkle root"), "{reason}");
    let tampered = reconcile(&pool).await.unwrap();
    assert!(!tampered.is_healthy());
    assert!(tampered
        .balance_mismatches
        .iter()
        .any(|m| m.account_id == b.bob.as_uuid()));
    assert!(
        tampered
            .sum_mismatches
            .iter()
            .any(|m| m.account_id == b.bob.as_uuid()),
        "the reconciled sums must notice history changing under them"
    );

    as_superuser(&pool, tamper, &[entry_id], Some(original)).await;
    verify_chain(&pool, &trusted())
        .await
        .expect("chain verifies again after restore");
    assert!(reconcile(&pool).await.unwrap().is_healthy());
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn only_trusted_keys_verify_and_rotation_keeps_history_valid() {
    let (pool, ledger) = setup().await;
    let b = open_books(&ledger).await;

    ledger
        .post(&transfer(b.settlement, b.alice, 1_000))
        .await
        .unwrap();
    seal_all(&pool, &key_a(), 500).await.unwrap();
    ledger.post(&transfer(b.alice, b.bob, 400)).await.unwrap();
    seal_all(&pool, &key_b(), 500).await.unwrap();

    verify_chain(&pool, &trusted())
        .await
        .expect("both the retired and the current key are trusted");

    let only_b = TrustedKeys::new().with(key_b().public_key_bytes()).unwrap();
    let first_by_a: i64 =
        sqlx::query_scalar("SELECT MIN(seq) FROM checkpoints WHERE public_key = $1")
            .bind(key_a().public_key_bytes().to_vec())
            .fetch_one(&pool)
            .await
            .unwrap();
    let (seq, reason) = broken_reason(verify_chain(&pool, &only_b).await);
    assert_eq!(seq, first_by_a);
    assert_eq!(reason, "untrusted signing key");

    // Rewriting history needs a valid signature; the attacker can only make
    // one with a key of their own, which the verifier does not trust.
    let attacker = Sealer::generate();
    let last = sqlx::query(
        "SELECT seq, checkpoint_hash, signature, public_key FROM checkpoints
         ORDER BY seq DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let last_seq: i64 = last.try_get("seq").unwrap();
    let hash: Vec<u8> = last.try_get("checkpoint_hash").unwrap();
    let signature: Vec<u8> = last.try_get("signature").unwrap();
    let public_key: Vec<u8> = last.try_get("public_key").unwrap();
    let forged = attacker.sign_hash(&crypto::Hash::from_bytes(hash.try_into().unwrap()));
    let resign = |sig: Vec<u8>, pk: Vec<u8>| {
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SET LOCAL session_replication_role = replica")
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("UPDATE checkpoints SET signature = $2, public_key = $3 WHERE seq = $1")
                .bind(last_seq)
                .bind(sig)
                .bind(pk)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
    };
    resign(forged.to_vec(), attacker.public_key_bytes().to_vec()).await;
    let (seq, reason) = broken_reason(verify_chain(&pool, &trusted()).await);
    assert_eq!((seq, reason.as_str()), (last_seq, "untrusted signing key"));
    resign(signature, public_key).await;
    verify_chain(&pool, &trusted()).await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn incremental_verification_misses_old_tampering_full_reverification_catches_it() {
    let (pool, ledger) = setup().await;
    let b = open_books(&ledger).await;
    let old = transfer(b.settlement, b.alice, 2_000);
    ledger.post(&old).await.unwrap();
    seal_all(&pool, &key_b(), 500).await.unwrap();

    let (_, state) = verify_chain_from(&pool, &trusted(), &VerifyState::genesis())
        .await
        .unwrap();

    let tamper = "UPDATE entries SET amount_minor = $2 WHERE transaction_id = $1";
    as_superuser(&pool, tamper, &[old.id.as_uuid()], Some(2_001)).await;

    let (report, _) = verify_chain_from(&pool, &trusted(), &state).await.unwrap();
    assert_eq!(
        report.checkpoints_verified, 0,
        "nothing new to verify incrementally"
    );
    let (_, reason) = broken_reason(verify_chain(&pool, &trusted()).await);
    assert!(reason.contains("Merkle root"), "{reason}");
    as_superuser(&pool, tamper, &[old.id.as_uuid()], Some(2_000)).await;
    verify_chain(&pool, &trusted()).await.unwrap();

    // The checkpoint an incremental verifier stopped at is re-checked on resume.
    let swap = |hash: Vec<u8>| {
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SET LOCAL session_replication_role = replica")
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("UPDATE checkpoints SET checkpoint_hash = $2 WHERE seq = $1")
                .bind(state.last_checkpoint_seq)
                .bind(hash)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
    };
    swap(vec![0u8; 32]).await;
    let (seq, reason) = broken_reason(verify_chain_from(&pool, &trusted(), &state).await);
    assert_eq!(seq, state.last_checkpoint_seq);
    assert!(reason.contains("altered or removed"), "{reason}");
    swap(state.last_checkpoint_hash.to_vec()).await;
    verify_chain_from(&pool, &trusted(), &state).await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn verification_pages_through_a_long_chain_and_catches_uncovered_rows() {
    let (pool, ledger) = setup().await;
    seal_all(&pool, &key_b(), 500).await.unwrap();
    let b = open_books(&ledger).await;
    ledger
        .post(&transfer(b.settlement, b.alice, 1_000))
        .await
        .unwrap();
    for _ in 0..(workers::VERIFY_PAGE_CHECKPOINTS + 6) {
        ledger.post(&transfer(b.alice, b.bob, 1)).await.unwrap();
    }
    let sealed = seal_all(&pool, &key_b(), 1).await.unwrap();
    assert!(sealed as i64 > workers::VERIFY_PAGE_CHECKPOINTS);

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM checkpoints")
        .fetch_one(&pool)
        .await
        .unwrap();
    let report = verify_chain(&pool, &trusted()).await.unwrap();
    assert_eq!(report.checkpoints_verified as i64, total);

    // A sealed row no checkpoint covers is planted history.
    let err = sqlx::query("INSERT INTO transactions (id, sealed_seq) VALUES ($1, 1)")
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already sealed"), "{err}");
    let planted = Uuid::now_v7();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO transactions (id, sealed_seq)
         SELECT $1, MAX(to_txn_seq) + 1000 FROM checkpoints",
    )
    .bind(planted)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (_, reason) = broken_reason(verify_chain(&pool, &trusted()).await);
    assert!(reason.contains("not covered"), "{reason}");
    as_superuser(
        &pool,
        "DELETE FROM transactions WHERE id = $1",
        &[planted],
        None,
    )
    .await;
    verify_chain(&pool, &trusted()).await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn verify_chain_command_reports_json_and_exit_code() {
    let (pool, ledger) = setup().await;
    let b = open_books(&ledger).await;
    ledger
        .post(&transfer(b.settlement, b.alice, 10))
        .await
        .unwrap();
    seal_all(&pool, &key_b(), 500).await.unwrap();
    // One void-style transaction without entries, sealed like any other.
    sqlx::query("INSERT INTO transactions (id) VALUES ($1)")
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .unwrap();

    let run = |keys: String, url: String| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_payment-workers"))
            .arg("verify-chain")
            .env("DATABASE_URL", url)
            .env("WORKER_TRUSTED_PUBLIC_KEYS", keys)
            .env_remove("WORKER_SIGNING_KEY")
            .env_remove("WORKER_SIGNING_KEY_FILE")
            .output()
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
        (out.status.code(), json)
    };
    let url = std::env::var("DATABASE_URL").unwrap();
    let keys = trusted().to_hex().join(",");

    let (code, json) = run(keys.clone(), url.clone());
    assert_eq!(code, Some(0), "{json}");
    assert_eq!(json["status"], "intact");
    assert!(json["checkpoints_verified"].as_u64().unwrap() >= 1);
    assert!(json["unsealed_transactions"].as_i64().unwrap() >= 1);

    let (code, json) = run(Sealer::generate().public_key_hex(), url);
    assert_eq!(code, Some(1), "{json}");
    assert_eq!(json["status"], "broken");
    assert_eq!(json["broken_at_seq"], 1);
    assert_eq!(json["reason"], "untrusted signing key");

    let (code, json) = run(keys, "postgres://nobody:wrong@127.0.0.1:1/none".into());
    assert_eq!(code, Some(2), "{json}");
    assert_eq!(json["status"], "error");

    seal_all(&pool, &key_b(), 500).await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn incremental_reconciliation_uses_the_watermark() {
    let (pool, ledger) = setup().await;
    let cfg = ReconcileConfig::default();
    seal_all(&pool, &key_b(), 500).await.unwrap();
    assert!(reconcile(&pool).await.unwrap().is_healthy());

    let b = open_books(&ledger).await;
    ledger
        .post(&transfer(b.settlement, b.alice, 9_000))
        .await
        .unwrap();
    ledger.post(&transfer(b.alice, b.bob, 1_000)).await.unwrap();

    // Unsealed transactions are reconciled from the tail.
    let pass = reconcile_incremental(&pool, &cfg, &[]).await.unwrap();
    assert!(pass.is_healthy(), "{pass:?}");
    assert_eq!(pass.accounts_checked, 3);
    assert_eq!(pass.transactions_folded, 0);

    seal_all(&pool, &key_b(), 500).await.unwrap();
    let pass = reconcile_incremental(&pool, &cfg, &[]).await.unwrap();
    assert!(pass.is_healthy(), "{pass:?}");
    assert_eq!(pass.transactions_folded, 2);
    let base: i64 =
        sqlx::query_scalar("SELECT sum_minor FROM reconciled_sums WHERE account_id = $1")
            .bind(b.alice.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(base, 8_000);
    let pass = reconcile_incremental(&pool, &cfg, &[]).await.unwrap();
    assert_eq!(
        (pass.accounts_checked, pass.transactions_folded),
        (0, 0),
        "nothing new, nothing re-read"
    );

    // A drifted balance is caught when its account is next touched...
    let drift = "UPDATE balances SET raw_minor = raw_minor + $2 WHERE account_id = $1";
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(drift)
        .bind(b.bob.as_uuid())
        .bind(5i64)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let pass = reconcile_incremental(&pool, &cfg, &[]).await.unwrap();
    assert!(
        pass.is_healthy(),
        "bob untouched: not visible incrementally"
    );
    let full = reconcile_full(&pool, &cfg).await.unwrap();
    assert_eq!(full.balance_mismatches.len(), 1);
    assert_eq!(full.balance_mismatches[0].account_id, b.bob.as_uuid());
    assert_eq!(full.currency_imbalances, vec![("TJS".to_string(), 5)]);

    ledger.post(&transfer(b.alice, b.bob, 1)).await.unwrap();
    let pass = reconcile_incremental(&pool, &cfg, &[]).await.unwrap();
    assert_eq!(pass.balance_mismatches.len(), 1, "{pass:?}");
    assert_eq!(pass.balance_mismatches[0].stored_minor, 1_006);
    assert_eq!(pass.balance_mismatches[0].derived_minor, 1_001);

    // ...and keeps being reported, as a suspect, until it is fixed.
    let suspects = [b.bob.as_uuid()];
    let pass = reconcile_incremental(&pool, &cfg, &suspects).await.unwrap();
    assert_eq!(pass.balance_mismatches.len(), 1);
    sqlx::query(drift)
        .bind(b.bob.as_uuid())
        .bind(-5i64)
        .execute(&pool)
        .await
        .unwrap();
    let pass = reconcile_incremental(&pool, &cfg, &suspects).await.unwrap();
    assert!(pass.is_healthy());

    // The sums table itself is audited by the full pass.
    let skew = "UPDATE reconciled_sums SET sum_minor = sum_minor + $2 WHERE account_id = $1";
    sqlx::query(skew)
        .bind(b.alice.as_uuid())
        .bind(7i64)
        .execute(&pool)
        .await
        .unwrap();
    let full = reconcile_full(&pool, &cfg).await.unwrap();
    assert_eq!(full.sum_mismatches.len(), 1, "{full:?}");
    assert_eq!(full.sum_mismatches[0].account_id, b.alice.as_uuid());
    assert!(full.balance_mismatches.is_empty());
    sqlx::query(skew)
        .bind(b.alice.as_uuid())
        .bind(-7i64)
        .execute(&pool)
        .await
        .unwrap();
    assert!(reconcile(&pool).await.unwrap().is_healthy());
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn reconciliation_works_in_bounded_chunks() {
    let (pool, ledger) = setup().await;
    seal_all(&pool, &key_b(), 500).await.unwrap();
    assert!(reconcile(&pool).await.unwrap().is_healthy());

    let b = open_books(&ledger).await;
    ledger
        .post(&transfer(b.settlement, b.alice, 100_000))
        .await
        .unwrap();
    for i in 0..40 {
        let (from, to) = if i % 3 == 2 {
            (b.bob, b.alice)
        } else {
            (b.alice, b.bob)
        };
        ledger.post(&transfer(from, to, 10)).await.unwrap();
    }
    seal_all(&pool, &key_b(), 500).await.unwrap();

    // A backlog is folded a chunk at a time, unchecked, before the tail check.
    let cfg = ReconcileConfig {
        fold_chunk: 7,
        max_fold_chunks: 2,
        full_chunk_entries: 9,
    };
    let mut calls = 0;
    let mut unchecked = 0;
    loop {
        let pass = reconcile_incremental(&pool, &cfg, &[]).await.unwrap();
        calls += 1;
        unchecked += pass.folded_unchecked;
        if !pass.backlog_remaining {
            assert!(pass.is_healthy(), "{pass:?}");
            break;
        }
    }
    assert!(
        calls >= 3,
        "41 sealed rows in chunks of 7, two chunks per call"
    );
    assert!(unchecked >= 35);

    // Alice has 41 entries: above full_chunk_entries, so her history is
    // summed in pages of 9 rather than by one statement. Only this test's
    // accounts are walked, so the pages stay few however big the database is.
    let first = [b.settlement, b.alice, b.bob]
        .iter()
        .map(|a| a.as_uuid().as_u128())
        .min()
        .unwrap();
    let ours = || async {
        let mut full = FullReconciliation::starting_after(Uuid::from_u128(first - 1));
        while !full.step(&pool, &cfg).await.unwrap() {}
        full.into_report()
    };
    let full = ours().await;
    assert!(full.is_healthy(), "{full:?}");
    assert!(full.accounts_checked >= 3);
    let drift = "UPDATE balances SET raw_minor = raw_minor + $2 WHERE account_id = $1";
    sqlx::query(drift)
        .bind(b.alice.as_uuid())
        .bind(3i64)
        .execute(&pool)
        .await
        .unwrap();
    let skew = "UPDATE reconciled_sums SET sum_minor = sum_minor + $2 WHERE account_id = $1";
    sqlx::query(skew)
        .bind(b.alice.as_uuid())
        .bind(4i64)
        .execute(&pool)
        .await
        .unwrap();
    let full = ours().await;
    assert_eq!(full.balance_mismatches.len(), 1, "{full:?}");
    assert_eq!(full.balance_mismatches[0].account_id, b.alice.as_uuid());
    assert_eq!(
        full.balance_mismatches[0].stored_minor - full.balance_mismatches[0].derived_minor,
        3
    );
    assert_eq!(full.sum_mismatches.len(), 1, "{full:?}");
    assert_eq!(
        full.sum_mismatches[0].stored_minor - full.sum_mismatches[0].derived_minor,
        4
    );
    sqlx::query(drift)
        .bind(b.alice.as_uuid())
        .bind(-3i64)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(skew)
        .bind(b.alice.as_uuid())
        .bind(-4i64)
        .execute(&pool)
        .await
        .unwrap();
    assert!(ours().await.is_healthy());
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn retention_pruning_still_works_with_append_only_triggers() {
    let (pool, _ledger) = setup().await;
    let outbox_id = Uuid::now_v7();
    let key = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outbox (id, aggregate_id, event_type, payload, created_at, sent_at)
         VALUES ($1, $1, 'test', '{}', now() - interval '40 days', now() - interval '40 days')",
    )
    .bind(outbox_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO idempotency_keys (key, fingerprint, response_status, response_body, created_at)
         VALUES ($1, 'fp', 201, '{}', now() - interval '40 days')",
    )
    .bind(key)
    .execute(&pool)
    .await
    .unwrap();

    let report = prune_expired(&pool, &RetentionConfig::default())
        .await
        .unwrap();
    assert!(report.outbox >= 1 && report.idempotency_keys >= 1);
    let left: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM outbox WHERE id = $1)
              + (SELECT COUNT(*) FROM idempotency_keys WHERE key = $2)",
    )
    .bind(outbox_id)
    .bind(key)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn only_one_replica_holds_the_leader_lock() {
    let url = std::env::var("DATABASE_URL").unwrap();
    let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let key = i64::from_le_bytes(Uuid::new_v4().as_bytes()[..8].try_into().unwrap());

    let mut first = LeaderLock::try_acquire(&options, key)
        .await
        .unwrap()
        .expect("free lock is acquired");
    assert!(LeaderLock::try_acquire(&options, key)
        .await
        .unwrap()
        .is_none());
    first.check().await.unwrap();
    first.release().await.unwrap();
    let second = LeaderLock::try_acquire(&options, key).await.unwrap();
    assert!(second.is_some(), "released lock passes to the standby");
    drop(second);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn outbox_relay_publishes_posted_events() {
    let (pool, ledger) = setup().await;
    let b = open_books(&ledger).await;

    let txn = transfer(b.settlement, b.alice, 7_777);
    ledger.post(&txn).await.unwrap();

    let unsent: bool =
        sqlx::query("SELECT sent_at IS NULL AS unsent FROM outbox WHERE aggregate_id = $1")
            .bind(txn.id.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap()
            .try_get("unsent")
            .unwrap();
    assert!(unsent, "outbox row should exist and be unsent after post");

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

#[tokio::test]
#[ignore = "requires NATS (docker compose up -d)"]
async fn nats_publisher_delivers_events() {
    use futures::StreamExt;
    use std::time::Duration;
    use workers::OutboxEvent;

    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());

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
