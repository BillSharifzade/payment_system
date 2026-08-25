//! Continuous reconciliation — the early-warning system for correctness bugs
//! (DESIGN.md §13).
//!
//! Two independent checks, both of which must always hold:
//!  1. **Conservation:** the raw balances of all accounts sum to zero in every
//!     currency. Money is neither created nor destroyed.
//!  2. **Balance integrity:** each account's materialised balance equals the
//!     balance re-derived from scratch by summing its entries.
//!
//! A failure here means a bug (or tampering) has corrupted state, and should
//! page a human immediately.

use crate::error::Result;
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// An account whose stored balance disagrees with the sum of its entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceMismatch {
    pub account_id: Uuid,
    pub stored_minor: i64,
    pub derived_minor: i64,
}

/// The outcome of a reconciliation pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationReport {
    /// Currencies whose balances do not sum to zero, with the non-zero total.
    pub currency_imbalances: Vec<(String, i64)>,
    /// Accounts whose stored balance ≠ derived balance.
    pub balance_mismatches: Vec<BalanceMismatch>,
}

impl ReconciliationReport {
    /// True if the ledger is fully consistent (the normal, healthy case).
    pub fn is_healthy(&self) -> bool {
        self.currency_imbalances.is_empty() && self.balance_mismatches.is_empty()
    }
}

/// Run a full reconciliation pass.
pub async fn reconcile(pool: &PgPool) -> Result<ReconciliationReport> {
    // 1. Conservation per currency: SUM(raw_minor) must be 0.
    // SUM over BIGINT yields NUMERIC, which does not decode as i64 — cast back
    // to BIGINT so the reported imbalance is the real number, not a decode
    // fallback. Balances fit i64 by construction.
    let imbalance_rows = sqlx::query(
        "SELECT a.currency AS currency, SUM(b.raw_minor)::BIGINT AS total
         FROM balances b
         JOIN accounts a ON a.id = b.account_id
         GROUP BY a.currency
         HAVING SUM(b.raw_minor) <> 0",
    )
    .fetch_all(pool)
    .await?;

    let mut currency_imbalances = Vec::new();
    for row in imbalance_rows {
        let currency: String = row.try_get("currency")?;
        let total: i64 = row.try_get("total")?;
        currency_imbalances.push((currency, total));
    }

    // 2. Balance integrity: stored balance vs the sum re-derived from entries.
    let mismatch_rows = sqlx::query(
        "SELECT b.account_id AS account_id,
                b.raw_minor AS stored,
                COALESCE(d.derived, 0)::BIGINT AS derived
         FROM balances b
         LEFT JOIN (
             SELECT account_id,
                    SUM(CASE direction WHEN 'credit' THEN amount_minor ELSE -amount_minor END) AS derived
             FROM entries
             GROUP BY account_id
         ) d ON d.account_id = b.account_id
         WHERE b.raw_minor <> COALESCE(d.derived, 0)",
    )
    .fetch_all(pool)
    .await?;

    let mut balance_mismatches = Vec::new();
    for row in mismatch_rows {
        let account_id: Uuid = row.try_get("account_id")?;
        let stored_minor: i64 = row.try_get("stored")?;
        let derived_minor: i64 = row.try_get("derived")?;
        balance_mismatches.push(BalanceMismatch {
            account_id,
            stored_minor,
            derived_minor,
        });
    }

    Ok(ReconciliationReport {
        currency_imbalances,
        balance_mismatches,
    })
}
