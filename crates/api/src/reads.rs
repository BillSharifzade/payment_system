use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use ledger::{Account, AccountId, AccountType, LedgerError};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use storage::StorageError;
use uuid::Uuid;

use crate::common::{cursor_db_error, default_currency, normalize_phone, parse_cursor};
use crate::kyc::ensure_kyc;
use crate::session::AuthUser;
use crate::{ApiError, ApiResult, AppState};

#[derive(Deserialize)]
pub struct CreateWalletRequest {
    #[serde(default = "default_currency")]
    currency: String,
}

#[derive(Serialize)]
pub struct AccountResponse {
    id: Uuid,
    account_type: String,
    currency: String,
}

pub async fn create_wallet(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateWalletRequest>,
) -> ApiResult<(StatusCode, Json<AccountResponse>)> {
    let currency = state.ledger.lookup_currency(&req.currency).await?;
    // One wallet per currency per user. Creations serialise on the user's row; the lookup is a
    // separate statement so its snapshot is taken after the lock and sees a racing creation.
    let mut tx = state
        .ledger
        .pool()
        .begin()
        .await
        .map_err(StorageError::from)?;
    let user: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM users WHERE id = $1 FOR NO KEY UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(StorageError::from)?;
    if user.is_none() {
        return Err(ApiError::Unauthorized("unknown user".to_string()));
    }
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM accounts
         WHERE owner_user_id = $1 AND currency = $2 AND account_type = 'user_wallet'
         ORDER BY created_at, id LIMIT 1",
    )
    .bind(user_id)
    .bind(currency.code())
    .fetch_optional(&mut *tx)
    .await
    .map_err(StorageError::from)?;
    let (status, id) = match existing {
        Some(id) => (StatusCode::OK, id),
        None => {
            let id = AccountId::new();
            state
                .ledger
                .open_account_owned_on(
                    &mut tx,
                    &Account::new(id, AccountType::UserWallet, currency),
                    Some(user_id),
                )
                .await?;
            (StatusCode::CREATED, id.as_uuid())
        }
    };
    storage::commit_durable(tx)
        .await
        .map_err(StorageError::from)?;
    Ok((
        status,
        Json(AccountResponse {
            id,
            account_type: AccountType::UserWallet.as_db_str().to_string(),
            currency: currency.code().to_string(),
        }),
    ))
}

#[derive(Serialize)]
pub struct BalanceResponse {
    account_id: Uuid,
    balance_minor: i64,
    currency: String,
    display: String,
}

