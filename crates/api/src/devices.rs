// Device binding (DESIGN.md §11): a stolen bearer token alone cannot move money.
//
// The app holds a per-install EC P-256 key in the Android Keystore that only a successful
// device authentication unlocks. Its public half is registered here with the account password
// (POST /v1/devices). Every money move — transfer, FX, app check payment — then carries
// `X-Device-Id` + `X-Device-Signature` (base64 DER ECDSA/SHA-256) over the canonical payload
// below, rebuilt server-side from the request and verified against an ACTIVE device of the
// caller. `DEVICE_BINDING` decides whether an unsigned request is refused (`required`, the
// default outside dev) or let through (`optional`: a signature that is sent is still checked).
//
// Where the check sits in a money handler: right after the idempotent replay, before the
// status / KYC / screening gates and the post. A replay moves no money and answers only what
// GET /v1/transactions/{id} already tells the same bearer; answering it first means a retry of
// a posted key still learns its outcome after the device was revoked (a retry carries the
// stored original signature anyway). The device row is fetched by the handler's existing
// context query (one round trip); last_used_at is refreshed outside the posting transaction.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use base64::Engine;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::pkcs8::{DecodePublicKey, EncodePublicKey};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::common::{db_err, rfc3339};
use crate::config::DeviceBinding;
use crate::middleware::current_request_id;
use crate::session::{verify_password_async, AuthUser};
use crate::{ApiError, ApiResult, AppState};

/// Domain-separation tag and version of the signed payload. A new layout gets a new tag.
pub const AUTH_PAYLOAD_TAG: &str = "tj.payment.authorize.v1";

const MAX_LABEL_CHARS: usize = 64;
const MAX_PASSWORD_BYTES: usize = 128;
const MAX_PUBLIC_KEY_B64: usize = 512;
// last_used_at is refreshed at most this often per device, so a busy device costs one small
// UPDATE every few minutes rather than one per payment.
const TOUCH_INTERVAL: &str = "5 minutes";

/// Which money move a signature approves (the app's `PaymentKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoneyMove {
    Transfer,
    Check,
    Fx,
}

impl MoneyMove {
    pub fn wire(&self) -> &'static str {
        match self {
            MoneyMove::Transfer => "transfer",
            MoneyMove::Check => "check",
            MoneyMove::Fx => "fx",
        }
    }
}

/// Everything that decides where money goes and how much, as the server sees the request.
///
/// - transfer: `from_account`/`to_account`/`amount_minor`/`currency` of the body
///   (`currency` defaults to TJS when omitted), no check id;
/// - fx: the body's accounts and amount, `currency` = the SOURCE wallet's currency;
/// - check (POST /v1/checks/{id}/pay): `from_account` = the body's `account` (empty when the
///   payer let the server pick the wallet), `to_account` = the check's merchant wallet,
///   amount and currency of the check, `check_id` = the check.
#[derive(Clone, Copy, Debug)]
pub struct PaymentAuth<'a> {
    pub kind: MoneyMove,
    pub user_id: Uuid,
    pub idempotency_key: Uuid,
    pub from_account: Option<Uuid>,
    pub to_account: Uuid,
    pub amount_minor: i64,
    pub currency: &'a str,
    pub check_id: Option<Uuid>,
}

impl PaymentAuth<'_> {
    /// The canonical bytes the device signs (shared with the app: `authorizationPayload()`).
    ///
    /// Nine fields in this order — tag, user id, kind, idempotency key, from account,
    /// to account, amount (decimal), currency, check id (empty when none) — each written as
    /// `<UTF-8 byte length in decimal>:<value>;`. Length-prefixed, so no value can be mistaken
    /// for a separator; UUIDs in lowercase hyphenated form.
    pub fn payload(&self) -> Vec<u8> {
        let fields = [
            AUTH_PAYLOAD_TAG.to_string(),
            self.user_id.to_string(),
            self.kind.wire().to_string(),
            self.idempotency_key.to_string(),
            self.from_account.map(|a| a.to_string()).unwrap_or_default(),
            self.to_account.to_string(),
            self.amount_minor.to_string(),
            self.currency.to_string(),
            self.check_id.map(|c| c.to_string()).unwrap_or_default(),
        ];
        let mut out = Vec::with_capacity(320);
        for f in &fields {
            out.extend_from_slice(f.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(f.as_bytes());
            out.push(b';');
        }
        out
    }
}

