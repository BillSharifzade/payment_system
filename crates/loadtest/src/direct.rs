// In process: the same Transaction (api::payment_entries) and the same pre-write AML guard
// (api::aml_guard) as POST /v1/transfers, posted with PostgresLedger::post_on on a pooled
// connection. What this leaves out — HTTP, JSON, JWT, the context query, the screening
// pre-checks — is exactly the difference between the two modes.

use std::time::{Duration, Instant};

use api::{aml_guard, payment_entries, settlement_shard, FeeConfig, Limits, ScreenCtx};
use ledger::{AccountId, Entry, LedgerError, Transaction, TransactionId};
use money::{Currency, Money};
use sqlx::PgPool;
use storage::{IdempotencyRecord, PostOptions, PostgresLedger, StorageError};
use uuid::Uuid;

use crate::args::{Config, Workload};
use crate::plan::{Planned, Rng};
use crate::stats::{Event, Op, Outcome, Posted, Step};
use crate::{par, Funding};

// Limits the guard enforces (it still sums the window): high enough never to reject.
const NO_LIMIT: Limits = Limits {
    per_tx_minor: i64::MAX / 4,
    daily_minor: i64::MAX / 4,
    velocity_per_hour: i64::MAX / 4,
};

pub struct User {
    id: Uuid,
    wallet: Uuid,
}

pub struct Direct {
    ledger: PostgresLedger,
    users: Vec<User>,
    fees: FeeConfig,
    tjs: Currency,
    retries: u32,
}

enum Failure {
    /// Rolled back for certain (a ledger rule, a guard, a lock/statement timeout, deadlock).
    Definite(String, bool),
    /// The connection failed mid-flight: the commit may or may not have happened.
    Unknown(String),
}

fn classify(e: &StorageError) -> Failure {
    match e {
        StorageError::Ledger(LedgerError::InsufficientFunds { .. }) => {
            Failure::Definite("insufficient_funds".into(), false)
        }
        StorageError::Ledger(l) => Failure::Definite(format!("ledger: {l}"), false),
        StorageError::Rejected { rule, .. } => Failure::Definite(format!("rejected {rule}"), false),
        StorageError::Database(sqlx::Error::PoolTimedOut) => {
            Failure::Definite("pool_timeout".into(), true)
        }
        StorageError::Database(sqlx::Error::Database(db)) => {
            let code = db.code().map(|c| c.to_string()).unwrap_or_default();
            let transient = matches!(code.as_str(), "55P03" | "57014" | "40001" | "40P01");
            Failure::Definite(format!("sqlstate {code}"), transient)
        }
        other => Failure::Unknown(other.to_string()),
    }
}

impl Direct {
    pub fn wallets(&self) -> Vec<Uuid> {
        self.users.iter().map(|u| u.wallet).collect()
    }

    async fn transfer(&self, from: &User, to: &User, amount_minor: i64) -> Outcome {
        let key = Uuid::new_v4();
        let amount = Money::from_minor(amount_minor as i128, self.tjs);
        let posted = Posted {
            id: key,
            legs: vec![
                (from.wallet, -amount_minor),
                (to.wallet, amount_minor - self.fees.fee_minor(amount_minor)),
            ],
        };
        let mut attempt = 0;
        loop {
            let entries = match payment_entries(self.fees, from.wallet, to.wallet, amount) {
                Ok(e) => e,
                Err(e) => {
                    return Outcome {
                        result: Err(e.to_string()),
                        retries: attempt,
                        event: Some(Event::Rejected(key)),
                    }
                }
            };
            let screen = ScreenCtx {
                user_id: from.id,
                from_account: from.wallet,
                to_account: to.wallet,
                amount_minor,
                currency: "TJS".into(),
            };
            let opts = PostOptions {
                idempotency: Some(IdempotencyRecord {
                    key,
                    fingerprint: format!(
                        "transfer:{}:{}:{}:{amount_minor}:TJS",
                        from.id, from.wallet, to.wallet
                    ),
                    response_status: 201,
                    response_body: serde_json::json!({"transaction_id": key, "status": "posted"}),
                }),
                guard: Some(aml_guard(screen, (1, 1), NO_LIMIT)),
            };
            let txn = Transaction::new(TransactionId(key), entries);
            let result = async {
                let mut conn = self.ledger.pool().acquire().await?;
                self.ledger.post_on(&mut conn, &txn, opts).await
            }
            .await;
            let (result, event) = match result {
                Ok(()) => (Ok(()), Event::Posted(posted)),
                // Only a retry can meet its own key: the earlier attempt did commit.
                Err(StorageError::Ledger(LedgerError::DuplicateTransaction(_))) if attempt > 0 => {
                    (Ok(()), Event::Posted(posted))
                }
                Err(e) => match classify(&e) {
                    Failure::Definite(_, true) if attempt < self.retries => {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_millis(10 * attempt as u64)).await;
                        continue;
                    }
                    Failure::Definite(code, _) => (Err(code), Event::Rejected(key)),
                    Failure::Unknown(code) => (Err(code), Event::Unresolved(posted)),
                },
            };
            return Outcome {
                result,
                retries: attempt,
                event: Some(event),
            };
        }
    }

    pub async fn exec(&self, planned: Planned, begin: Instant) -> Vec<Step> {
        let (op, outcome) = match planned {
            Planned::Transfer { from, to, amount } => (
                Op::Transfer,
                self.transfer(&self.users[from], &self.users[to], amount)
                    .await,
            ),
            Planned::Balance { user } => (
                Op::Balance,
                match self
                    .ledger
                    .balance_with_owner(AccountId(self.users[user].wallet))
                    .await
                {
                    Ok(_) => Outcome::ok(),
                    Err(e) => Outcome {
                        result: Err(e.to_string()),
                        ..Outcome::ok()
                    },
                },
            ),
            other => unreachable!("validated as http-only: {other:?}"),
        };
        vec![Step {
            op,
            started: begin,
            finished: Instant::now(),
            outcome,
        }]
    }
}

