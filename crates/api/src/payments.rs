use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use ledger::{AccountId, AccountType, Entry, LedgerError, Transaction, TransactionId};
use money::{Currency, Money};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, Row};
use storage::{IdempotencyRecord, PostHook, PostOptions, StorageError};
use uuid::Uuid;

use crate::admin::audit_on;
use crate::common::{default_currency, idempotency_key};
use crate::session::{AdminUser, AuthUser};
use crate::{ApiError, ApiResult, AppState, Limits};

pub const KYC_LEVEL_FOR_TRANSFER: i16 = 1;

const SHARD_COUNT: u64 = 16;
static SHARD_RR: AtomicU64 = AtomicU64::new(0);

fn shard(base: u128) -> Uuid {
    let i = SHARD_RR.fetch_add(1, Ordering::Relaxed) % SHARD_COUNT;
    Uuid::from_u128(base + i as u128)
}

pub fn settlement_shard(currency: &str) -> Option<Uuid> {
    match currency {
        "TJS" => Some(shard(0x1000)),
        "USD" => Some(shard(0x1100)),
        _ => None,
    }
}

fn fee_shard(currency: &str) -> Option<Uuid> {
    match currency {
        "TJS" => Some(shard(0x2000)),
        _ => None,
    }
}

fn fx_shard(currency: &str) -> Option<Uuid> {
    match currency {
        "TJS" => Some(shard(0x3000)),
        "USD" => Some(shard(0x3100)),
        _ => None,
    }
}

#[derive(Deserialize)]
pub struct DepositRequest {
    user_account: Uuid,
    amount_minor: i64,
    #[serde(default = "default_currency")]
    currency: String,
}

#[derive(Deserialize)]
pub struct TransferRequest {
    from_account: Uuid,
    to_account: Uuid,
    amount_minor: i64,
    #[serde(default = "default_currency")]
    currency: String,
}

