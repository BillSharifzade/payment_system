use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use ledger::{AccountId, AccountType, Entry, LedgerError, Transaction, TransactionId};
use money::Money;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use storage::{HookError, PostHook, PostOptions, StorageError};
use uuid::Uuid;

use crate::common::{
    cursor_db_error, db_err, default_currency, idempotency_key, parse_cursor, rfc3339,
};
use crate::payments::{record, settlement_shard, KeyState};
use crate::session::AdminUser;
use crate::{ApiError, ApiResult, AppState};

const MAX_REASON_CHARS: usize = 500;

#[derive(Deserialize)]
pub struct DepositRequest {
    user_account: Uuid,
    amount_minor: i64,
    #[serde(default = "default_currency")]
    currency: String,
}

#[derive(Serialize)]
pub struct Deposit {
    id: Uuid,
    transaction_id: Uuid,
    status: String,
    user_account: Uuid,
    amount_minor: i64,
    currency: String,
    customer_phone: Option<String>,
    requested_by: Uuid,
    requested_at: String,
    decided_by: Option<Uuid>,
    decided_at: Option<String>,
    reason: Option<String>,
    #[serde(skip)]
    owner: Option<Uuid>,
    #[serde(skip)]
    cursor: String,
}

impl Deposit {
    // A deposit posted by the admin who requested it can only come from the single-admin
    // (dual control off) path, so a replay can answer with the status it was created with.
    fn replay_status(&self) -> StatusCode {
        if self.status == "posted" && self.decided_by == Some(self.requested_by) {
            StatusCode::CREATED
        } else {
            StatusCode::ACCEPTED
        }
    }
}

fn deposit_select() -> String {
    format!(
        "SELECT d.id, d.status, d.user_account, d.amount_minor, d.currency, u.phone AS customer_phone,
                a.owner_user_id AS owner, d.requested_by, {} AS requested_at, d.decided_by,
                {} AS decided_at, d.reason, d.requested_at::text AS ts
         FROM deposit_requests d
         JOIN accounts a ON a.id = d.user_account
         LEFT JOIN users u ON u.id = a.owner_user_id",
        rfc3339("d.requested_at"),
        rfc3339("d.decided_at"),
    )
}

fn deposit_row(row: &PgRow) -> ApiResult<Deposit> {
    let id: Uuid = row.try_get("id").map_err(db_err)?;
    let ts: String = row.try_get("ts").map_err(db_err)?;
    Ok(Deposit {
        id,
        transaction_id: id,
        status: row.try_get("status").map_err(db_err)?,
        user_account: row.try_get("user_account").map_err(db_err)?,
        amount_minor: row.try_get("amount_minor").map_err(db_err)?,
        currency: row.try_get("currency").map_err(db_err)?,
        customer_phone: row.try_get("customer_phone").map_err(db_err)?,
        requested_by: row.try_get("requested_by").map_err(db_err)?,
        requested_at: row.try_get("requested_at").map_err(db_err)?,
        decided_by: row.try_get("decided_by").map_err(db_err)?,
        decided_at: row.try_get("decided_at").map_err(db_err)?,
        reason: row.try_get("reason").map_err(db_err)?,
        owner: row.try_get("owner").map_err(db_err)?,
        cursor: format!("{ts}|{id}"),
    })
}

async fn load_deposit(conn: &mut PgConnection, id: Uuid) -> ApiResult<Option<Deposit>> {
    let row = sqlx::query(&format!("{} WHERE d.id = $1", deposit_select()))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_err)?;
    row.as_ref().map(deposit_row).transpose()
}

async fn reload(conn: &mut PgConnection, id: Uuid) -> ApiResult<Deposit> {
    load_deposit(conn, id)
        .await?
        .ok_or_else(|| StorageError::DataIntegrity(format!("deposit {id} vanished")).into())
}

fn replay(
    existing: Deposit,
    admin_id: Uuid,
    req: &DepositRequest,
) -> ApiResult<(StatusCode, Json<Deposit>)> {
    if existing.requested_by != admin_id
        || existing.user_account != req.user_account
        || existing.amount_minor != req.amount_minor
        || existing.currency != req.currency
    {
        return Err(ApiError::IdempotencyConflict);
    }
    Ok((existing.replay_status(), Json(existing)))
}

