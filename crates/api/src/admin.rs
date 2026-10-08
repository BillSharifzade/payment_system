use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, Postgres, Row, Transaction};
use storage::StorageError;
use uuid::Uuid;

use crate::common::{cursor_db_error, normalize_phone, parse_cursor};
use crate::reads::{wallets_of, WalletResponse};
use crate::session::AdminUser;
use crate::{ApiError, ApiResult, AppState};

pub(crate) async fn audit_on(
    conn: &mut PgConnection,
    admin_id: Uuid,
    action: &str,
    target: Option<&str>,
    details: serde_json::Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO admin_actions (id, admin_id, action, target, details)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(admin_id)
    .bind(action)
    .bind(target)
    .bind(details)
    .execute(&mut *conn)
    .await?;
    tracing::info!(%admin_id, action, target = target.unwrap_or("-"), "admin action");
    Ok(())
}

pub(crate) async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    admin_id: Uuid,
    action: &str,
    target: Option<&str>,
    details: serde_json::Value,
) -> ApiResult<()> {
    audit_on(&mut *tx, admin_id, action, target, details)
        .await
        .map_err(StorageError::from)?;
    Ok(())
}

#[derive(Deserialize)]
pub struct BlockUserRequest {
    user_id: Uuid,
    reason: String,
}