/// What a money request presented in `X-Device-Id` / `X-Device-Signature`.
pub(crate) enum DeviceProof {
    Absent,
    /// One header without the other, or a value that does not parse.
    Malformed,
    Present {
        device_id: Uuid,
        signature: Signature,
    },
}

impl DeviceProof {
    pub(crate) fn from_headers(headers: &HeaderMap) -> Self {
        match (
            headers.get("x-device-id"),
            headers.get("x-device-signature"),
        ) {
            (None, None) => DeviceProof::Absent,
            (Some(id), Some(sig)) => {
                let parsed = (|| {
                    let device_id = Uuid::parse_str(id.to_str().ok()?.trim()).ok()?;
                    let der = base64::engine::general_purpose::STANDARD
                        .decode(sig.to_str().ok()?.trim())
                        .ok()?;
                    let signature = Signature::from_der(&der).ok()?;
                    Some(DeviceProof::Present {
                        device_id,
                        signature,
                    })
                })();
                parsed.unwrap_or(DeviceProof::Malformed)
            }
            _ => DeviceProof::Malformed,
        }
    }

    /// The device row to fold into the handler's context query (none when binding is off).
    pub(crate) fn lookup(&self, binding: DeviceBinding) -> Option<Uuid> {
        match (binding, self) {
            (DeviceBinding::Off, _) => None,
            (_, DeviceProof::Present { device_id, .. }) => Some(*device_id),
            _ => None,
        }
    }
}

/// Columns for a `LEFT JOIN devices dv ON dv.id = <presented id> AND dv.user_id = <caller>
/// AND dv.revoked_at IS NULL` in a handler's context query.
pub(crate) fn device_columns() -> String {
    format!(
        "dv.public_key AS device_key,
         (dv.last_used_at IS NULL OR dv.last_used_at < now() - interval '{TOUCH_INTERVAL}')
           AS device_touch_due"
    )
}

/// The caller's active device the request named, as found by the context query.
pub(crate) struct DeviceKey {
    public_key: Vec<u8>,
    touch_due: bool,
}

impl DeviceKey {
    pub(crate) fn from_row(row: &PgRow) -> ApiResult<Option<Self>> {
        let public_key: Option<Vec<u8>> = row.try_get("device_key").map_err(db_err)?;
        let touch_due: Option<bool> = row.try_get("device_touch_due").map_err(db_err)?;
        Ok(public_key.map(|public_key| DeviceKey {
            public_key,
            touch_due: touch_due.unwrap_or(false),
        }))
    }
}