pub async fn get_balance(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<BalanceResponse>> {
    let (owner, balance) = match state.ledger.balance_with_owner(AccountId(id)).await {
        Ok(v) => v,
        Err(StorageError::Ledger(LedgerError::UnknownAccount(_))) => {
            return Err(ApiError::NotFound("account not found".to_string()))
        }
        Err(e) => return Err(e.into()),
    };
    if owner != Some(user_id) {
        return Err(ApiError::Forbidden(
            "you do not own this account".to_string(),
        ));
    }
    let balance_minor = i64::try_from(balance.minor_units())
        .map_err(|_| StorageError::AmountTooLarge(balance.minor_units()))?;
    Ok(Json(BalanceResponse {
        account_id: id,
        balance_minor,
        currency: balance.currency().code().to_string(),
        display: balance.to_string(),
    }))
}

#[derive(Serialize)]
pub struct WalletResponse {
    id: Uuid,
    currency: String,
    balance_minor: i64,
    display: String,
}

pub(crate) async fn wallets_of(state: &AppState, owner: Uuid) -> ApiResult<Vec<WalletResponse>> {
    let mut conn = state
        .ledger
        .pool()
        .acquire()
        .await
        .map_err(StorageError::from)?;
    let ids: Vec<AccountId> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE owner_user_id = $1 ORDER BY created_at")
            .bind(owner)
            .fetch_all(&mut *conn)
            .await
            .map_err(StorageError::from)?
            .into_iter()
            .map(AccountId)
            .collect();
    let balances = state.ledger.balances(&mut *conn, &ids).await?;
    balances
        .into_iter()
        .map(|b| {
            let balance_minor = i64::try_from(b.posted.minor_units())
                .map_err(|_| StorageError::AmountTooLarge(b.posted.minor_units()))?;
            Ok(WalletResponse {
                id: b.account.as_uuid(),
                currency: b.posted.currency().code().to_string(),
                balance_minor,
                display: b.posted.to_string(),
            })
        })
        .collect()
}

pub async fn list_wallets(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<WalletResponse>>> {
    Ok(Json(wallets_of(&state, user_id).await?))
}

#[derive(Serialize)]
pub struct ClientConfigResponse {
    transfer_fee_bps: u32,
    biometric_max_minor: i64,
    check_ttl_secs: i64,
    biometric_identify: bool,
}

pub async fn client_config(
    AuthUser(_user_id): AuthUser,
    State(state): State<AppState>,
) -> ApiResult<Json<ClientConfigResponse>> {
    Ok(Json(ClientConfigResponse {
        transfer_fee_bps: state.fees.transfer_bps,
        biometric_max_minor: state.biometric.max_minor,
        check_ttl_secs: state.biometric.check_ttl_secs,
        biometric_identify: state.biometric.identify,
    }))
}

#[derive(Deserialize)]
pub struct ResolveParams {
    phone: Option<String>,
    wallet: Option<Uuid>,
}

#[derive(Serialize)]
pub struct ResolveResponse {
    wallet_id: Uuid,
    currency: String,
    name: Option<String>,
    name_verified: bool,
}

pub async fn resolve_recipient(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Query(params): Query<ResolveParams>,
) -> ApiResult<Json<ResolveResponse>> {
    ensure_kyc(&state, user_id, 1).await?;
    if !state
        .resolve_limit
        .allow(&format!("resolve:{user_id}"))
        .await
    {
        return Err(ApiError::TooManyRequests);
    }

    let (recipient_id, wallet_id, currency): (Uuid, Uuid, String) =
        match (params.phone.as_deref(), params.wallet) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(ApiError::BadRequest(
                    "provide exactly one of `phone` or `wallet`".to_string(),
                ));
            }
            (Some(raw), None) => {
                let phone = normalize_phone(raw)
                    .ok_or_else(|| ApiError::BadRequest("malformed phone number".to_string()))?;
                let row = sqlx::query(
                    "SELECT u.id AS owner, a.id AS wallet, a.currency
                     FROM users u
                     LEFT JOIN LATERAL (
                         SELECT id, currency FROM accounts
                         WHERE owner_user_id = u.id AND currency = 'TJS'
                         ORDER BY created_at LIMIT 1
                     ) a ON TRUE
                     WHERE u.phone = $1",
                )
                .bind(&phone)
                .fetch_optional(state.ledger.pool())
                .await
                .map_err(StorageError::from)?
                .ok_or_else(|| ApiError::NotFound("no account with that number".to_string()))?;
                let owner: Uuid = row.try_get("owner").map_err(StorageError::from)?;
                let wallet: Option<Uuid> = row.try_get("wallet").map_err(StorageError::from)?;
                let wallet = wallet.ok_or_else(|| {
                    ApiError::NotFound("recipient cannot receive TJS".to_string())
                })?;
                let currency: Option<String> =
                    row.try_get("currency").map_err(StorageError::from)?;
                (owner, wallet, currency.unwrap_or_else(default_currency))
            }
            (None, Some(wid)) => {
                let row = sqlx::query("SELECT owner_user_id, currency FROM accounts WHERE id = $1")
                    .bind(wid)
                    .fetch_optional(state.ledger.pool())
                    .await
                    .map_err(StorageError::from)?
                    .ok_or_else(|| ApiError::NotFound("no such wallet".to_string()))?;
                let owner: Option<Uuid> =
                    row.try_get("owner_user_id").map_err(StorageError::from)?;
                let owner =
                    owner.ok_or_else(|| ApiError::NotFound("not a user wallet".to_string()))?;
                (
                    owner,
                    wid,
                    row.try_get("currency").map_err(StorageError::from)?,
                )
            }
        };

    let name: Option<String> = sqlx::query_scalar(
        "SELECT full_name FROM kyc_submissions
         WHERE user_id = $1 AND status = 'approved'
         ORDER BY reviewed_at DESC NULLS LAST, created_at DESC
         LIMIT 1",
    )
    .bind(recipient_id)
    .fetch_optional(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;

    Ok(Json(ResolveResponse {
        wallet_id,
        currency,
        name_verified: name.is_some(),
        name,
    }))
}

#[derive(Deserialize)]
pub struct StatementParams {
    cursor: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
pub struct StatementEntry {
    entry_id: Uuid,
    transaction_id: Uuid,
    direction: String,
    amount_minor: i64,
    currency: String,
    created_at_ms: i64,
    kind: String,
    counterparty_phone: Option<String>,
    counterparty_name: Option<String>,
}

#[derive(Serialize)]
pub struct StatementResponse {
    entries: Vec<StatementEntry>,
    next_cursor: Option<String>,
}

pub async fn list_account_transactions(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<StatementParams>,
) -> ApiResult<Json<StatementResponse>> {
    let limit = params.limit.unwrap_or(50).clamp(1, 100);
    let cursor = match &params.cursor {
        None => None,
        Some(raw) => Some(parse_cursor(raw)?),
    };

    let base = "SELECT e.id, e.transaction_id, e.direction, e.amount_minor, e.currency,
                       e.created_at, e.created_at::text AS ts,
                       (EXTRACT(EPOCH FROM e.created_at) * 1000)::BIGINT AS ms,
                       cp.account_type AS cp_type,
                       cp.owner_user_id AS cp_owner,
                       cp.phone AS cp_phone,
                       cp.verified_name AS cp_name
                FROM entries e
                LEFT JOIN LATERAL (
                    SELECT ca.account_type, ca.owner_user_id, u.phone,
                           (SELECT ks.full_name FROM kyc_submissions ks
                             WHERE ks.user_id = ca.owner_user_id AND ks.status = 'approved'
                             ORDER BY ks.reviewed_at DESC NULLS LAST, ks.created_at DESC
                             LIMIT 1) AS verified_name
                    FROM entries ce
                    JOIN accounts ca ON ca.id = ce.account_id
                    LEFT JOIN users u ON u.id = ca.owner_user_id
                    WHERE ce.transaction_id = e.transaction_id
                      AND ce.account_id <> e.account_id
                    ORDER BY (ce.direction <> e.direction) DESC,
                             (ca.account_type = 'user_wallet') DESC,
                             (ce.currency = e.currency) DESC
                    LIMIT 1
                ) cp ON TRUE
                WHERE e.account_id = $1";
    // The ownership check rides in the same round trip: no row = no such account, one row
    // with no entry = the owner's empty page or someone else's account.
    let page = |after: &str| {
        format!(
            "SELECT a.owner_user_id AS owner, p.*
             FROM accounts a
             LEFT JOIN LATERAL ({base} AND a.owner_user_id = $3 {after}
                                ORDER BY e.created_at DESC, e.id DESC LIMIT $2) p ON TRUE
             WHERE a.id = $1
             ORDER BY p.created_at DESC, p.id DESC"
        )
    };
    let rows = match &cursor {
        None => {
            sqlx::query(&page(""))
                .bind(id)
                .bind(limit)
                .bind(user_id)
                .fetch_all(state.ledger.pool())
                .await
        }
        Some((ts, eid)) => {
            sqlx::query(&page("AND (e.created_at, e.id) < ($4::timestamptz, $5)"))
                .bind(id)
                .bind(limit)
                .bind(user_id)
                .bind(ts)
                .bind(eid)
                .fetch_all(state.ledger.pool())
                .await
        }
    }
    .map_err(cursor_db_error)?;

    let owner: Option<Uuid> = rows
        .first()
        .ok_or_else(|| ApiError::NotFound("account not found".to_string()))?
        .try_get("owner")
        .map_err(StorageError::from)?;
    if owner != Some(user_id) {
        return Err(ApiError::Forbidden(
            "you do not own this account".to_string(),
        ));
    }
    let mut entries = Vec::with_capacity(rows.len());
    let mut last: Option<(String, Uuid)> = None;
    for row in rows {
        let entry_id: Option<Uuid> = row.try_get("id").map_err(StorageError::from)?;
        let Some(entry_id) = entry_id else {
            continue;
        };
        let ts: String = row.try_get("ts").map_err(StorageError::from)?;
        let direction: String = row.try_get("direction").map_err(StorageError::from)?;
        let cp_type: Option<String> = row.try_get("cp_type").map_err(StorageError::from)?;
        let cp_owner: Option<Uuid> = row.try_get("cp_owner").map_err(StorageError::from)?;

        let kind = match (cp_type.as_deref(), cp_owner) {
            (Some("user_wallet"), Some(owner)) if owner == user_id => "fx",
            (Some("user_wallet"), _) => "transfer",
            (Some("system_settlement"), _) if direction == "credit" => "deposit",
            (Some("system_settlement"), _) => "withdrawal",
            (Some("system_fx_gain_loss"), _) => "fx",
            (Some("system_fee_revenue"), _) => "fee",
            _ => "other",
        };
        let (counterparty_phone, counterparty_name) = if kind == "transfer" {
            (
                row.try_get("cp_phone").map_err(StorageError::from)?,
                row.try_get("cp_name").map_err(StorageError::from)?,
            )
        } else {
            (None, None)
        };

        entries.push(StatementEntry {
            entry_id,
            transaction_id: row.try_get("transaction_id").map_err(StorageError::from)?,
            direction,
            amount_minor: row.try_get("amount_minor").map_err(StorageError::from)?,
            currency: row.try_get("currency").map_err(StorageError::from)?,
            created_at_ms: row.try_get("ms").map_err(StorageError::from)?,
            kind: kind.to_string(),
            counterparty_phone,
            counterparty_name,
        });
        last = Some((ts, entry_id));
    }

    let next_cursor = if entries.len() as i64 == limit {
        last.map(|(ts, eid)| format!("{ts}|{eid}"))
    } else {
        None
    };
    Ok(Json(StatementResponse {
        entries,
        next_cursor,
    }))
}

#[derive(Serialize)]
pub struct FxRateResponse {
    base: String,
    quote: String,
    rate_num: i64,
    rate_den: i64,
    updated_at_ms: i64,
}

pub async fn list_fx_rates(
    _user: AuthUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<FxRateResponse>>> {
    let rows = sqlx::query(
        "SELECT base_currency, quote_currency, rate_num, rate_den,
                (EXTRACT(EPOCH FROM updated_at) * 1000)::BIGINT AS ms
         FROM fx_rates ORDER BY base_currency, quote_currency",
    )
    .fetch_all(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;

    let mut rates = Vec::with_capacity(rows.len());
    for row in rows {
        rates.push(FxRateResponse {
            base: row.try_get("base_currency").map_err(StorageError::from)?,
            quote: row.try_get("quote_currency").map_err(StorageError::from)?,
            rate_num: row.try_get("rate_num").map_err(StorageError::from)?,
            rate_den: row.try_get("rate_den").map_err(StorageError::from)?,
            updated_at_ms: row.try_get("ms").map_err(StorageError::from)?,
        });
    }
    Ok(Json(rates))
}