#[derive(Deserialize)]
pub struct FxRequest {
    from_account: Uuid,
    to_account: Uuid,
    amount_minor: i64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PostResponse {
    transaction_id: Uuid,
    status: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FxResponse {
    transaction_id: Uuid,
    debited_minor: i64,
    credited_minor: i64,
    from_currency: String,
    to_currency: String,
}

struct AccountRow {
    account_type: AccountType,
    owner: Option<Uuid>,
    currency: Currency,
}

struct MoneyContext {
    kyc_level: i16,
    status: String,
    sender_blocked: bool,
    from: Option<AccountRow>,
    to: Option<AccountRow>,
    recipient_blocked: bool,
    pair_rate: Option<(i64, i64)>,
    tjs_rate: Option<(i64, i64)>,
    stored: Option<(String, i32, serde_json::Value)>,
}

fn account_row(row: &sqlx::postgres::PgRow, prefix: &str) -> ApiResult<Option<AccountRow>> {
    let type_str: Option<String> = row
        .try_get(format!("{prefix}_type").as_str())
        .map_err(StorageError::from)?;
    let Some(type_str) = type_str else {
        return Ok(None);
    };
    let owner: Option<Uuid> = row
        .try_get(format!("{prefix}_owner").as_str())
        .map_err(StorageError::from)?;
    let code: String = row
        .try_get(format!("{prefix}_currency").as_str())
        .map_err(StorageError::from)?;
    let exponent: i16 = row
        .try_get(format!("{prefix}_exponent").as_str())
        .map_err(StorageError::from)?;
    let account_type = AccountType::from_db_str(&type_str)
        .ok_or_else(|| StorageError::DataIntegrity(format!("account_type={type_str}")))?;
    let currency = Currency::new(&code, exponent as u8)
        .map_err(|e| StorageError::DataIntegrity(e.to_string()))?;
    Ok(Some(AccountRow {
        account_type,
        owner,
        currency,
    }))
}

async fn money_context(
    conn: &mut PgConnection,
    user_id: Uuid,
    from: Uuid,
    to: Uuid,
    key: Uuid,
) -> ApiResult<MoneyContext> {
    let row = sqlx::query(
        "SELECT u.kyc_level, u.status,
                (bu.user_id IS NOT NULL) AS sender_blocked,
                f.account_type AS from_type, f.owner_user_id AS from_owner,
                f.currency AS from_currency, fc.exponent AS from_exponent,
                t.account_type AS to_type, t.owner_user_id AS to_owner,
                t.currency AS to_currency, tc.exponent AS to_exponent,
                (bt.user_id IS NOT NULL) AS recipient_blocked,
                rp.rate_num AS pair_num, rp.rate_den AS pair_den,
                rt.rate_num AS tjs_num, rt.rate_den AS tjs_den,
                ik.fingerprint AS ik_fingerprint, ik.response_status AS ik_status,
                ik.response_body AS ik_body
         FROM users u
         LEFT JOIN blocked_users bu ON bu.user_id = u.id
         LEFT JOIN accounts f ON f.id = $2
         LEFT JOIN currencies fc ON fc.code = f.currency
         LEFT JOIN accounts t ON t.id = $3
         LEFT JOIN currencies tc ON tc.code = t.currency
         LEFT JOIN blocked_users bt ON bt.user_id = t.owner_user_id
         LEFT JOIN fx_rates rp ON rp.base_currency = f.currency AND rp.quote_currency = t.currency
         LEFT JOIN fx_rates rt ON rt.base_currency = f.currency AND rt.quote_currency = 'TJS'
         LEFT JOIN idempotency_keys ik ON ik.key = $4
         WHERE u.id = $1",
    )
    .bind(user_id)
    .bind(from)
    .bind(to)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await
    .map_err(StorageError::from)?
    .ok_or_else(|| ApiError::Unauthorized("unknown user".to_string()))?;

    let get_rate = |num: &str, den: &str| -> ApiResult<Option<(i64, i64)>> {
        let n: Option<i64> = row.try_get(num).map_err(StorageError::from)?;
        let d: Option<i64> = row.try_get(den).map_err(StorageError::from)?;
        Ok(n.zip(d))
    };
    let ik_fingerprint: Option<String> =
        row.try_get("ik_fingerprint").map_err(StorageError::from)?;
    let stored = match ik_fingerprint {
        Some(fp) => Some((
            fp,
            row.try_get::<i32, _>("ik_status")
                .map_err(StorageError::from)?,
            row.try_get::<serde_json::Value, _>("ik_body")
                .map_err(StorageError::from)?,
        )),
        None => None,
    };

    Ok(MoneyContext {
        kyc_level: row.try_get("kyc_level").map_err(StorageError::from)?,
        status: row.try_get("status").map_err(StorageError::from)?,
        sender_blocked: row.try_get("sender_blocked").map_err(StorageError::from)?,
        from: account_row(&row, "from")?,
        to: account_row(&row, "to")?,
        recipient_blocked: row
            .try_get("recipient_blocked")
            .map_err(StorageError::from)?,
        pair_rate: get_rate("pair_num", "pair_den")?,
        tjs_rate: get_rate("tjs_num", "tjs_den")?,
        stored,
    })
}

impl MoneyContext {
    fn replay<T: serde::de::DeserializeOwned>(
        &self,
        fingerprint: &str,
    ) -> ApiResult<Option<(StatusCode, Json<T>)>> {
        let Some((stored_fp, status, body)) = &self.stored else {
            return Ok(None);
        };
        if stored_fp != fingerprint {
            return Err(ApiError::IdempotencyConflict);
        }
        let resp: T = serde_json::from_value(body.clone())
            .map_err(|e| StorageError::DataIntegrity(format!("stored idempotent response: {e}")))?;
        let status = StatusCode::from_u16(*status as u16)
            .map_err(|e| StorageError::DataIntegrity(format!("stored status: {e}")))?;
        Ok(Some((status, Json(resp))))
    }

    fn require_active(&self) -> ApiResult<()> {
        if self.status != "active" {
            return Err(ApiError::Forbidden(format!("account is {}", self.status)));
        }
        Ok(())
    }

    fn require_kyc(&self, min_level: i16) -> ApiResult<()> {
        if self.kyc_level < min_level {
            return Err(ApiError::KycRequired(format!(
                "this action requires KYC level {min_level}"
            )));
        }
        Ok(())
    }
}

fn owned(account: &Option<AccountRow>, user_id: Uuid) -> ApiResult<&AccountRow> {
    let acct = account
        .as_ref()
        .ok_or_else(|| ApiError::NotFound("account not found".to_string()))?;
    if acct.owner != Some(user_id) {
        return Err(ApiError::Forbidden(
            "you do not own this account".to_string(),
        ));
    }
    Ok(acct)
}

#[derive(Clone)]
struct ScreenCtx {
    user_id: Uuid,
    from_account: Uuid,
    to_account: Uuid,
    amount_minor: i64,
    currency: String,
}

async fn log_screening(
    conn: &mut PgConnection,
    s: &ScreenCtx,
    decision: &str,
    rule: Option<&str>,
    detail: Option<&str>,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO screening_events
           (id, user_id, from_account, to_account, amount_minor, currency, decision, rule, detail)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(Uuid::now_v7())
    .bind(s.user_id)
    .bind(s.from_account)
    .bind(s.to_account)
    .bind(s.amount_minor)
    .bind(&s.currency)
    .bind(decision)
    .bind(rule)
    .bind(detail)
    .execute(&mut *conn)
    .await
    .map_err(StorageError::from)?;
    Ok(())
}

fn to_tjs(minor: i64, (num, den): (i64, i64)) -> i64 {
    i64::try_from((minor as i128 * num as i128) / den as i128).unwrap_or(i64::MAX)
}

async fn pre_screen(
    conn: &mut PgConnection,
    state: &AppState,
    ctx: &MoneyContext,
    s: &ScreenCtx,
) -> ApiResult<((i64, i64), Limits)> {
    if ctx.sender_blocked {
        log_screening(
            conn,
            s,
            "blocked",
            Some("blocklist"),
            Some("sender is blocked"),
        )
        .await?;
        return Err(ApiError::Blocked("sender is blocked".to_string()));
    }
    if ctx.recipient_blocked {
        log_screening(
            conn,
            s,
            "blocked",
            Some("blocklist"),
            Some("recipient is blocked"),
        )
        .await?;
        return Err(ApiError::Blocked("recipient is blocked".to_string()));
    }

    let limits = state.aml.limits_for(ctx.kyc_level);
    let tjs = if s.currency == "TJS" {
        (1, 1)
    } else {
        match ctx.tjs_rate {
            Some(r) => r,
            None => {
                let msg = "no TJS conversion rate configured for AML screening of this currency";
                log_screening(conn, s, "blocked", Some("no_fx_rate"), Some(msg)).await?;
                return Err(ApiError::LimitExceeded(msg.to_string()));
            }
        }
    };
    if to_tjs(s.amount_minor, tjs) > limits.per_tx_minor {
        let msg = "amount exceeds the per-transaction limit";
        log_screening(conn, s, "blocked", Some("per_tx_limit"), Some(msg)).await?;
        return Err(ApiError::LimitExceeded(msg.to_string()));
    }
    Ok((tjs, limits))
}

fn aml_guard(s: ScreenCtx, tjs: (i64, i64), limits: Limits) -> PostHook {
    Box::new(move |conn: &mut PgConnection| {
        Box::pin(async move {
            let row = sqlx::query(
                "WITH w AS (
                     SELECT COALESCE(SUM(amount_minor), 0)::BIGINT AS daily,
                            COUNT(*) FILTER (WHERE created_at >= now() - interval '1 hour')::BIGINT AS hourly
                     FROM entries
                     WHERE account_id = $1 AND direction = 'debit'
                       AND created_at >= now() - interval '24 hours'
                 ), ok AS (
                     SELECT w.daily, w.hourly,
                            (floor(w.daily::numeric * ($2::bigint)::numeric / ($3::bigint)::numeric)
                             + floor(($4::bigint)::numeric * ($2::bigint)::numeric / ($3::bigint)::numeric))
                              <= ($5::bigint)::numeric AS daily_ok,
                            w.hourly < $6::bigint AS velocity_ok
                     FROM w
                 ), ins AS (
                     INSERT INTO screening_events
                       (id, user_id, from_account, to_account, amount_minor, currency, decision, rule, detail)
                     SELECT $7::uuid, $8::uuid, $1::uuid, $9::uuid, $4::bigint, $10::text, 'allowed', NULL, NULL
                     FROM ok WHERE ok.daily_ok AND ok.velocity_ok
                 )
                 SELECT daily_ok, velocity_ok FROM ok",
            )
            .bind(s.from_account)
            .bind(tjs.0)
            .bind(tjs.1)
            .bind(s.amount_minor)
            .bind(limits.daily_minor)
            .bind(limits.velocity_per_hour)
            .bind(Uuid::now_v7())
            .bind(s.user_id)
            .bind(s.to_account)
            .bind(&s.currency)
            .fetch_one(&mut *conn)
            .await?;
            let daily_ok: bool = row.try_get("daily_ok")?;
            let velocity_ok: bool = row.try_get("velocity_ok")?;
            if !daily_ok {
                return Err(storage::HookError::Rejected {
                    rule: "daily_limit".to_string(),
                    message: "amount exceeds the rolling 24h limit".to_string(),
                });
            }
            if !velocity_ok {
                return Err(storage::HookError::Rejected {
                    rule: "velocity".to_string(),
                    message: "too many transfers in the last hour".to_string(),
                });
            }
            Ok(())
        })
    })
}

fn admin_audit_guard(
    admin_id: Uuid,
    action: &'static str,
    target: String,
    details: serde_json::Value,
) -> PostHook {
    Box::new(move |conn: &mut PgConnection| {
        Box::pin(async move {
            audit_on(conn, admin_id, action, Some(&target), details).await?;
            Ok(())
        })
    })
}

fn record<T: Serialize>(key: Uuid, fingerprint: &str, created: &T) -> ApiResult<IdempotencyRecord> {
    Ok(IdempotencyRecord {
        key,
        fingerprint: fingerprint.to_string(),
        response_status: StatusCode::CREATED.as_u16() as i32,
        response_body: serde_json::to_value(created)
            .map_err(|e| ApiError::Internal(format!("response serialises: {e}")))?,
    })
}

pub(crate) async fn load_idempotent<T: serde::de::DeserializeOwned>(
    conn: &mut PgConnection,
    key: Uuid,
    fingerprint: &str,
) -> ApiResult<Option<(StatusCode, Json<T>)>> {
    let row = sqlx::query(
        "SELECT fingerprint, response_status, response_body FROM idempotency_keys WHERE key = $1",
    )
    .bind(key)
    .fetch_optional(&mut *conn)
    .await
    .map_err(StorageError::from)?;

    let Some(row) = row else { return Ok(None) };

    let stored_fingerprint: String = row.try_get("fingerprint").map_err(StorageError::from)?;
    if stored_fingerprint != fingerprint {
        return Err(ApiError::IdempotencyConflict);
    }
    let status: i32 = row.try_get("response_status").map_err(StorageError::from)?;
    let body: serde_json::Value = row.try_get("response_body").map_err(StorageError::from)?;
    let resp: T = serde_json::from_value(body)
        .map_err(|e| StorageError::DataIntegrity(format!("stored idempotent response: {e}")))?;
    let status = StatusCode::from_u16(status as u16)
        .map_err(|e| StorageError::DataIntegrity(format!("stored status: {e}")))?;
    Ok(Some((status, Json(resp))))
}

async fn finish<T>(
    conn: &mut PgConnection,
    result: storage::Result<()>,
    key: Uuid,
    fingerprint: &str,
    created: T,
    raced: T,
    screen: Option<&ScreenCtx>,
) -> ApiResult<(StatusCode, Json<T>)>
where
    T: Serialize + serde::de::DeserializeOwned,
{
    match result {
        Ok(()) => {
            metrics::counter!("ledger_posts_total", "outcome" => "posted").increment(1);
            Ok((StatusCode::CREATED, Json(created)))
        }
        Err(StorageError::Ledger(LedgerError::DuplicateTransaction(_))) => {
            metrics::counter!("ledger_posts_total", "outcome" => "duplicate").increment(1);
            if let Some(found) = load_idempotent::<T>(conn, key, fingerprint).await? {
                return Ok(found);
            }
            Ok((StatusCode::OK, Json(raced)))
        }
        Err(StorageError::Rejected { rule, message }) => {
            metrics::counter!("ledger_posts_total", "outcome" => "rejected").increment(1);
            if let Some(s) = screen {
                log_screening(conn, s, "blocked", Some(&rule), Some(&message)).await?;
            }
            Err(ApiError::LimitExceeded(message))
        }
        Err(e) => {
            metrics::counter!("ledger_posts_total", "outcome" => "error").increment(1);
            Err(e.into())
        }
    }
}

pub async fn create_deposit(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DepositRequest>,
) -> ApiResult<(StatusCode, Json<PostResponse>)> {
    if req.amount_minor <= 0 {
        return Err(ApiError::BadRequest(
            "amount_minor must be positive".to_string(),
        ));
    }
    let key = idempotency_key(&headers)?;
    let settlement = settlement_shard(&req.currency).ok_or_else(|| {
        ApiError::BadRequest(format!("deposits are not supported in {}", req.currency))
    })?;
    let fingerprint = format!(
        "deposit:{admin_id}:{}:{}:{}",
        req.user_account, req.amount_minor, req.currency
    );

    let mut conn = state
        .ledger
        .pool()
        .acquire()
        .await
        .map_err(StorageError::from)?;
    let ctx = money_context(&mut conn, admin_id, settlement, req.user_account, key).await?;
    if let Some(found) = ctx.replay::<PostResponse>(&fingerprint)? {
        return Ok(found);
    }

    let target = ctx
        .to
        .as_ref()
        .ok_or_else(|| ApiError::NotFound("account not found".to_string()))?;
    if target.account_type != AccountType::UserWallet {
        return Err(ApiError::BadRequest(
            "deposits must credit a user wallet".to_string(),
        ));
    }
    if target.currency.code() != req.currency {
        return Err(ApiError::BadRequest(format!(
            "wallet holds {}, not {}",
            target.currency.code(),
            req.currency
        )));
    }
    let currency = target.currency;
    let amount = Money::from_minor(req.amount_minor as i128, currency);

    let txn = Transaction::new(
        TransactionId(key),
        vec![
            Entry::debit(AccountId(settlement), amount),
            Entry::credit(AccountId(req.user_account), amount),
        ],
    );
    let created = PostResponse {
        transaction_id: key,
        status: "posted".to_string(),
    };
    let raced = PostResponse {
        transaction_id: key,
        status: "already_posted".to_string(),
    };
    let opts = PostOptions {
        idempotency: Some(record(key, &fingerprint, &created)?),
        guard: Some(admin_audit_guard(
            admin_id,
            "deposit",
            req.user_account.to_string(),
            serde_json::json!({
                "amount_minor": req.amount_minor,
                "currency": currency.code(),
                "transaction_id": key,
            }),
        )),
    };
    let result = state.ledger.post_on(&mut conn, &txn, opts).await;
    finish(&mut conn, result, key, &fingerprint, created, raced, None).await
}

pub async fn create_transfer(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TransferRequest>,
) -> ApiResult<(StatusCode, Json<PostResponse>)> {
    if req.amount_minor <= 0 {
        return Err(ApiError::BadRequest(
            "amount_minor must be positive".to_string(),
        ));
    }
    if req.from_account == req.to_account {
        return Err(ApiError::BadRequest(
            "from_account and to_account must differ".to_string(),
        ));
    }
    let key = idempotency_key(&headers)?;
    let fingerprint = format!(
        "transfer:{user_id}:{}:{}:{}:{}",
        req.from_account, req.to_account, req.amount_minor, req.currency
    );

    let mut conn = state
        .ledger
        .pool()
        .acquire()
        .await
        .map_err(StorageError::from)?;
    let ctx = money_context(&mut conn, user_id, req.from_account, req.to_account, key).await?;

    let from = owned(&ctx.from, user_id)?;
    if let Some(found) = ctx.replay::<PostResponse>(&fingerprint)? {
        return Ok(found);
    }
    ctx.require_active()?;
    ctx.require_kyc(KYC_LEVEL_FOR_TRANSFER)?;

    let to = ctx
        .to
        .as_ref()
        .ok_or_else(|| ApiError::NotFound("account not found".to_string()))?;
    if to.account_type != AccountType::UserWallet {
        return Err(ApiError::BadRequest(
            "recipient must be a user wallet".to_string(),
        ));
    }
    if from.currency.code() != req.currency {
        return Err(ApiError::BadRequest(format!(
            "wallet holds {}, not {}",
            from.currency.code(),
            req.currency
        )));
    }
    if to.currency != from.currency {
        return Err(ApiError::BadRequest(format!(
            "recipient wallet holds {}; use /v1/fx to convert",
            to.currency.code()
        )));
    }
    let currency = from.currency;

    let screen = ScreenCtx {
        user_id,
        from_account: req.from_account,
        to_account: req.to_account,
        amount_minor: req.amount_minor,
        currency: currency.code().to_string(),
    };
    let (tjs, limits) = pre_screen(&mut conn, &state, &ctx, &screen).await?;

    let amount = Money::from_minor(req.amount_minor as i128, currency);

    let fee_minor = if currency.code() == "TJS" {
        state.fees.fee_minor(req.amount_minor)
    } else {
        0
    };
    let entries = if fee_minor > 0 {
        let to_recipient_minor = req
            .amount_minor
            .checked_sub(fee_minor)
            .filter(|v| *v > 0)
            .ok_or_else(|| ApiError::BadRequest("amount does not cover the fee".to_string()))?;
        let fee_account = fee_shard(currency.code())
            .ok_or_else(|| ApiError::Internal("no fee account for currency".to_string()))?;
        vec![
            Entry::debit(AccountId(req.from_account), amount),
            Entry::credit(
                AccountId(req.to_account),
                Money::from_minor(to_recipient_minor as i128, currency),
            ),
            Entry::credit(
                AccountId(fee_account),
                Money::from_minor(fee_minor as i128, currency),
            ),
        ]
    } else {
        vec![
            Entry::debit(AccountId(req.from_account), amount),
            Entry::credit(AccountId(req.to_account), amount),
        ]
    };

    let txn = Transaction::new(TransactionId(key), entries);
    let created = PostResponse {
        transaction_id: key,
        status: "posted".to_string(),
    };
    let raced = PostResponse {
        transaction_id: key,
        status: "already_posted".to_string(),
    };
    let opts = PostOptions {
        idempotency: Some(record(key, &fingerprint, &created)?),
        guard: Some(aml_guard(screen.clone(), tjs, limits)),
    };
    let result = state.ledger.post_on(&mut conn, &txn, opts).await;
    finish(
        &mut conn,
        result,
        key,
        &fingerprint,
        created,
        raced,
        Some(&screen),
    )
    .await
}

pub async fn create_fx(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<FxRequest>,
) -> ApiResult<(StatusCode, Json<FxResponse>)> {
    let key = idempotency_key(&headers)?;
    if req.amount_minor <= 0 {
        return Err(ApiError::BadRequest(
            "amount_minor must be positive".to_string(),
        ));
    }

    let mut conn = state
        .ledger
        .pool()
        .acquire()
        .await
        .map_err(StorageError::from)?;
    let ctx = money_context(&mut conn, user_id, req.from_account, req.to_account, key).await?;

    let from = owned(&ctx.from, user_id)?;
    let to = owned(&ctx.to, user_id)?;
    let (from_cur, to_cur) = (from.currency, to.currency);

    let fingerprint = format!(
        "fx:{user_id}:{}:{}:{}:{}->{}",
        req.from_account,
        req.to_account,
        req.amount_minor,
        from_cur.code(),
        to_cur.code()
    );
    if let Some(found) = ctx.replay::<FxResponse>(&fingerprint)? {
        return Ok(found);
    }
    ctx.require_active()?;
    ctx.require_kyc(KYC_LEVEL_FOR_TRANSFER)?;

    if from_cur == to_cur {
        return Err(ApiError::BadRequest(
            "use /v1/transfers for same-currency movements".to_string(),
        ));
    }

    let screen = ScreenCtx {
        user_id,
        from_account: req.from_account,
        to_account: req.to_account,
        amount_minor: req.amount_minor,
        currency: from_cur.code().to_string(),
    };
    let (tjs, limits) = pre_screen(&mut conn, &state, &ctx, &screen).await?;

    let (rate_num, rate_den) = ctx.pair_rate.ok_or_else(|| {
        ApiError::BadRequest(format!(
            "no FX rate configured for {}->{}",
            from_cur.code(),
            to_cur.code()
        ))
    })?;
    let credited_128 = (req.amount_minor as i128 * rate_num as i128) / rate_den as i128;
    let credited_minor = i64::try_from(credited_128)
        .map_err(|_| ApiError::BadRequest("converted amount is too large".to_string()))?;
    if credited_minor <= 0 {
        return Err(ApiError::BadRequest(
            "converted amount rounds to zero".to_string(),
        ));
    }

    let fx_from = fx_shard(from_cur.code())
        .ok_or_else(|| ApiError::BadRequest("unsupported source currency".to_string()))?;
    let fx_to = fx_shard(to_cur.code())
        .ok_or_else(|| ApiError::BadRequest("unsupported target currency".to_string()))?;

    let amount_a = Money::from_minor(req.amount_minor as i128, from_cur);
    let amount_b = Money::from_minor(credited_minor as i128, to_cur);

    let txn = Transaction::new(
        TransactionId(key),
        vec![
            Entry::debit(AccountId(req.from_account), amount_a),
            Entry::credit(AccountId(fx_from), amount_a),
            Entry::debit(AccountId(fx_to), amount_b),
            Entry::credit(AccountId(req.to_account), amount_b),
        ],
    );

    let resp = FxResponse {
        transaction_id: key,
        debited_minor: req.amount_minor,
        credited_minor,
        from_currency: from_cur.code().to_string(),
        to_currency: to_cur.code().to_string(),
    };
    let opts = PostOptions {
        idempotency: Some(record(key, &fingerprint, &resp)?),
        guard: Some(aml_guard(screen.clone(), tjs, limits)),
    };
    let result = state.ledger.post_on(&mut conn, &txn, opts).await;
    finish(
        &mut conn,
        result,
        key,
        &fingerprint,
        resp.clone(),
        resp,
        Some(&screen),
    )
    .await
}