/// Enforce `DEVICE_BINDING` for one money request. `key` is the active device of the caller
/// named by the request (None when unknown, revoked, another user's, or not looked up).
pub(crate) async fn verify(
    conn: &mut PgConnection,
    binding: DeviceBinding,
    proof: &DeviceProof,
    key: Option<&DeviceKey>,
    auth: &PaymentAuth<'_>,
) -> ApiResult<()> {
    let outcome = |o: &'static str| {
        metrics::counter!("device_signatures_total", "kind" => auth.kind.wire(), "outcome" => o)
            .increment(1);
    };
    let (device_id, signature) = match (binding, proof) {
        (DeviceBinding::Off, _) | (DeviceBinding::Optional, DeviceProof::Absent) => return Ok(()),
        (DeviceBinding::Required, DeviceProof::Absent) => {
            outcome("missing");
            return Err(ApiError::DeviceSignatureRequired);
        }
        (_, DeviceProof::Malformed) => {
            outcome("malformed");
            return Err(ApiError::DeviceSignatureInvalid);
        }
        (
            _,
            DeviceProof::Present {
                device_id,
                signature,
            },
        ) => (*device_id, signature),
    };
    let Some(key) = key else {
        outcome("unknown_device");
        tracing::warn!(user_id = %auth.user_id, %device_id, "money request signed by an unknown or revoked device");
        return Err(ApiError::DeviceSignatureInvalid);
    };
    let verifying = VerifyingKey::from_public_key_der(&key.public_key).map_err(|e| {
        tracing::error!(%device_id, error = %e, "stored device key does not parse");
        ApiError::DeviceSignatureInvalid
    })?;
    if verifying.verify(&auth.payload(), signature).is_err() {
        outcome("bad_signature");
        tracing::warn!(user_id = %auth.user_id, %device_id, kind = auth.kind.wire(), "device signature does not match the request");
        return Err(ApiError::DeviceSignatureInvalid);
    }
    outcome("valid");
    if key.touch_due {
        // Outside any transaction and before the posting locks; a failure costs nothing.
        let touched = sqlx::query(&format!(
            "UPDATE devices SET last_used_at = now()
             WHERE id = $1 AND revoked_at IS NULL
               AND (last_used_at IS NULL OR last_used_at < now() - interval '{TOUCH_INTERVAL}')"
        ))
        .bind(device_id)
        .execute(&mut *conn)
        .await;
        if let Err(e) = touched {
            tracing::warn!(%device_id, error = %e, "could not record device use");
        }
    }
    Ok(())
}

// --- registration endpoints ---

#[derive(Deserialize)]
pub struct RegisterDeviceRequest {
    public_key: String,
    label: String,
    password: String,
}

#[derive(Serialize)]
pub struct DeviceResponse {
    id: Uuid,
    label: String,
    /// Base64 SPKI DER (canonical, uncompressed point).
    public_key: String,
    created_at: String,
    last_used_at: Option<String>,
    revoked_at: Option<String>,
}

#[derive(Serialize)]
pub struct DeviceList {
    items: Vec<DeviceResponse>,
}

fn columns(prefix: &str) -> String {
    format!(
        "{prefix}id, {prefix}label, {prefix}public_key, {} AS created_at,
         {} AS last_used_at, {} AS revoked_at",
        rfc3339(&format!("{prefix}created_at")),
        rfc3339(&format!("{prefix}last_used_at")),
        rfc3339(&format!("{prefix}revoked_at")),
    )
}

fn device_row(row: &PgRow) -> ApiResult<DeviceResponse> {
    let key: Vec<u8> = row.try_get("public_key").map_err(db_err)?;
    Ok(DeviceResponse {
        id: row.try_get("id").map_err(db_err)?,
        label: row.try_get("label").map_err(db_err)?,
        public_key: base64::engine::general_purpose::STANDARD.encode(key),
        created_at: row.try_get("created_at").map_err(db_err)?,
        last_used_at: row.try_get("last_used_at").map_err(db_err)?,
        revoked_at: row.try_get("revoked_at").map_err(db_err)?,
    })
}

/// A base64 SPKI DER that must be a P-256 public key, re-encoded canonically so the same key
/// is the same bytes however the client encoded it.
pub(crate) fn canonical_public_key(b64: &str) -> ApiResult<Vec<u8>> {
    let bad = || {
        ApiError::BadRequest(
            "public_key must be a base64 SubjectPublicKeyInfo DER P-256 key".into(),
        )
    };
    if b64.len() > MAX_PUBLIC_KEY_B64 {
        return Err(bad());
    }
    let der = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| bad())?;
    let key = p256::PublicKey::from_public_key_der(&der).map_err(|_| bad())?;
    let doc = key.to_public_key_der().map_err(|_| bad())?;
    Ok(doc.as_bytes().to_vec())
}

fn registration(outcome: &'static str) {
    metrics::counter!("device_registrations_total", "outcome" => outcome).increment(1);
}