pub async fn setup(
    cfg: &Config,
    pool: PgPool,
    rng: &mut Rng,
) -> Result<(Direct, Vec<Funding>), String> {
    let ledger = PostgresLedger::new(pool.clone());
    ledger
        .migrate()
        .await
        .map_err(|e| format!("migrate: {e}"))?;
    let tjs = ledger
        .lookup_currency("TJS")
        .await
        .map_err(|e| format!("TJS: {e}"))?;
    let tag = rng.below(100_000);
    let users: Vec<User> = (0..cfg.users)
        .map(|_| User {
            id: Uuid::new_v4(),
            wallet: Uuid::new_v4(),
        })
        .collect();
    eprintln!("creating {} users and wallets...", cfg.users);
    for (n, chunk) in users.chunks(1000).enumerate() {
        let ids: Vec<Uuid> = chunk.iter().map(|u| u.id).collect();
        let wallets: Vec<Uuid> = chunk.iter().map(|u| u.wallet).collect();
        let phones: Vec<String> = (0..chunk.len())
            .map(|i| format!("8{tag:05}{:07}", n * 1000 + i))
            .collect();
        sqlx::query(
            "WITH u AS (
                 INSERT INTO users (id, phone, password_hash, kyc_level)
                 SELECT id, phone, 'loadtest: no password', 2
                 FROM UNNEST($1::uuid[], $2::text[]) AS t(id, phone)
             ), a AS (
                 INSERT INTO accounts (id, account_type, currency, owner_user_id)
                 SELECT w, 'user_wallet', 'TJS', u FROM UNNEST($3::uuid[], $1::uuid[]) AS t(w, u)
             )
             INSERT INTO balances (account_id, raw_minor, version, min_raw)
             SELECT w, 0, 0, 0 FROM UNNEST($3::uuid[]) AS t(w)",
        )
        .bind(&ids)
        .bind(&phones)
        .bind(&wallets)
        .execute(&pool)
        .await
        .map_err(|e| format!("create users: {e}"))?;
    }

    let mut deposits = Vec::with_capacity(users.len());
    for (i, u) in users.iter().enumerate() {
        let n = if cfg.workload == Workload::Contention && i == 0 {
            50
        } else {
            1
        };
        deposits.extend(std::iter::repeat_n(u.wallet, n));
    }
    eprintln!("funding {} deposits...", deposits.len());
    let (l, fund) = (ledger.clone(), cfg.fund);
    let funding = par(cfg.pool_size as usize, deposits, move |wallet| {
        let ledger = l.clone();
        async move {
            let id = Uuid::new_v4();
            let amount = Money::from_minor(fund as i128, tjs);
            let settlement = settlement_shard("TJS").expect("TJS settles");
            let txn = Transaction::new(
                TransactionId(id),
                vec![
                    Entry::debit(AccountId(settlement), amount),
                    Entry::credit(AccountId(wallet), amount),
                ],
            );
            let mut conn = ledger
                .pool()
                .acquire()
                .await
                .map_err(|e| format!("deposit: {e}"))?;
            ledger
                .post_on(&mut conn, &txn, PostOptions::default())
                .await
                .map_err(|e| format!("deposit: {e}"))?;
            Ok(Funding {
                id,
                wallet,
                amount: fund,
            })
        }
    })
    .await?;
    Ok((
        Direct {
            ledger,
            users,
            fees: FeeConfig {
                transfer_bps: cfg.fee_bps,
            },
            tjs,
            retries: cfg.retries,
        },
        funding,
    ))
}
