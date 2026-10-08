use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use ledger::{AccountId, AccountType, Entry, LedgerError, Transaction, TransactionId};
use money::{Currency, Money};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use storage::{HookError, IdempotencyRecord, PostHook, PostOptions, StorageError};
use uuid::Uuid;

use crate::common::{default_currency, idempotency_key};
use crate::session::AuthUser;
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

pub(crate) fn fee_shard(currency: &str) -> Option<Uuid> {
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
    pub(crate) transaction_id: Uuid,
    pub(crate) status: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FxResponse {
    transaction_id: Uuid,
    debited_minor: i64,
    credited_minor: i64,
    from_currency: String,
    to_currency: String,
}

pub(crate) struct AccountRow {
    pub(crate) account_type: AccountType,
    pub(crate) owner: Option<Uuid>,
    pub(crate) currency: Currency,
}

pub(crate) struct MoneyContext {
    pub(crate) kyc_level: i16,
    pub(crate) status: String,
    pub(crate) sender_blocked: bool,
    pub(crate) from: Option<AccountRow>,
    pub(crate) to: Option<AccountRow>,
    pub(crate) recipient_blocked: bool,
    pub(crate) recipient_status: Option<String>,
    pub(crate) pair_rate: Option<(i64, i64)>,
    pub(crate) tjs_rate: Option<(i64, i64)>,
    pub(crate) key: KeyState,
}

// What the database already knows about an Idempotency-Key: the stored response of a post
// made with it, and whether it was voided (claimed with no entries; it can never post).
pub(crate) struct KeyState {
    stored: Option<(String, i32, serde_json::Value)>,
    voided: bool,
}

pub(crate) const KEY_STATE_COLUMNS: &str = "ik.fingerprint AS ik_fingerprint,
     ik.response_status AS ik_status, ik.response_body AS ik_body,
     (vt.id IS NOT NULL) AS key_voided";

impl KeyState {
    pub(crate) fn from_row(row: &PgRow) -> ApiResult<Self> {
        let fingerprint: Option<String> =
            row.try_get("ik_fingerprint").map_err(StorageError::from)?;
        let stored = match fingerprint {
            Some(fp) => Some((
                fp,
                row.try_get::<i32, _>("ik_status")
                    .map_err(StorageError::from)?,
                row.try_get::<serde_json::Value, _>("ik_body")
                    .map_err(StorageError::from)?,
            )),
            None => None,
        };
        Ok(Self {
            stored,
            voided: row.try_get("key_voided").map_err(StorageError::from)?,
        })
    }

    pub(crate) async fn load(conn: &mut PgConnection, key: Uuid) -> ApiResult<Self> {
        let row = sqlx::query(&format!(
            "SELECT {KEY_STATE_COLUMNS} FROM (SELECT $1::uuid AS key) k
             LEFT JOIN idempotency_keys ik ON ik.key = k.key
             LEFT JOIN voided_transactions vt ON vt.id = k.key"
        ))
        .bind(key)
        .fetch_one(&mut *conn)
        .await
        .map_err(StorageError::from)?;
        Self::from_row(&row)
    }

    pub(crate) fn voided(&self) -> bool {
        self.voided
    }

    pub(crate) fn replay<T: serde::de::DeserializeOwned>(
        &self,
        fingerprint: &str,
    ) -> ApiResult<Option<(StatusCode, Json<T>)>> {
        if self.voided {
            return Err(ApiError::Voided);
        }
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

pub(crate) async fn money_context(
    conn: &mut PgConnection,
    user_id: Uuid,
    from: Uuid,
    to: Uuid,
    key: Uuid,
) -> ApiResult<MoneyContext> {
    let row = sqlx::query(&format!(
        "SELECT u.kyc_level, u.status,
                (bu.user_id IS NOT NULL) AS sender_blocked,
                f.account_type AS from_type, f.owner_user_id AS from_owner,
                f.currency AS from_currency, fc.exponent AS from_exponent,
                t.account_type AS to_type, t.owner_user_id AS to_owner,
                t.currency AS to_currency, tc.exponent AS to_exponent,
                (bt.user_id IS NOT NULL) AS recipient_blocked, tu.status AS recipient_status,
                rp.rate_num AS pair_num, rp.rate_den AS pair_den,
                rt.rate_num AS tjs_num, rt.rate_den AS tjs_den,
                {KEY_STATE_COLUMNS}
         FROM users u
         LEFT JOIN blocked_users bu ON bu.user_id = u.id
         LEFT JOIN accounts f ON f.id = $2
         LEFT JOIN currencies fc ON fc.code = f.currency
         LEFT JOIN accounts t ON t.id = $3
         LEFT JOIN currencies tc ON tc.code = t.currency
         LEFT JOIN blocked_users bt ON bt.user_id = t.owner_user_id
         LEFT JOIN users tu ON tu.id = t.owner_user_id
         LEFT JOIN fx_rates rp ON rp.base_currency = f.currency AND rp.quote_currency = t.currency
         LEFT JOIN fx_rates rt ON rt.base_currency = f.currency AND rt.quote_currency = 'TJS'
         LEFT JOIN idempotency_keys ik ON ik.key = $4
         LEFT JOIN voided_transactions vt ON vt.id = $4
         WHERE u.id = $1"
    ))
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

    Ok(MoneyContext {
        kyc_level: row.try_get("kyc_level").map_err(StorageError::from)?,
        status: row.try_get("status").map_err(StorageError::from)?,
        sender_blocked: row.try_get("sender_blocked").map_err(StorageError::from)?,
        from: account_row(&row, "from")?,
        to: account_row(&row, "to")?,
        recipient_blocked: row
            .try_get("recipient_blocked")
            .map_err(StorageError::from)?,
        recipient_status: row
            .try_get("recipient_status")
            .map_err(StorageError::from)?,
        pair_rate: get_rate("pair_num", "pair_den")?,
        tjs_rate: get_rate("tjs_num", "tjs_den")?,
        key: KeyState::from_row(&row)?,
    })
}

impl MoneyContext {
    pub(crate) fn replay<T: serde::de::DeserializeOwned>(
        &self,
        fingerprint: &str,
    ) -> ApiResult<Option<(StatusCode, Json<T>)>> {
        self.key.replay(fingerprint)
    }

    pub(crate) fn require_active(&self) -> ApiResult<()> {
        if self.status != "active" {
            return Err(ApiError::Forbidden(format!("account is {}", self.status)));
        }
        Ok(())
    }

    pub(crate) fn require_recipient_active(&self) -> ApiResult<()> {
        match self.recipient_status.as_deref() {
            None | Some("active") => Ok(()),
            Some(_) => Err(ApiError::RecipientUnavailable(
                "the recipient cannot receive payments".to_string(),
            )),
        }
    }

    pub(crate) fn require_kyc(&self, min_level: i16) -> ApiResult<()> {
        if self.kyc_level < min_level {
            return Err(ApiError::KycRequired(format!(
                "this action requires KYC level {min_level}"
            )));
        }
        Ok(())
    }
}

pub(crate) fn owned(account: &Option<AccountRow>, user_id: Uuid) -> ApiResult<&AccountRow> {
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
pub(crate) struct ScreenCtx {
    pub(crate) user_id: Uuid,
    pub(crate) from_account: Uuid,
    pub(crate) to_account: Uuid,
    pub(crate) amount_minor: i64,
    pub(crate) currency: String,
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

pub(crate) async fn pre_screen(
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

// AML windows are per USER across all their wallets, so the guard serialises one user's posts
// on the user's row. It runs inside post_on after the wallets are locked; the window is then
// summed in a NEW statement (a fresh READ COMMITTED snapshot taken after the lock was granted),
// so a concurrent post from any wallet of the same user is either fully visible or still
// queued behind the lock — the limit is exact. No deadlock: every posting transaction locks
// its wallets first (one statement, id order), then exactly one users row, then system shards
// last (sorted, never followed by another lock); nothing that holds a users row lock (status
// and KYC changes, wallet creation) ever waits for a wallet balance. NO KEY UPDATE so FK checks
// against users (KEY SHARE, e.g. inserting checks, events, refresh tokens) never queue on it.
pub(crate) async fn lock_user(conn: &mut PgConnection, user_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR NO KEY UPDATE")
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

// Every debit of the user's wallets in the window, each currency converted to TJS at the
// current rate (floored per currency, as the per-transaction check does). A debit currency
// with no TJS rate cannot be priced, so the guard fails closed.
pub(crate) async fn aml_check(
    conn: &mut PgConnection,
    s: &ScreenCtx,
    tjs: (i64, i64),
    limits: Limits,
) -> Result<(), HookError> {
    let row = sqlx::query(
        "WITH d AS (
             SELECT a.currency, SUM(e.amount_minor)::numeric AS total,
                    COUNT(*) FILTER (WHERE e.created_at >= now() - interval '1 hour') AS hourly
             FROM accounts a
             JOIN entries e ON e.account_id = a.id
             WHERE a.owner_user_id = $1 AND a.account_type = 'user_wallet'
               AND e.direction = 'debit' AND e.created_at >= now() - interval '24 hours'
             GROUP BY a.currency
         ), w AS (
             SELECT COALESCE(SUM(CASE WHEN d.currency = 'TJS' THEN d.total
                                      ELSE floor(d.total * r.rate_num / r.rate_den) END), 0) AS daily,
                    COALESCE(SUM(d.hourly), 0) AS hourly,
                    COUNT(*) FILTER (WHERE d.currency <> 'TJS' AND r.rate_num IS NULL) AS unpriced
             FROM d
             LEFT JOIN fx_rates r ON r.base_currency = d.currency AND r.quote_currency = 'TJS'
         ), ok AS (
             SELECT w.unpriced = 0 AS priced_ok,
                    w.daily + floor(($2::bigint)::numeric * ($3::bigint)::numeric / ($4::bigint)::numeric)
                      <= ($5::bigint)::numeric AS daily_ok,
                    w.hourly < $6::bigint AS velocity_ok
             FROM w
         ), ins AS (
             INSERT INTO screening_events
               (id, user_id, from_account, to_account, amount_minor, currency, decision, rule, detail)
             SELECT $7::uuid, $1::uuid, $8::uuid, $9::uuid, $2::bigint, $10::text, 'allowed', NULL, NULL
             FROM ok WHERE ok.priced_ok AND ok.daily_ok AND ok.velocity_ok
         )
         SELECT priced_ok, daily_ok, velocity_ok FROM ok",
    )
    .bind(s.user_id)
    .bind(s.amount_minor)
    .bind(tjs.0)
    .bind(tjs.1)
    .bind(limits.daily_minor)
    .bind(limits.velocity_per_hour)
    .bind(Uuid::now_v7())
    .bind(s.from_account)
    .bind(s.to_account)
    .bind(&s.currency)
    .fetch_one(&mut *conn)
    .await?;
    let reject = |rule: &str, message: &str| HookError::Rejected {
        rule: rule.to_string(),
        message: message.to_string(),
    };
    if !row.try_get::<bool, _>("priced_ok")? {
        return Err(reject(
            "no_fx_rate",
            "a wallet's recent debits cannot be converted to TJS for AML screening",
        ));
    }
    if !row.try_get::<bool, _>("daily_ok")? {
        return Err(reject(
            "daily_limit",
            "amount exceeds the rolling 24h limit",
        ));
    }
    if !row.try_get::<bool, _>("velocity_ok")? {
        return Err(reject("velocity", "too many transfers in the last hour"));
    }
    Ok(())
}

pub(crate) fn aml_guard(s: ScreenCtx, tjs: (i64, i64), limits: Limits) -> PostHook {
    Box::new(move |conn: &mut PgConnection| {
        Box::pin(async move {
            lock_user(conn, s.user_id).await?;
            aml_check(conn, &s, tjs, limits).await
        })
    })
}

pub(crate) fn record<T: Serialize>(
    key: Uuid,
    fingerprint: &str,
    created: &T,
) -> ApiResult<IdempotencyRecord> {
    Ok(IdempotencyRecord {
        key,
        fingerprint: fingerprint.to_string(),
        response_status: StatusCode::CREATED.as_u16() as i32,
        response_body: serde_json::to_value(created)
            .map_err(|e| ApiError::Internal(format!("response serialises: {e}")))?,
    })
}

pub(crate) async fn finish<T>(
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
            if let Some(found) = KeyState::load(conn, key).await?.replay::<T>(fingerprint)? {
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
    ctx.require_recipient_active()?;
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