async fn deposit_txn(
    state: &AppState,
    id: Uuid,
    account: Uuid,
    amount_minor: i64,
    currency: &str,
) -> ApiResult<Transaction> {
    let settlement = settlement_shard(currency)
        .ok_or_else(|| ApiError::BadRequest(format!("deposits are not supported in {currency}")))?;
    let amount = Money::from_minor(
        amount_minor as i128,
        state.ledger.lookup_currency(currency).await?,
    );
    Ok(Transaction::new(
        TransactionId(id),
        vec![
            Entry::debit(AccountId(settlement), amount),
            Entry::credit(AccountId(account), amount),
        ],
    ))
}

fn deposit_record(id: Uuid) -> ApiResult<storage::IdempotencyRecord> {
    record(
        id,
        &format!("deposit:{id}"),
        &serde_json::json!({ "deposit_id": id }),
    )
}

pub async fn create_deposit(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DepositRequest>,
) -> ApiResult<(StatusCode, Json<Deposit>)> {
    if req.amount_minor <= 0 {
        return Err(ApiError::BadRequest(
            "amount_minor must be positive".to_string(),
        ));
    }
    let key = idempotency_key(&headers)?;
    if settlement_shard(&req.currency).is_none() {
        return Err(ApiError::BadRequest(format!(
            "deposits are not supported in {}",
            req.currency
        )));
    }
    if req.amount_minor > state.deposits.max_minor {
        return Err(ApiError::LimitExceeded(format!(
            "deposits are limited to {} minor units",
            state.deposits.max_minor
        )));
    }

    let mut conn = state.ledger.pool().acquire().await.map_err(db_err)?;
    let ctx = sqlx::query(
        "SELECT a.account_type, a.owner_user_id, a.currency,
                (d.id IS NOT NULL) AS existing, (vt.id IS NOT NULL) AS voided,
                (t.id IS NOT NULL) AS used
         FROM (SELECT $1::uuid AS key) k
         LEFT JOIN accounts a ON a.id = $2
         LEFT JOIN deposit_requests d ON d.id = k.key
         LEFT JOIN voided_transactions vt ON vt.id = k.key
         LEFT JOIN transactions t ON t.id = k.key",
    )
    .bind(key)
    .bind(req.user_account)
    .fetch_one(&mut *conn)
    .await
    .map_err(db_err)?;
    if ctx.try_get::<bool, _>("existing").map_err(db_err)? {
        return replay(reload(&mut conn, key).await?, admin_id, &req);
    }
    if ctx.try_get::<bool, _>("voided").map_err(db_err)? {
        return Err(ApiError::Voided);
    }
    if ctx.try_get::<bool, _>("used").map_err(db_err)? {
        return Err(ApiError::IdempotencyConflict);
    }
    let account_type: Option<String> = ctx.try_get("account_type").map_err(db_err)?;
    let Some(account_type) = account_type else {
        return Err(ApiError::NotFound("account not found".to_string()));
    };
    if AccountType::from_db_str(&account_type) != Some(AccountType::UserWallet) {
        return Err(ApiError::BadRequest(
            "deposits must credit a user wallet".to_string(),
        ));
    }
    let wallet_currency: String = ctx.try_get("currency").map_err(db_err)?;
    if wallet_currency != req.currency {
        return Err(ApiError::BadRequest(format!(
            "wallet holds {wallet_currency}, not {}",
            req.currency
        )));
    }
    let owner: Option<Uuid> = ctx.try_get("owner_user_id").map_err(db_err)?;
    if owner == Some(admin_id) {
        return Err(ApiError::Forbidden(
            "admins cannot fund their own wallets".to_string(),
        ));
    }

    if state.deposits.dual_control {
        let row = sqlx::query(
            "WITH ins AS (
                 INSERT INTO deposit_requests (id, user_account, amount_minor, currency, requested_by)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (id) DO NOTHING
                 RETURNING id, user_account, amount_minor, currency
             )
             INSERT INTO admin_actions (id, admin_id, action, target, details)
             SELECT $6, $5, 'deposit.request', ins.user_account::text,
                    jsonb_build_object('deposit_id', ins.id, 'amount_minor', ins.amount_minor,
                                       'currency', ins.currency)
             FROM ins
             RETURNING id",
        )
        .bind(key)
        .bind(req.user_account)
        .bind(req.amount_minor)
        .bind(&req.currency)
        .bind(admin_id)
        .bind(Uuid::now_v7())
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_err)?;
        let deposit = reload(&mut conn, key).await?;
        if row.is_none() {
            return replay(deposit, admin_id, &req);
        }
        metrics::counter!("deposits_total", "outcome" => "requested").increment(1);
        tracing::info!(%admin_id, deposit_id = %key, user_account = %req.user_account, amount_minor = req.amount_minor, "deposit requested; awaiting a second admin");
        return Ok((StatusCode::ACCEPTED, Json(deposit)));
    }

    let txn = deposit_txn(
        &state,
        key,
        req.user_account,
        req.amount_minor,
        &req.currency,
    )
    .await?;
    let (account, amount_minor, code) = (req.user_account, req.amount_minor, req.currency.clone());
    let guard: PostHook = Box::new(move |conn: &mut PgConnection| {
        Box::pin(async move {
            sqlx::query(
                "WITH ins AS (
                     INSERT INTO deposit_requests
                       (id, user_account, amount_minor, currency, status, requested_by,
                        decided_by, decided_at)
                     VALUES ($1, $2, $3, $4, 'posted', $5, $5, now())
                 )
                 INSERT INTO admin_actions (id, admin_id, action, target, details)
                 VALUES ($6, $5, 'deposit', $2::text,
                         jsonb_build_object('amount_minor', $3::bigint, 'currency', $4::text,
                                            'transaction_id', $1::uuid))",
            )
            .bind(key)
            .bind(account)
            .bind(amount_minor)
            .bind(&code)
            .bind(admin_id)
            .bind(Uuid::now_v7())
            .execute(&mut *conn)
            .await?;
            Ok(())
        })
    });
    let opts = PostOptions {
        idempotency: Some(deposit_record(key)?),
        guard: Some(guard),
    };
    match state.ledger.post_on(&mut conn, &txn, opts).await {
        Ok(()) => {
            metrics::counter!("deposits_total", "outcome" => "posted").increment(1);
            tracing::info!(%admin_id, deposit_id = %key, "deposit posted without dual control");
            Ok((StatusCode::CREATED, Json(reload(&mut conn, key).await?)))
        }
        Err(StorageError::Ledger(LedgerError::DuplicateTransaction(_))) => {
            match load_deposit(&mut conn, key).await? {
                Some(existing) => replay(existing, admin_id, &req),
                None if KeyState::load(&mut conn, key).await?.voided() => Err(ApiError::Voided),
                None => Err(ApiError::IdempotencyConflict),
            }
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn approve_deposit(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Deposit>> {
    let mut conn = state.ledger.pool().acquire().await.map_err(db_err)?;
    let deposit = load_deposit(&mut conn, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("deposit not found".to_string()))?;
    match deposit.status.as_str() {
        "posted" => return Ok(Json(deposit)),
        "rejected" => return Err(ApiError::Conflict("deposit was rejected".to_string())),
        _ => {}
    }
    if deposit.requested_by == admin_id {
        return Err(ApiError::DualControlRequired(
            "a deposit must be approved by a different admin than the one who requested it"
                .to_string(),
        ));
    }
    if deposit.owner == Some(admin_id) {
        return Err(ApiError::DualControlRequired(
            "admins cannot approve deposits into their own wallets".to_string(),
        ));
    }
    let txn = deposit_txn(
        &state,
        id,
        deposit.user_account,
        deposit.amount_minor,
        &deposit.currency,
    )
    .await?;
    let guard: PostHook = Box::new(move |conn: &mut PgConnection| {
        Box::pin(async move {
            let flipped = sqlx::query(
                "WITH upd AS (
                     UPDATE deposit_requests
                     SET status = 'posted', decided_by = $2, decided_at = now()
                     WHERE id = $1 AND status = 'pending_approval'
                     RETURNING id, user_account, amount_minor, currency, requested_by
                 )
                 INSERT INTO admin_actions (id, admin_id, action, target, details)
                 SELECT $3, $2, 'deposit.approve', upd.id::text,
                        jsonb_build_object('user_account', upd.user_account,
                                           'amount_minor', upd.amount_minor,
                                           'currency', upd.currency,
                                           'requested_by', upd.requested_by)
                 FROM upd
                 RETURNING id",
            )
            .bind(id)
            .bind(admin_id)
            .bind(Uuid::now_v7())
            .fetch_optional(&mut *conn)
            .await?;
            if flipped.is_none() {
                return Err(HookError::Rejected {
                    rule: "deposit_not_pending".to_string(),
                    message: "deposit is no longer pending".to_string(),
                });
            }
            Ok(())
        })
    });
    let opts = PostOptions {
        idempotency: Some(deposit_record(id)?),
        guard: Some(guard),
    };
    match state.ledger.post_on(&mut conn, &txn, opts).await {
        Ok(()) => {
            metrics::counter!("deposits_total", "outcome" => "approved").increment(1);
            tracing::info!(%admin_id, deposit_id = %id, requested_by = %deposit.requested_by, "deposit approved and posted");
        }
        Err(StorageError::Ledger(LedgerError::DuplicateTransaction(_))) => {
            if KeyState::load(&mut conn, id).await?.voided() {
                return Err(ApiError::Voided);
            }
        }
        Err(StorageError::Rejected { .. }) => {}
        Err(e) => return Err(e.into()),
    }
    let after = reload(&mut conn, id).await?;
    if after.status != "posted" {
        return Err(ApiError::Conflict(format!("deposit is {}", after.status)));
    }
    Ok(Json(after))
}

#[derive(Deserialize)]
pub struct RejectDepositRequest {
    reason: String,
}

pub async fn reject_deposit(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<RejectDepositRequest>,
) -> ApiResult<Json<Deposit>> {
    let reason = req.reason.trim();
    if reason.is_empty() || reason.chars().count() > MAX_REASON_CHARS {
        return Err(ApiError::BadRequest(format!(
            "reason must be 1 to {MAX_REASON_CHARS} characters"
        )));
    }
    let mut conn = state.ledger.pool().acquire().await.map_err(db_err)?;
    let rejected = sqlx::query(
        "WITH upd AS (
             UPDATE deposit_requests
             SET status = 'rejected', decided_by = $2, decided_at = now(), reason = $3
             WHERE id = $1 AND status = 'pending_approval'
             RETURNING id, user_account, amount_minor, currency, requested_by
         )
         INSERT INTO admin_actions (id, admin_id, action, target, details)
         SELECT $4, $2, 'deposit.reject', upd.id::text,
                jsonb_build_object('user_account', upd.user_account,
                                   'amount_minor', upd.amount_minor, 'currency', upd.currency,
                                   'requested_by', upd.requested_by, 'reason', $3::text)
         FROM upd
         RETURNING id",
    )
    .bind(id)
    .bind(admin_id)
    .bind(reason)
    .bind(Uuid::now_v7())
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_err)?;
    let deposit = load_deposit(&mut conn, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("deposit not found".to_string()))?;
    if rejected.is_none() {
        return Err(ApiError::Conflict(format!("deposit is {}", deposit.status)));
    }
    metrics::counter!("deposits_total", "outcome" => "rejected").increment(1);
    tracing::info!(%admin_id, deposit_id = %id, "deposit rejected");
    Ok(Json(deposit))
}

