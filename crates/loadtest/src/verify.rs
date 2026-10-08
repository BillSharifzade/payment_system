// After every run the database must agree with physics (conservation, balances = entries) and
// with what the client was told: every acknowledged money move exists exactly as asked, no
// refused one exists, and nothing else touched the run's wallets.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::stats::{Posted, Recorder};
use crate::Funding;

const CHUNK: usize = 10_000;

#[derive(Serialize, Default)]
pub struct Verification {
    /// Currencies whose balances do not sum to zero (whole database).
    pub unbalanced_currencies: Vec<String>,
    /// Accounts whose materialised balance differs from the sum of their entries (whole database).
    pub balance_mismatches: i64,
    pub acknowledged: usize,
    /// Acknowledged but absent from the ledger.
    pub lost: usize,
    /// Present, but their wallet legs differ from what was acknowledged.
    pub wrong_legs: usize,
    /// Refused (4xx / rolled back) yet present in the ledger.
    pub rejected_but_posted: usize,
    pub unresolved: usize,
    pub unresolved_posted: usize,
    /// Transactions on the run's wallets that the client never asked for.
    pub unexpected_transactions: usize,
    /// Run wallets whose balance differs from funding + every posted leg the client knows of.
    pub wallet_drift: usize,
    pub passed: bool,
}

async fn legs_of(
    pool: &PgPool,
    ids: &[Uuid],
    wallets: &HashSet<Uuid>,
) -> Result<HashMap<Uuid, Vec<(Uuid, i64)>>, String> {
    let mut out: HashMap<Uuid, Vec<(Uuid, i64)>> = HashMap::with_capacity(ids.len());
    for chunk in ids.chunks(CHUNK) {
        let rows = sqlx::query(
            "SELECT transaction_id, account_id,
                    CASE direction WHEN 'credit' THEN amount_minor ELSE -amount_minor END AS signed
             FROM entries WHERE transaction_id = ANY($1)",
        )
        .bind(chunk)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("read entries: {e}"))?;
        for r in rows {
            let txn: Uuid = r.get("transaction_id");
            let account: Uuid = r.get("account_id");
            let entry = out.entry(txn).or_default();
            if wallets.contains(&account) {
                entry.push((account, r.get("signed")));
            }
        }
    }
    Ok(out)
}

fn same_legs(mut got: Vec<(Uuid, i64)>, want: &[(Uuid, i64)]) -> bool {
    let mut want = want.to_vec();
    got.sort_unstable();
    want.sort_unstable();
    got == want
}

pub async fn verify(
    pool: &PgPool,
    wallets: &[Uuid],
    funding: &[Funding],
    rec: &Recorder,
) -> Result<Verification, String> {
    let db = |e: sqlx::Error| format!("verify: {e}");
    let mut v = Verification {
        acknowledged: rec.posted.len(),
        unresolved: rec.unresolved.len(),
        ..Verification::default()
    };

    v.unbalanced_currencies = sqlx::query_scalar(
        "SELECT a.currency FROM balances b JOIN accounts a ON a.id = b.account_id
         GROUP BY a.currency HAVING SUM(b.raw_minor) <> 0 ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .map_err(db)?;
    v.balance_mismatches = sqlx::query_scalar(
        "SELECT count(*) FROM balances b
         LEFT JOIN (SELECT account_id,
                           SUM(CASE direction WHEN 'credit' THEN amount_minor ELSE -amount_minor END) AS s
                    FROM entries GROUP BY account_id) e ON e.account_id = b.account_id
         WHERE b.raw_minor <> COALESCE(e.s, 0)",
    )
    .fetch_one(pool)
    .await
    .map_err(db)?;

    let wallet_set: HashSet<Uuid> = wallets.iter().copied().collect();
    let ids: Vec<Uuid> = rec.posted.iter().map(|p| p.id).collect();
    let found = legs_of(pool, &ids, &wallet_set).await?;
    for p in &rec.posted {
        match found.get(&p.id) {
            None => v.lost += 1,
            Some(legs) if !same_legs(legs.clone(), &p.legs) => v.wrong_legs += 1,
            Some(_) => {}
        }
    }

    for chunk in rec.rejected.chunks(CHUNK) {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM transactions t
             WHERE t.id = ANY($1) AND EXISTS (SELECT 1 FROM entries e WHERE e.transaction_id = t.id)",
        )
        .bind(chunk)
        .fetch_one(pool)
        .await
        .map_err(db)?;
        v.rejected_but_posted += n as usize;
    }

    let unresolved_ids: Vec<Uuid> = rec.unresolved.iter().map(|p| p.id).collect();
    let unresolved_found = legs_of(pool, &unresolved_ids, &wallet_set).await?;
    let mut effective: Vec<&Posted> = rec.posted.iter().collect();
    for p in &rec.unresolved {
        match unresolved_found.get(&p.id) {
            Some(legs) if same_legs(legs.clone(), &p.legs) => {
                v.unresolved_posted += 1;
                effective.push(p);
            }
            Some(_) => v.wrong_legs += 1,
            None => {}
        }
    }

    let mut known: HashSet<Uuid> = effective.iter().map(|p| p.id).collect();
    known.extend(funding.iter().map(|f| f.id));
    let mut expected: HashMap<Uuid, i64> = wallets.iter().map(|w| (*w, 0)).collect();
    for f in funding {
        *expected.entry(f.wallet).or_default() += f.amount;
    }
    for p in &effective {
        for (w, d) in &p.legs {
            *expected.entry(*w).or_default() += d;
        }
    }
    for chunk in wallets.chunks(CHUNK) {
        let touching: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT transaction_id FROM entries WHERE account_id = ANY($1)",
        )
        .bind(chunk)
        .fetch_all(pool)
        .await
        .map_err(db)?;
        v.unexpected_transactions += touching.iter().filter(|t| !known.contains(t)).count();
        let rows =
            sqlx::query("SELECT account_id, raw_minor FROM balances WHERE account_id = ANY($1)")
                .bind(chunk)
                .fetch_all(pool)
                .await
                .map_err(db)?;
        v.wallet_drift += chunk.len() - rows.len();
        for r in rows {
            let id: Uuid = r.get("account_id");
            if expected.get(&id).copied() != Some(r.get::<i64, _>("raw_minor")) {
                v.wallet_drift += 1;
            }
        }
    }

    v.passed = v.unbalanced_currencies.is_empty()
        && v.balance_mismatches == 0
        && v.lost == 0
        && v.wrong_legs == 0
        && v.rejected_but_posted == 0
        && v.unexpected_transactions == 0
        && v.wallet_drift == 0;
    Ok(v)
}
