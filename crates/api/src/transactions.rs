use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::common::{db_err, rfc3339};
use crate::session::AuthUser;
use crate::{ApiError, ApiResult, AppState};

#[derive(Serialize)]
pub struct TransactionEntry {
    account_id: Uuid,
    direction: String,
    amount_minor: i64,
    currency: String,
}

#[derive(Serialize)]
pub struct TransactionResponse {
    transaction_id: Uuid,
    status: &'static str,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entries: Option<Vec<TransactionEntry>>,
}

struct EntryRow {
    account_id: Uuid,
    direction: String,
    amount_minor: i64,
    currency: String,
    account_type: String,
    owner: Option<Uuid>,
}

// Same classification as the statement endpoint: the counterparty of one of the caller's
// entries is the other entry preferring the opposite direction, then a user wallet, then the
// same currency.
fn kind_of(mine: &EntryRow, all: &[EntryRow], caller: Uuid) -> &'static str {
    let counterparty = all
        .iter()
        .filter(|e| e.account_id != mine.account_id)
        .max_by_key(|e| {
            (
                e.direction != mine.direction,
                e.account_type == "user_wallet",
                e.currency == mine.currency,
            )
        });
    match counterparty.map(|e| (e.account_type.as_str(), e.owner)) {
        Some(("user_wallet", Some(owner))) if owner == caller => "fx",
        Some(("user_wallet", _)) => "transfer",
        Some(("system_settlement", _)) if mine.direction == "credit" => "deposit",
        Some(("system_settlement", _)) => "withdrawal",
        Some(("system_fx_gain_loss", _)) => "fx",
        Some(("system_fee_revenue", _)) => "fee",
        _ => "other",
    }
}

async fn lookup(conn: &mut PgConnection, id: Uuid, caller: Uuid) -> ApiResult<TransactionResponse> {
    let rows = sqlx::query(&format!(
        "SELECT {} AS created_at, v.voided_by,
                e.account_id, e.direction, e.amount_minor, e.currency,
                a.account_type, a.owner_user_id
         FROM transactions t
         LEFT JOIN voided_transactions v ON v.id = t.id
         LEFT JOIN entries e ON e.transaction_id = t.id
         LEFT JOIN accounts a ON a.id = e.account_id
         WHERE t.id = $1
         ORDER BY e.id",
        rfc3339("t.created_at")
    ))
    .bind(id)
    .fetch_all(&mut *conn)
    .await
    .map_err(db_err)?;
    let not_found = || ApiError::NotFound("transaction not found".to_string());
    let first = rows.first().ok_or_else(not_found)?;
    let created_at: String = first.try_get("created_at").map_err(db_err)?;
    let voided_by: Option<Uuid> = first.try_get("voided_by").map_err(db_err)?;
    if let Some(by) = voided_by {
        if by != caller {
            return Err(not_found());
        }
        return Ok(TransactionResponse {
            transaction_id: id,
            status: "voided",
            created_at,
            kind: None,
            entries: None,
        });
    }
    let mut all = Vec::with_capacity(rows.len());
    for row in &rows {
        let account_id: Option<Uuid> = row.try_get("account_id").map_err(db_err)?;
        let Some(account_id) = account_id else {
            continue;
        };
        all.push(EntryRow {
            account_id,
            direction: row.try_get("direction").map_err(db_err)?,
            amount_minor: row.try_get("amount_minor").map_err(db_err)?,
            currency: row.try_get("currency").map_err(db_err)?,
            account_type: row.try_get("account_type").map_err(db_err)?,
            owner: row.try_get("owner_user_id").map_err(db_err)?,
        });
    }
    let mine: Vec<&EntryRow> = all.iter().filter(|e| e.owner == Some(caller)).collect();
    let first_mine = mine.first().ok_or_else(not_found)?;
    let kind = kind_of(first_mine, &all, caller);
    let entries = mine
        .iter()
        .map(|e| TransactionEntry {
            account_id: e.account_id,
            direction: e.direction.clone(),
            amount_minor: e.amount_minor,
            currency: e.currency.clone(),
        })
        .collect();
    Ok(TransactionResponse {
        transaction_id: id,
        status: "posted",
        created_at,
        kind: Some(kind),
        entries: Some(entries),
    })
}

pub async fn get_transaction(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<TransactionResponse>> {
    let mut conn = state.ledger.pool().acquire().await.map_err(db_err)?;
    Ok(Json(lookup(&mut conn, id, user_id).await?))
}

// Claiming the id in `transactions` (no entries) is what makes a void final: post_on claims
// the same primary key first, so whichever commits first wins and the loser sees a duplicate.
// A deposit request's id is reserved for its approval; withdrawing one is a reject, not a void.
pub async fn void_transaction(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<TransactionResponse>> {
    let mut conn = state.ledger.pool().acquire().await.map_err(db_err)?;
    let claimed: Option<String> = sqlx::query_scalar(&format!(
        "WITH claim AS (
             INSERT INTO transactions (id)
             SELECT $1 WHERE NOT EXISTS (SELECT 1 FROM deposit_requests WHERE id = $1)
             ON CONFLICT (id) DO NOTHING
             RETURNING id, created_at
         ), v AS (
             INSERT INTO voided_transactions (id, voided_by, created_at)
             SELECT id, $2, created_at FROM claim
             RETURNING created_at
         )
         SELECT {} FROM v",
        rfc3339("v.created_at")
    ))
    .bind(id)
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_err)?;
    if let Some(created_at) = claimed {
        metrics::counter!("transactions_voided_total").increment(1);
        tracing::info!(%user_id, transaction_id = %id, "idempotency key voided");
        return Ok(Json(TransactionResponse {
            transaction_id: id,
            status: "voided",
            created_at,
            kind: None,
            entries: None,
        }));
    }
    Ok(Json(lookup(&mut conn, id, user_id).await?))
}