pub async fn get_deposit(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Deposit>> {
    let mut conn = state.ledger.pool().acquire().await.map_err(db_err)?;
    load_deposit(&mut conn, id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound("deposit not found".to_string()))
}

#[derive(Deserialize)]
pub struct DepositListParams {
    status: Option<String>,
    cursor: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
pub struct DepositList {
    items: Vec<Deposit>,
    next_cursor: Option<String>,
}

pub async fn list_deposits(
    _admin: AdminUser,
    State(state): State<AppState>,
    Query(params): Query<DepositListParams>,
) -> ApiResult<Json<DepositList>> {
    let status = params
        .status
        .unwrap_or_else(|| "pending_approval".to_string());
    if !["pending_approval", "posted", "rejected"].contains(&status.as_str()) {
        return Err(ApiError::BadRequest(
            "status must be pending_approval, posted or rejected".to_string(),
        ));
    }
    let limit = params.limit.unwrap_or(50).clamp(1, 200);
    let (order, cmp) = if status == "pending_approval" {
        ("ASC", ">")
    } else {
        ("DESC", "<")
    };
    let base = format!("{} WHERE d.status = $1", deposit_select());
    let pool = state.ledger.pool();
    let rows = match &params.cursor {
        None => {
            sqlx::query(&format!(
                "{base} ORDER BY d.requested_at {order}, d.id {order} LIMIT $2"
            ))
            .bind(&status)
            .bind(limit)
            .fetch_all(pool)
            .await
        }
        Some(raw) => {
            let (ts, id) = parse_cursor(raw)?;
            sqlx::query(&format!(
                "{base} AND (d.requested_at, d.id) {cmp} ($3::timestamptz, $4)
                 ORDER BY d.requested_at {order}, d.id {order} LIMIT $2"
            ))
            .bind(&status)
            .bind(limit)
            .bind(ts)
            .bind(id)
            .fetch_all(pool)
            .await
        }
    }
    .map_err(cursor_db_error)?;
    let items = rows
        .iter()
        .map(deposit_row)
        .collect::<ApiResult<Vec<_>>>()?;
    let next_cursor = if items.len() as i64 == limit {
        items.last().map(|d| d.cursor.clone())
    } else {
        None
    };
    Ok(Json(DepositList { items, next_cursor }))
}