pub async fn register_device(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Json(req): Json<RegisterDeviceRequest>,
) -> ApiResult<(StatusCode, Json<DeviceResponse>)> {
    let label = req.label.trim();
    if label.is_empty()
        || label.chars().count() > MAX_LABEL_CHARS
        || label.chars().any(char::is_control)
    {
        return Err(ApiError::BadRequest(format!(
            "label must be 1 to {MAX_LABEL_CHARS} printable characters"
        )));
    }
    let public_key = canonical_public_key(&req.public_key)?;
    if req.password.is_empty() || req.password.len() > MAX_PASSWORD_BYTES {
        return Err(ApiError::BadRequest("password is required".to_string()));
    }

    // The password is re-proven (a bearer token alone must not bind a new key), through the
    // login path's Argon2 verification and its per-phone throttle: guesses made here count
    // against the same budget as guesses at /v1/auth/login.
    let pool = state.ledger.pool();
    let row = sqlx::query("SELECT phone, password_hash FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?
        .ok_or_else(|| ApiError::Unauthorized("unknown user".to_string()))?;
    let phone: String = row.try_get("phone").map_err(db_err)?;
    let hash: String = row.try_get("password_hash").map_err(db_err)?;
    let throttle_key = format!("login:{phone}");
    if state.login_limit.over_limit(&throttle_key).await {
        registration("throttled");
        return Err(ApiError::TooManyRequests);
    }
    if !verify_password_async(req.password, hash).await? {
        state.login_limit.hit(&throttle_key).await;
        registration("wrong_password");
        tracing::warn!(%user_id, "device registration with a wrong password");
        return Err(ApiError::Forbidden("incorrect password".to_string()));
    }

    // Serialised per user on the users row (as a status change or a post would be), so two
    // concurrent registrations cannot both pass the device cap, and a freeze that commits
    // first is seen here.
    let mut tx = pool.begin().await.map_err(db_err)?;
    let row = sqlx::query(
        "SELECT u.status, (bu.user_id IS NOT NULL) AS blocked,
                (SELECT count(*) FROM devices
                 WHERE user_id = u.id AND revoked_at IS NULL) AS active,
                d.id AS existing_id
         FROM users u
         LEFT JOIN blocked_users bu ON bu.user_id = u.id
         LEFT JOIN devices d
                ON d.user_id = u.id AND d.public_key = $2 AND d.revoked_at IS NULL
         WHERE u.id = $1
         FOR NO KEY UPDATE OF u",
    )
    .bind(user_id)
    .bind(&public_key)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?
    .ok_or_else(|| ApiError::Unauthorized("unknown user".to_string()))?;
    let status: String = row.try_get("status").map_err(db_err)?;
    let blocked: bool = row.try_get("blocked").map_err(db_err)?;
    let active: i64 = row.try_get("active").map_err(db_err)?;
    let existing: Option<Uuid> = row.try_get("existing_id").map_err(db_err)?;
    if blocked {
        registration("blocked");
        return Err(ApiError::Blocked("account is blocked".to_string()));
    }
    if status != "active" {
        registration("inactive");
        return Err(ApiError::Forbidden(format!("account is {status}")));
    }

    if let Some(id) = existing {
        tx.rollback().await.map_err(db_err)?;
        let row = sqlx::query(&format!(
            "SELECT {} FROM devices WHERE id = $1",
            columns("")
        ))
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(db_err)?;
        registration("already_registered");
        return Ok((StatusCode::OK, Json(device_row(&row)?)));
    }
    if active >= state.devices.max_active {
        registration("limit");
        return Err(ApiError::Conflict(format!(
            "this account already has {active} active devices (the maximum is {}); revoke one first",
            state.devices.max_active
        )));
    }

    let row = sqlx::query(&format!(
        "WITH ins AS (
             INSERT INTO devices (id, user_id, public_key, label)
             VALUES ($1, $2, $3, $4)
             RETURNING *
         ), ev AS (
             INSERT INTO device_events (id, user_id, device_id, action, request_id)
             SELECT $5, ins.user_id, ins.id, 'registered', $6 FROM ins
         )
         SELECT {} FROM ins",
        columns("ins.")
    ))
    .bind(Uuid::now_v7())
    .bind(user_id)
    .bind(&public_key)
    .bind(label)
    .bind(Uuid::now_v7())
    .bind(current_request_id())
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err)?;
    storage::commit_durable(tx).await.map_err(db_err)?;
    let device = device_row(&row)?;
    registration("registered");
    tracing::info!(%user_id, device_id = %device.id, "device registered");
    Ok((StatusCode::CREATED, Json(device)))
}

