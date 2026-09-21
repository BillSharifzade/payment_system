use crate::error::Result;
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceMismatch {
    pub account_id: Uuid,
    pub stored_minor: i64,
    pub derived_minor: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationReport {
    pub currency_imbalances: Vec<(String, i64)>,
    pub balance_mismatches: Vec<BalanceMismatch>,
    pub accounts_checked: i64,
}

impl ReconciliationReport {
    pub fn is_healthy(&self) -> bool {
        self.currency_imbalances.is_empty() && self.balance_mismatches.is_empty()
    }
}

pub async fn reconcile(pool: &PgPool) -> Result<ReconciliationReport> {
    reconcile_since(pool, None).await
}

pub async fn reconcile_since(
    pool: &PgPool,
    since_epoch_secs: Option<f64>,
) -> Result<ReconciliationReport> {
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

    let mismatch_rows = sqlx::query(
        "SELECT b.account_id AS account_id,
                b.raw_minor AS stored,
                COALESCE((SELECT SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                                       ELSE -e.amount_minor END)
                          FROM entries e WHERE e.account_id = b.account_id), 0)::BIGINT AS derived
         FROM balances b
         WHERE $1::FLOAT8 IS NULL OR b.updated_at >= to_timestamp($1::FLOAT8)",
    )
    .bind(since_epoch_secs)
    .fetch_all(pool)
    .await?;

    let accounts_checked = mismatch_rows.len() as i64;
    let mut balance_mismatches = Vec::new();
    for row in mismatch_rows {
        let stored_minor: i64 = row.try_get("stored")?;
        let derived_minor: i64 = row.try_get("derived")?;
        if stored_minor != derived_minor {
            balance_mismatches.push(BalanceMismatch {
                account_id: row.try_get("account_id")?,
                stored_minor,
                derived_minor,
            });
        }
    }

    Ok(ReconciliationReport {
        currency_imbalances,
        balance_mismatches,
        accounts_checked,
    })
}

pub async fn unsealed_lag(pool: &PgPool) -> Result<(i64, f64)> {
    let row = sqlx::query(
        "SELECT COUNT(*)::BIGINT AS unsealed,
                COALESCE(EXTRACT(EPOCH FROM (now() - MIN(created_at))), 0)::FLOAT8 AS oldest_age
         FROM transactions WHERE sealed_seq IS NULL",
    )
    .fetch_one(pool)
    .await?;
    Ok((row.try_get("unsealed")?, row.try_get("oldest_age")?))
}
