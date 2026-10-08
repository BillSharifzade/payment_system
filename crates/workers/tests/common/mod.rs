#![allow(dead_code)]

use std::str::FromStr;

use crypto::{Sealer, TrustedKeys};
use ledger::{Account, AccountId, AccountType, Entry, Transaction};
use money::{Currency, Money};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use storage::{LedgerStore, PostgresLedger};
use workers::VerifyState;

/// Ignored tests that need more than Postgres and NATS (Vault, SoftHSM, the
/// public witnesses) skip — loudly — when it is not configured, so
/// `cargo test -p workers -- --ignored` passes without it. tamper.yml sets
/// TAMPER_REQUIRE_EXTERNAL=1: there a missing service fails the test.
pub fn external(what: &str, available: bool) -> bool {
    if !available {
        assert!(
            std::env::var_os("TAMPER_REQUIRE_EXTERNAL").is_none(),
            "{what} is required (TAMPER_REQUIRE_EXTERNAL is set)"
        );
        eprintln!("SKIPPED: {what} is not configured");
    }
    available
}

pub fn vault_configured() -> bool {
    external(
        "Vault (VAULT_ADDR + admin VAULT_TOKEN)",
        std::env::var_os("VAULT_ADDR").is_some() && std::env::var_os("VAULT_TOKEN").is_some(),
    )
}

pub fn key_a() -> Sealer {
    Sealer::from_secret_bytes(&[0xA1; 32])
}

pub fn trusted() -> TrustedKeys {
    TrustedKeys::new().with(key_a().public_key_bytes()).unwrap()
}

fn tjs() -> Currency {
    Currency::tjs()
}

/// A fresh database per test (created next to DATABASE_URL's, so the role
/// needs CREATEDB): the attack tests rewrite the whole chain.
pub struct Scratch {
    admin: PgPool,
    name: String,
    pub url: String,
    pub pool: PgPool,
    pub ledger: PostgresLedger,
}

pub async fn scratch(tag: &str) -> Scratch {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let name = format!("tamper_test_{tag}");
    sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .execute(&admin)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let opts = PgConnectOptions::from_str(&url).unwrap().database(&name);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .unwrap();
    let ledger = PostgresLedger::new(pool.clone());
    ledger.migrate().await.unwrap();
    let (base, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let sep = if query.is_empty() { "" } else { "?" };
    let scratch_url = format!("{}/{name}{sep}{query}", &base[..base.rfind('/').unwrap()]);
    Scratch {
        admin,
        name,
        url: scratch_url,
        pool,
        ledger,
    }
}

impl Scratch {
    pub async fn drop_db(self) {
        self.pool.close().await;
        sqlx::query(&format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        ))
        .execute(&self.admin)
        .await
        .unwrap();
    }

    /// Opens a settlement account and two wallets, funds one, and posts `n`
    /// transfers between them.
    pub async fn post_transfers(&self, n: usize) {
        let (settlement, alice, bob) = (AccountId::new(), AccountId::new(), AccountId::new());
        self.ledger
            .open_account(&Account::new(
                settlement,
                AccountType::SystemSettlement,
                tjs(),
            ))
            .await
            .unwrap();
        for id in [alice, bob] {
            self.ledger
                .open_account(&Account::new(id, AccountType::UserWallet, tjs()))
                .await
                .unwrap();
        }
        let transfer = |from, to, minor| {
            Transaction::with_entries(vec![
                Entry::debit(from, Money::from_minor(minor, tjs())),
                Entry::credit(to, Money::from_minor(minor, tjs())),
            ])
        };
        self.ledger
            .post(&transfer(settlement, alice, 1_000_000))
            .await
            .unwrap();
        for i in 0..n {
            self.ledger
                .post(&transfer(alice, bob, 100 + i as i128))
                .await
                .unwrap();
        }
    }

    pub async fn latest(&self) -> VerifyState {
        let row = sqlx::query(
            "SELECT seq, checkpoint_hash, to_txn_seq FROM checkpoints ORDER BY seq DESC LIMIT 1",
        )
        .fetch_one(&self.pool)
        .await
        .unwrap();
        VerifyState {
            last_checkpoint_seq: row.get("seq"),
            last_checkpoint_hash: row.get::<Vec<u8>, _>("checkpoint_hash").try_into().unwrap(),
            last_to_txn_seq: row.get("to_txn_seq"),
        }
    }

    /// What a database superuser (or a stolen owner credential) can do: run
    /// statements with triggers — append-only rules, foreign keys — skipped.
    pub async fn as_superuser(&self, statements: &[&str]) {
        let mut tx = self.pool.begin().await.unwrap();
        sqlx::query("SET LOCAL session_replication_role = replica")
            .execute(&mut *tx)
            .await
            .unwrap();
        for sql in statements {
            sqlx::query(sql).execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
    }
}