pub async fn list_devices(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> ApiResult<Json<DeviceList>> {
    let rows = sqlx::query(&format!(
        "SELECT {} FROM devices WHERE user_id = $1
         ORDER BY (revoked_at IS NULL) DESC, created_at DESC, id DESC LIMIT 100",
        columns("")
    ))
    .bind(user_id)
    .fetch_all(state.ledger.pool())
    .await
    .map_err(db_err)?;
    let items = rows.iter().map(device_row).collect::<ApiResult<_>>()?;
    Ok(Json(DeviceList { items }))
}

// No password: revoking only ever takes capability away (a lost phone can be cut off from
// another device). Idempotent; another user's device is a 404.
pub async fn revoke_device(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<DeviceResponse>> {
    let cols = columns("");
    let row = sqlx::query(&format!(
        "WITH upd AS (
             UPDATE devices SET revoked_at = now()
             WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL
             RETURNING *
         ), ev AS (
             INSERT INTO device_events (id, user_id, device_id, action, request_id)
             SELECT $3, upd.user_id, upd.id, 'revoked', $4 FROM upd
         )
         SELECT {cols} FROM upd
         UNION ALL
         SELECT {cols} FROM devices
         WHERE id = $1 AND user_id = $2 AND NOT EXISTS (SELECT 1 FROM upd)"
    ))
    .bind(id)
    .bind(user_id)
    .bind(Uuid::now_v7())
    .bind(current_request_id())
    .fetch_optional(state.ledger.pool())
    .await
    .map_err(db_err)?
    .ok_or_else(|| ApiError::NotFound("device not found".to_string()))?;
    tracing::info!(%user_id, device_id = %id, "device revoked");
    Ok(Json(device_row(&row)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_is_length_prefixed_in_a_fixed_order() {
        let auth = PaymentAuth {
            kind: MoneyMove::Check,
            user_id: Uuid::from_u128(1),
            idempotency_key: Uuid::from_u128(2),
            from_account: None,
            to_account: Uuid::from_u128(3),
            amount_minor: 1500,
            currency: "TJS",
            check_id: Some(Uuid::from_u128(4)),
        };
        let expected = "23:tj.payment.authorize.v1;\
             36:00000000-0000-0000-0000-000000000001;5:check;\
             36:00000000-0000-0000-0000-000000000002;0:;\
             36:00000000-0000-0000-0000-000000000003;4:1500;3:TJS;\
             36:00000000-0000-0000-0000-000000000004;";
        assert_eq!(String::from_utf8(auth.payload()).unwrap(), expected);
    }

    #[test]
    fn a_public_key_is_stored_in_one_canonical_encoding() {
        // A fixed P-256 key's SPKI DER (uncompressed point) round-trips byte for byte.
        let spki = "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEYP7UuiVanTHJYet0xjVtaMBJuJI7Yfps5mliLmDyn7Z5A/4QCLi8maQa6elWKLxk8vGyDC1+n1F3o8KU1EYimQ==";
        let got = canonical_public_key(spki).unwrap();
        assert_eq!(base64::engine::general_purpose::STANDARD.encode(&got), spki);
        // The same key with a compressed point is stored as the same (uncompressed) bytes.
        let compressed_hex = "3039301306072a8648ce3d020106082a8648ce3d03010703220003\
             60fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6";
        let compressed: Vec<u8> = (0..compressed_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&compressed_hex[i..i + 2], 16).unwrap())
            .collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(compressed);
        assert_eq!(canonical_public_key(&b64).unwrap(), got);
        // Not a key, not P-256 (an Ed25519 SPKI), not base64.
        assert!(canonical_public_key("bm90IGEga2V5").is_err());
        assert!(canonical_public_key(
            "MCowBQYDK2VwAyEAGb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE="
        )
        .is_err());
        assert!(canonical_public_key("%%%").is_err());
        assert!(canonical_public_key("").is_err());
    }
}