pub async fn block_user(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    Json(req): Json<BlockUserRequest>,
) -> ApiResult<StatusCode> {
    let mut tx = state
        .ledger
        .pool()
        .begin()
        .await
        .map_err(StorageError::from)?;
    sqlx::query(
        "INSERT INTO blocked_users (user_id, reason, blocked_by)
         VALUES ($1, $2, $3)
         ON CONFLICT (user_id) DO UPDATE SET reason = EXCLUDED.reason, blocked_by = EXCLUDED.blocked_by",
    )
    .bind(req.user_id)
    .bind(&req.reason)
    .bind(admin_id)
    .execute(&mut *tx)
    .await
    .map_err(StorageError::from)?;
    audit(
        &mut tx,
        admin_id,
        "user.block",
        Some(&req.user_id.to_string()),
        serde_json::json!({ "reason": req.reason }),
    )
    .await?;
    storage::commit_durable(tx)
        .await
        .map_err(StorageError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn unblock_user(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let mut tx = state
        .ledger
        .pool()
        .begin()
        .await
        .map_err(StorageError::from)?;
    sqlx::query("DELETE FROM blocked_users WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    audit(
        &mut tx,
        admin_id,
        "user.unblock",
        Some(&user_id.to_string()),
        serde_json::json!({}),
    )
    .await?;
    storage::commit_durable(tx)
        .await
        .map_err(StorageError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct SetUserStatusRequest {
    status: String,
}

pub async fn set_user_status(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
    Json(req): Json<SetUserStatusRequest>,
) -> ApiResult<StatusCode> {
    if !["active", "frozen", "closed"].contains(&req.status.as_str()) {
        return Err(ApiError::BadRequest(
            "status must be active, frozen or closed".to_string(),
        ));
    }
    let mut tx = state
        .ledger
        .pool()
        .begin()
        .await
        .map_err(StorageError::from)?;
    let updated = sqlx::query("UPDATE users SET status = $2 WHERE id = $1")
        .bind(user_id)
        .bind(&req.status)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound("no user with that id".to_string()));
    }
    if req.status != "active" {
        sqlx::query(
            "UPDATE refresh_tokens SET revoked_at = now()
             WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    }
    audit(
        &mut tx,
        admin_id,
        "user.status",
        Some(&user_id.to_string()),
        serde_json::json!({ "status": req.status }),
    )
    .await?;
    storage::commit_durable(tx)
        .await
        .map_err(StorageError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct SetFxRateRequest {
    base: String,
    quote: String,
    rate_num: i64,
    rate_den: i64,
    #[serde(default)]
    also_reverse: bool,
}

pub async fn set_fx_rate(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    Json(req): Json<SetFxRateRequest>,
) -> ApiResult<StatusCode> {
    if req.rate_num <= 0 || req.rate_den <= 0 {
        return Err(ApiError::BadRequest(
            "rate_num and rate_den must be positive".to_string(),
        ));
    }
    let base = req.base.to_uppercase();
    let quote = req.quote.to_uppercase();
    if base == quote {
        return Err(ApiError::BadRequest(
            "base and quote must differ".to_string(),
        ));
    }
    state.ledger.lookup_currency(&base).await?;
    state.ledger.lookup_currency(&quote).await?;

    let mut tx = state
        .ledger
        .pool()
        .begin()
        .await
        .map_err(StorageError::from)?;
    let upsert = "INSERT INTO fx_rates (base_currency, quote_currency, rate_num, rate_den, updated_at)
                  VALUES ($1, $2, $3, $4, now())
                  ON CONFLICT (base_currency, quote_currency)
                  DO UPDATE SET rate_num = EXCLUDED.rate_num, rate_den = EXCLUDED.rate_den, updated_at = now()";
    sqlx::query(upsert)
        .bind(&base)
        .bind(&quote)
        .bind(req.rate_num)
        .bind(req.rate_den)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    if req.also_reverse {
        sqlx::query(upsert)
            .bind(&quote)
            .bind(&base)
            .bind(req.rate_den)
            .bind(req.rate_num)
            .execute(&mut *tx)
            .await
            .map_err(StorageError::from)?;
    }
    audit(
        &mut tx,
        admin_id,
        "fx_rate.set",
        Some(&format!("{base}->{quote}")),
        serde_json::json!({
            "rate_num": req.rate_num,
            "rate_den": req.rate_den,
            "also_reverse": req.also_reverse,
        }),
    )
    .await?;
    storage::commit_durable(tx)
        .await
        .map_err(StorageError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct KycListParams {
    status: Option<String>,
    limit: Option<i64>,
    after: Option<String>,
}

#[derive(Serialize)]
pub struct KycSubmissionDetail {
    id: Uuid,
    user_id: Uuid,
    phone: String,
    requested_level: i16,
    full_name: String,
    document_type: String,
    document_ref: String,
    status: String,
    created_at_ms: i64,
    cursor: String,
}

pub async fn list_kyc_submissions(
    _admin: AdminUser,
    State(state): State<AppState>,
    Query(params): Query<KycListParams>,
) -> ApiResult<Json<Vec<KycSubmissionDetail>>> {
    let status = params.status.unwrap_or_else(|| "pending".to_string());
    if !["pending", "approved", "rejected"].contains(&status.as_str()) {
        return Err(ApiError::BadRequest(
            "status must be pending, approved or rejected".to_string(),
        ));
    }
    let limit = params.limit.unwrap_or(100).clamp(1, 500);
    let after = match &params.after {
        None => None,
        Some(raw) => Some(parse_cursor(raw)?),
    };

    let base = "SELECT k.id, k.user_id, u.phone, k.requested_level, k.full_name,
                       k.document_type, k.document_ref, k.status,
                       k.created_at::text AS ts,
                       (EXTRACT(EPOCH FROM k.created_at) * 1000)::BIGINT AS ms
                FROM kyc_submissions k
                JOIN users u ON u.id = k.user_id
                WHERE k.status = $1";
    let rows = match &after {
        None => {
            sqlx::query(&format!("{base} ORDER BY k.created_at, k.id LIMIT $2"))
                .bind(&status)
                .bind(limit)
                .fetch_all(state.ledger.pool())
                .await
        }
        Some((ts, id)) => {
            sqlx::query(&format!(
                "{base} AND (k.created_at, k.id) > ($3::timestamptz, $4)
                 ORDER BY k.created_at, k.id LIMIT $2"
            ))
            .bind(&status)
            .bind(limit)
            .bind(ts)
            .bind(id)
            .fetch_all(state.ledger.pool())
            .await
        }
    }
    .map_err(cursor_db_error)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: Uuid = row.try_get("id").map_err(StorageError::from)?;
        let ts: String = row.try_get("ts").map_err(StorageError::from)?;
        out.push(KycSubmissionDetail {
            id,
            user_id: row.try_get("user_id").map_err(StorageError::from)?,
            phone: row.try_get("phone").map_err(StorageError::from)?,
            requested_level: row.try_get("requested_level").map_err(StorageError::from)?,
            full_name: row.try_get("full_name").map_err(StorageError::from)?,
            document_type: row.try_get("document_type").map_err(StorageError::from)?,
            document_ref: row.try_get("document_ref").map_err(StorageError::from)?,
            status: row.try_get("status").map_err(StorageError::from)?,
            created_at_ms: row.try_get("ms").map_err(StorageError::from)?,
            cursor: format!("{ts}|{id}"),
        });
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct UserLookupParams {
    phone: String,
}

#[derive(Serialize)]
pub struct AdminUserResponse {
    id: Uuid,
    phone: String,
    status: String,
    kyc_level: i16,
    is_admin: bool,
    created_at_ms: i64,
    blocked_reason: Option<String>,
    full_name: Option<String>,
    wallets: Vec<WalletResponse>,
}

pub async fn admin_user_lookup(
    _admin: AdminUser,
    State(state): State<AppState>,
    Query(params): Query<UserLookupParams>,
) -> ApiResult<Json<AdminUserResponse>> {
    let phone = normalize_phone(&params.phone)
        .ok_or_else(|| ApiError::BadRequest("malformed phone number".to_string()))?;

    let row = sqlx::query(
        "SELECT u.id, u.phone, u.status, u.kyc_level, u.is_admin,
                (EXTRACT(EPOCH FROM u.created_at) * 1000)::BIGINT AS ms,
                b.reason AS blocked_reason, k.full_name
         FROM users u
         LEFT JOIN blocked_users b ON b.user_id = u.id
         LEFT JOIN LATERAL (
             SELECT full_name FROM kyc_submissions
             WHERE user_id = u.id AND status = 'approved'
             ORDER BY reviewed_at DESC NULLS LAST, created_at DESC LIMIT 1
         ) k ON TRUE
         WHERE u.phone = $1",
    )
    .bind(&phone)
    .fetch_optional(state.ledger.pool())
    .await
    .map_err(StorageError::from)?
    .ok_or_else(|| ApiError::NotFound("no user with that phone".to_string()))?;

    let user_id: Uuid = row.try_get("id").map_err(StorageError::from)?;
    Ok(Json(AdminUserResponse {
        id: user_id,
        phone: row.try_get("phone").map_err(StorageError::from)?,
        status: row.try_get("status").map_err(StorageError::from)?,
        kyc_level: row.try_get("kyc_level").map_err(StorageError::from)?,
        is_admin: row.try_get("is_admin").map_err(StorageError::from)?,
        created_at_ms: row.try_get("ms").map_err(StorageError::from)?,
        blocked_reason: row.try_get("blocked_reason").map_err(StorageError::from)?,
        full_name: row.try_get("full_name").map_err(StorageError::from)?,
        wallets: wallets_of(&state, user_id).await?,
    }))
}

#[derive(Deserialize)]
pub struct UserListParams {
    q: Option<String>,
    cursor: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
pub struct UserListItem {
    id: Uuid,
    phone: String,
    status: String,
    kyc_level: i16,
    is_admin: bool,
    is_blocked: bool,
    created_at_ms: i64,
}

#[derive(Serialize)]
pub struct UserListResponse {
    users: Vec<UserListItem>,
    next_cursor: Option<String>,
}

pub async fn admin_user_list(
    _admin: AdminUser,
    State(state): State<AppState>,
    Query(params): Query<UserListParams>,
) -> ApiResult<Json<UserListResponse>> {
    let limit = params.limit.unwrap_or(25).clamp(1, 100);

    let base = "SELECT u.id, u.phone, u.status, u.kyc_level, u.is_admin,
                       u.created_at::text AS ts,
                       (EXTRACT(EPOCH FROM u.created_at) * 1000)::BIGINT AS ms,
                       EXISTS(SELECT 1 FROM blocked_users b WHERE b.user_id = u.id) AS is_blocked
                FROM users u";

    let prefix: Option<String> = params.q.as_deref().map(|q| {
        q.chars()
            .filter(|c| c.is_ascii_digit())
            .take(15)
            .collect::<String>()
    });

    let rows = match &prefix {
        Some(p) if p.is_empty() => Vec::new(),
        Some(p) => sqlx::query(&format!(
            "{base} WHERE u.phone LIKE $1 ORDER BY u.phone LIMIT $2"
        ))
        .bind(format!("{p}%"))
        .bind(limit)
        .fetch_all(state.ledger.pool())
        .await
        .map_err(StorageError::from)?,
        None => match &params.cursor {
            None => sqlx::query(&format!(
                "{base} ORDER BY u.created_at DESC, u.id DESC LIMIT $1"
            ))
            .bind(limit)
            .fetch_all(state.ledger.pool())
            .await
            .map_err(StorageError::from)?,
            Some(raw) => {
                let (ts, uid) = parse_cursor(raw)?;
                sqlx::query(&format!(
                    "{base} WHERE (u.created_at, u.id) < ($2::timestamptz, $3)
                     ORDER BY u.created_at DESC, u.id DESC LIMIT $1"
                ))
                .bind(limit)
                .bind(ts)
                .bind(uid)
                .fetch_all(state.ledger.pool())
                .await
                .map_err(cursor_db_error)?
            }
        },
    };

    let mut users = Vec::with_capacity(rows.len());
    let mut last: Option<(String, Uuid)> = None;
    for row in rows {
        let id: Uuid = row.try_get("id").map_err(StorageError::from)?;
        let ts: String = row.try_get("ts").map_err(StorageError::from)?;
        users.push(UserListItem {
            id,
            phone: row.try_get("phone").map_err(StorageError::from)?,
            status: row.try_get("status").map_err(StorageError::from)?,
            kyc_level: row.try_get("kyc_level").map_err(StorageError::from)?,
            is_admin: row.try_get("is_admin").map_err(StorageError::from)?,
            is_blocked: row.try_get("is_blocked").map_err(StorageError::from)?,
            created_at_ms: row.try_get("ms").map_err(StorageError::from)?,
        });
        last = Some((ts, id));
    }

    let next_cursor = if prefix.is_none() && users.len() as i64 == limit {
        last.map(|(ts, id)| format!("{ts}|{id}"))
    } else {
        None
    };

    Ok(Json(UserListResponse { users, next_cursor }))
}

#[derive(Serialize)]
pub struct CheckpointStatus {
    seq: i64,
    to_txn_seq: i64,
    txn_count: i64,
    created_at_ms: i64,
}

#[derive(Serialize)]
pub struct ConservationStatus {
    currency: String,
    net_minor: i64,
}

#[derive(Serialize)]
pub struct AdminStatusResponse {
    latest_checkpoint: Option<CheckpointStatus>,
    unsealed_transactions: i64,
    conservation: Vec<ConservationStatus>,
    deposit_dual_control: bool,
    deposit_max_minor: i64,
    pending_deposits: i64,
}

async fn pending_deposits(state: &AppState) -> ApiResult<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM deposit_requests WHERE status = 'pending_approval'",
    )
    .fetch_one(state.ledger.pool())
    .await
    .map_err(StorageError::from)?)
}

pub async fn admin_status(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<AdminStatusResponse>> {
    let cp = sqlx::query(
        "SELECT seq, to_txn_seq, txn_count,
                (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS ms
         FROM checkpoints ORDER BY seq DESC LIMIT 1",
    )
    .fetch_optional(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;
    let latest_checkpoint = match cp {
        None => None,
        Some(row) => Some(CheckpointStatus {
            seq: row.try_get("seq").map_err(StorageError::from)?,
            to_txn_seq: row.try_get("to_txn_seq").map_err(StorageError::from)?,
            txn_count: row.try_get("txn_count").map_err(StorageError::from)?,
            created_at_ms: row.try_get("ms").map_err(StorageError::from)?,
        }),
    };

    let unsealed_transactions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM transactions WHERE sealed_seq IS NULL")
            .fetch_one(state.ledger.pool())
            .await
            .map_err(StorageError::from)?;

    let conservation = state
        .ledger
        .conservation()
        .await?
        .into_iter()
        .map(|(currency, net_minor)| ConservationStatus {
            currency,
            net_minor,
        })
        .collect();

    Ok(Json(AdminStatusResponse {
        latest_checkpoint,
        unsealed_transactions,
        conservation,
        deposit_dual_control: state.deposits.dual_control,
        deposit_max_minor: state.deposits.max_minor,
        pending_deposits: pending_deposits(&state).await?,
    }))
}

#[derive(Serialize)]
pub struct DailyVolume {
    date: String,
    currency: String,
    volume_minor: i64,
}

#[derive(Serialize)]
pub struct DailyCount {
    date: String,
    count: i64,
}

#[derive(Serialize)]
pub struct MixSlice {
    kind: String,
    count: i64,
}

#[derive(Serialize)]
pub struct KycDaily {
    date: String,
    approved: i64,
    rejected: i64,
}

#[derive(Serialize)]
pub struct KycFunnel {
    pending: i64,
    approved: i64,
    rejected: i64,
    decisions_14d: Vec<KycDaily>,
}

#[derive(Serialize)]
pub struct CurrencyTotal {
    currency: String,
    total_minor: i64,
}

#[derive(Serialize)]
pub struct UsersMetrics {
    total: i64,
    blocked: i64,
    new_30d: Vec<DailyCount>,
}

#[derive(Serialize)]
pub struct MetricsResponse {
    daily_volume: Vec<DailyVolume>,
    daily_transactions: Vec<DailyCount>,
    mix_30d: Vec<MixSlice>,
    kyc: KycFunnel,
    customer_funds: Vec<CurrencyTotal>,
    users: UsersMetrics,
    aml_blocked_30d: i64,
    pending_deposits: i64,
}

pub async fn admin_metrics(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<MetricsResponse>> {
    let pool = state.ledger.pool();

    let volume_rows = sqlx::query(
        "SELECT to_char(date_trunc('day', created_at), 'YYYY-MM-DD') AS date, currency,
                SUM(amount_minor)::BIGINT AS volume
         FROM entries
         WHERE direction = 'debit' AND created_at >= now() - interval '30 days'
         GROUP BY 1, 2 ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::from)?;
    let mut daily_volume = Vec::with_capacity(volume_rows.len());
    for row in volume_rows {
        daily_volume.push(DailyVolume {
            date: row.try_get("date").map_err(StorageError::from)?,
            currency: row.try_get("currency").map_err(StorageError::from)?,
            volume_minor: row.try_get("volume").map_err(StorageError::from)?,
        });
    }

    let txn_rows = sqlx::query(
        "SELECT to_char(date_trunc('day', created_at), 'YYYY-MM-DD') AS date,
                COUNT(*)::BIGINT AS count
         FROM transactions t
         WHERE created_at >= now() - interval '30 days'
           AND NOT EXISTS (SELECT 1 FROM voided_transactions v WHERE v.id = t.id)
         GROUP BY 1 ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::from)?;
    let mut daily_transactions = Vec::with_capacity(txn_rows.len());
    for row in txn_rows {
        daily_transactions.push(DailyCount {
            date: row.try_get("date").map_err(StorageError::from)?,
            count: row.try_get("count").map_err(StorageError::from)?,
        });
    }

    let mix_rows = sqlx::query(
        "SELECT kind, COUNT(*)::BIGINT AS count FROM (
             SELECT t.id,
                    CASE
                      WHEN bool_or(a.account_type = 'system_settlement') THEN 'deposit'
                      WHEN bool_or(a.account_type = 'system_fx_gain_loss') THEN 'fx'
                      ELSE 'transfer'
                    END AS kind
             FROM transactions t
             JOIN entries e ON e.transaction_id = t.id
             JOIN accounts a ON a.id = e.account_id
             WHERE t.created_at >= now() - interval '30 days'
             GROUP BY t.id
         ) classified GROUP BY kind ORDER BY kind",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::from)?;
    let mut mix_30d = Vec::with_capacity(mix_rows.len());
    for row in mix_rows {
        mix_30d.push(MixSlice {
            kind: row.try_get("kind").map_err(StorageError::from)?,
            count: row.try_get("count").map_err(StorageError::from)?,
        });
    }

    let funnel_rows = sqlx::query(
        "SELECT status, COUNT(*)::BIGINT AS count FROM kyc_submissions GROUP BY status",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::from)?;
    let (mut pending, mut approved, mut rejected) = (0i64, 0i64, 0i64);
    for row in funnel_rows {
        let status: String = row.try_get("status").map_err(StorageError::from)?;
        let count: i64 = row.try_get("count").map_err(StorageError::from)?;
        match status.as_str() {
            "pending" => pending = count,
            "approved" => approved = count,
            "rejected" => rejected = count,
            _ => {}
        }
    }
    let decision_rows = sqlx::query(
        "SELECT to_char(date_trunc('day', reviewed_at), 'YYYY-MM-DD') AS date,
                COUNT(*) FILTER (WHERE status = 'approved')::BIGINT AS approved,
                COUNT(*) FILTER (WHERE status = 'rejected')::BIGINT AS rejected
         FROM kyc_submissions
         WHERE reviewed_at >= now() - interval '14 days'
         GROUP BY 1 ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::from)?;
    let mut decisions_14d = Vec::with_capacity(decision_rows.len());
    for row in decision_rows {
        decisions_14d.push(KycDaily {
            date: row.try_get("date").map_err(StorageError::from)?,
            approved: row.try_get("approved").map_err(StorageError::from)?,
            rejected: row.try_get("rejected").map_err(StorageError::from)?,
        });
    }

    let customer_funds = state
        .ledger
        .customer_funds()
        .await?
        .into_iter()
        .map(|(currency, total_minor)| CurrencyTotal {
            currency,
            total_minor,
        })
        .collect();

    let users_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await
        .map_err(StorageError::from)?;
    let users_blocked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blocked_users")
        .fetch_one(pool)
        .await
        .map_err(StorageError::from)?;
    let new_rows = sqlx::query(
        "SELECT to_char(date_trunc('day', created_at), 'YYYY-MM-DD') AS date,
                COUNT(*)::BIGINT AS count
         FROM users WHERE created_at >= now() - interval '30 days'
         GROUP BY 1 ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::from)?;
    let mut new_30d = Vec::with_capacity(new_rows.len());
    for row in new_rows {
        new_30d.push(DailyCount {
            date: row.try_get("date").map_err(StorageError::from)?,
            count: row.try_get("count").map_err(StorageError::from)?,
        });
    }

    let aml_blocked_30d: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM screening_events
         WHERE decision = 'blocked' AND created_at >= now() - interval '30 days'",
    )
    .fetch_one(pool)
    .await
    .map_err(StorageError::from)?;

    Ok(Json(MetricsResponse {
        daily_volume,
        daily_transactions,
        mix_30d,
        kyc: KycFunnel {
            pending,
            approved,
            rejected,
            decisions_14d,
        },
        customer_funds,
        users: UsersMetrics {
            total: users_total,
            blocked: users_blocked,
            new_30d,
        },
        aml_blocked_30d,
        pending_deposits: pending_deposits(&state).await?,
    }))
}
