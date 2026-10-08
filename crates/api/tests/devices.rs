// Device binding: registration of the app's Keystore keys and signature checks on every money
// move (transfer, FX, app check payment). The canonical vectors at the top are shared byte for
// byte with mobile/core's DeviceSignatureVectorsTest.kt.

mod common;

use api::devices::{MoneyMove, PaymentAuth};
use api::{DeviceBinding, DeviceConfig, RateLimitState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use common::*;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::pkcs8::{DecodePublicKey, EncodePublicKey};
use serde_json::{json, Value};
use uuid::Uuid;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
const PASSWORD: &str = "password123";

// --- canonical vectors (identical in DeviceSignatureVectorsTest.kt) ---

// The RFC 6979 A.2.5 P-256 test key, and its SubjectPublicKeyInfo DER.
const VECTOR_KEY_HEX: &str = "c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721";
const VECTOR_SPKI: &str = "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEYP7UuiVanTHJYet0xjVtaMBJuJI7Yfps5mliLmDyn7Z5A/4QCLi8maQa6elWKLxk8vGyDC1+n1F3o8KU1EYimQ==";

struct Vector {
    auth: PaymentAuth<'static>,
    payload: &'static str,
    /// RFC 6979 (deterministic) signature made here, verified by the Kotlin test.
    rust_sig: &'static str,
    /// Made once by java.security SHA256withECDSA (what the Keystore runs), verified here.
    java_sig: &'static str,
}

fn u(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap()
}

fn vectors() -> Vec<Vector> {
    let user = u("11111111-1111-4111-8111-111111111111");
    vec![
        Vector {
            auth: PaymentAuth {
                kind: MoneyMove::Transfer,
                user_id: user,
                idempotency_key: u("22222222-2222-4222-8222-222222222222"),
                from_account: Some(u("33333333-3333-4333-8333-333333333333")),
                to_account: u("44444444-4444-4444-8444-444444444444"),
                amount_minor: 2500,
                currency: "TJS",
                check_id: None,
            },
            payload: "23:tj.payment.authorize.v1;36:11111111-1111-4111-8111-111111111111;\
                      8:transfer;36:22222222-2222-4222-8222-222222222222;\
                      36:33333333-3333-4333-8333-333333333333;\
                      36:44444444-4444-4444-8444-444444444444;4:2500;3:TJS;0:;",
            rust_sig: "MEUCIHE0pzWUCRslKd4m3rTsEz0OVTBWUIHCiMj90gcoVUJpAiEAh25kQ4JWz+HW2tYB0Zs7af8jNOzbBExJAZTgsSNbUCk=",
            java_sig: "MEUCIQDkg2FM4TvYKgF9B8O4M3VZ/ddp5znKq7Eqxr9QjmGJdAIgF3rq4LjeG5ocb9l3Jk9Ingl/r1t6DfjBIZNGo9IaaOk=",
        },
        Vector {
            auth: PaymentAuth {
                kind: MoneyMove::Fx,
                user_id: user,
                idempotency_key: u("55555555-5555-4555-8555-555555555555"),
                from_account: Some(u("66666666-6666-4666-8666-666666666666")),
                to_account: u("33333333-3333-4333-8333-333333333333"),
                amount_minor: 10000,
                currency: "USD",
                check_id: None,
            },
            payload: "23:tj.payment.authorize.v1;36:11111111-1111-4111-8111-111111111111;\
                      2:fx;36:55555555-5555-4555-8555-555555555555;\
                      36:66666666-6666-4666-8666-666666666666;\
                      36:33333333-3333-4333-8333-333333333333;5:10000;3:USD;0:;",
            rust_sig: "MEQCICoMDyzygwLaw+NYAWZfnhRkVRso2Xwb/T90i2wCWTqTAiBofhLuQ8f6/dzshRJx1xt0hshd6G9YTyY+r+jm67alkw==",
            java_sig: "MEUCIQDoYfQvdZE7gfzMxCo2tQlyDOHnKXrtWYdTi+KqNk+EYQIgQxyie5uWHyWdAcZooeGZv/rTFi0SZVtg+Q5MNxjf7yM=",
        },
        Vector {
            auth: PaymentAuth {
                kind: MoneyMove::Check,
                user_id: user,
                idempotency_key: u("77777777-7777-4777-8777-777777777777"),
                from_account: Some(u("33333333-3333-4333-8333-333333333333")),
                to_account: u("88888888-8888-4888-8888-888888888888"),
                amount_minor: 1500,
                currency: "TJS",
                check_id: Some(u("99999999-9999-4999-8999-999999999999")),
            },
            payload: "23:tj.payment.authorize.v1;36:11111111-1111-4111-8111-111111111111;\
                      5:check;36:77777777-7777-4777-8777-777777777777;\
                      36:33333333-3333-4333-8333-333333333333;\
                      36:88888888-8888-4888-8888-888888888888;4:1500;3:TJS;\
                      36:99999999-9999-4999-8999-999999999999;",
            rust_sig: "MEUCIDiBaX59vCc/7zc1hXOG5dEaHI6EBizjhPeDzAuiWY3iAiEAzw4/f+4/U8iA2ejLXNB2euBDJYrpQ/DaNALfN8lSjvA=",
            java_sig: "MEQCIGzE5Q+nKnrOpIHZMHBc9pyzNIkhYTEL1bO+iJm259hJAiAS4UqGCVYoBVxic7XK04HPFqW9oX5uGcN6OnOIVqVjAw==",
        },
    ]
}

fn vector_key() -> SigningKey {
    let bytes: Vec<u8> = (0..VECTOR_KEY_HEX.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&VECTOR_KEY_HEX[i..i + 2], 16).unwrap())
        .collect();
    SigningKey::from_slice(&bytes).unwrap()
}

fn verify_b64(spki: &str, payload: &[u8], sig_b64: &str) -> bool {
    let key = VerifyingKey::from_public_key_der(&B64.decode(spki).unwrap()).unwrap();
    let sig = Signature::from_der(&B64.decode(sig_b64).unwrap()).unwrap();
    key.verify(payload, &sig).is_ok()
}

#[test]
fn canonical_payloads_and_signatures_match_the_app_vectors() {
    let key = vector_key();
    assert_eq!(spki(&key), VECTOR_SPKI);
    for v in vectors() {
        let payload = v.auth.payload();
        assert_eq!(String::from_utf8(payload.clone()).unwrap(), v.payload);
        let sig: Signature = key.sign(&payload);
        assert_eq!(B64.encode(sig.to_der().as_bytes()), v.rust_sig);
        assert!(verify_b64(VECTOR_SPKI, &payload, v.rust_sig));
        assert!(
            verify_b64(VECTOR_SPKI, &payload, v.java_sig),
            "a java.security signature must verify: {}",
            v.payload
        );
        // Any change to what is signed breaks it.
        let tampered = PaymentAuth {
            amount_minor: v.auth.amount_minor + 1,
            ..v.auth
        };
        assert!(!verify_b64(VECTOR_SPKI, &tampered.payload(), v.java_sig));
    }
}

// --- helpers ---

fn cfg(binding: DeviceBinding) -> TestConfig {
    TestConfig {
        devices: DeviceConfig {
            binding,
            max_active: 3,
        },
        ..TestConfig::default()
    }
}

fn new_key() -> SigningKey {
    SigningKey::random(&mut rand_core::OsRng)
}

fn spki(key: &SigningKey) -> String {
    let der = p256::PublicKey::from(key.verifying_key())
        .to_public_key_der()
        .unwrap();
    B64.encode(der.as_bytes())
}

fn sign(key: &SigningKey, auth: &PaymentAuth) -> String {
    let sig: Signature = key.sign(&auth.payload());
    B64.encode(sig.to_der().as_bytes())
}

async fn add_device(
    app: &axum::Router,
    token: &str,
    public_key: &str,
    password: &str,
) -> (StatusCode, Value) {
    send(
        app,
        req(
            "POST",
            "/v1/devices",
            Some(token),
            None,
            json!({"public_key": public_key, "label": "Pixel 8", "password": password}),
        ),
    )
    .await
}

async fn device_id(app: &axum::Router, token: &str, key: &SigningKey) -> String {
    let (status, body) = add_device(app, token, &spki(key), PASSWORD).await;
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "register device: {body}"
    );
    body["id"].as_str().unwrap().to_string()
}

async fn revoke(app: &axum::Router, token: &str, id: &str) -> (StatusCode, Value) {
    send(
        app,
        req(
            "POST",
            &format!("/v1/devices/{id}/revoke"),
            Some(token),
            None,
            Value::Null,
        ),
    )
    .await
}

fn with_headers(mut r: Request<Body>, headers: &[(&'static str, &str)]) -> Request<Body> {
    for (name, value) in headers {
        r.headers_mut().insert(*name, value.parse().unwrap());
    }
    r
}

fn signed(r: Request<Body>, device: &str, signature: &str) -> Request<Body> {
    with_headers(
        r,
        &[("x-device-id", device), ("x-device-signature", signature)],
    )
}

fn transfer_req(token: &str, from: &str, to: &str, amount: i64, key: &str) -> Request<Body> {
    req(
        "POST",
        "/v1/transfers",
        Some(token),
        Some(key),
        json!({"from_account": from, "to_account": to, "amount_minor": amount, "currency": "TJS"}),
    )
}

fn transfer_auth<'a>(user: &str, key: &str, from: &str, to: &str, amount: i64) -> PaymentAuth<'a> {
    PaymentAuth {
        kind: MoneyMove::Transfer,
        user_id: u(user),
        idempotency_key: u(key),
        from_account: Some(u(from)),
        to_account: u(to),
        amount_minor: amount,
        currency: "TJS",
        check_id: None,
    }
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

async fn device_events(pool: &sqlx::PgPool, user: &str, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM device_events WHERE user_id = $1 AND action = $2")
        .bind(u(user))
        .bind(action)
        .fetch_one(pool)
        .await
        .unwrap()
}

// --- registration ---

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn registration_rechecks_the_password_and_is_idempotent() {
    let (app, pool) = app_with(cfg(DeviceBinding::Required)).await;
    let (uid, tok) = register(&app).await;
    let key = new_key();

    let (status, body) = add_device(&app, &tok, &spki(&key), "not-my-password").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "wrong password: {body}");
    assert_eq!(code(&body), "forbidden");
    for bad in [
        "bm90IGEga2V5",
        // An Ed25519 key is a valid SPKI, but not P-256.
        "MCowBQYDK2VwAyEAGb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE=",
    ] {
        let (status, body) = add_device(&app, &tok, bad, PASSWORD).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }
    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/devices",
            None,
            None,
            json!({"public_key": spki(&key), "label": "x", "password": PASSWORD}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no bearer: {body}");
    assert_eq!(device_events(&pool, &uid, "registered").await, 0);

    let (status, first) = add_device(&app, &tok, &spki(&key), PASSWORD).await;
    assert_eq!(status, StatusCode::CREATED, "register: {first}");
    assert_eq!(first["public_key"], spki(&key));
    assert_eq!(first["label"], "Pixel 8");
    assert!(first["revoked_at"].is_null());
    assert!(first["last_used_at"].is_null());

    let (status, again) = add_device(&app, &tok, &spki(&key), PASSWORD).await;
    assert_eq!(status, StatusCode::OK, "re-register: {again}");
    assert_eq!(again["id"], first["id"]);

    let (status, list) = send(
        &app,
        req("GET", "/v1/devices", Some(&tok), None, Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["items"].as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list["items"][0]["id"], first["id"]);
    assert_eq!(device_events(&pool, &uid, "registered").await, 1);

    // The key is per install: another user signed in on the same phone registers it too.
    let (_, other_tok) = register(&app).await;
    let (status, theirs) = add_device(&app, &other_tok, &spki(&key), PASSWORD).await;
    assert_eq!(status, StatusCode::CREATED, "{theirs}");
    assert_ne!(theirs["id"], first["id"]);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn the_device_cap_counts_active_devices_only() {
    let (app, pool) = app_with(cfg(DeviceBinding::Required)).await;
    let (uid, tok) = register(&app).await;
    let keys: Vec<SigningKey> = (0..4).map(|_| new_key()).collect();
    let mut ids = Vec::new();
    for key in &keys[..3] {
        let (status, body) = add_device(&app, &tok, &spki(key), PASSWORD).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        ids.push(body["id"].as_str().unwrap().to_string());
    }
    let (status, body) = add_device(&app, &tok, &spki(&keys[3]), PASSWORD).await;
    assert_eq!(status, StatusCode::CONFLICT, "fourth device: {body}");
    assert_eq!(code(&body), "conflict");
    // At the cap, an already-registered key is still answered (it adds nothing).
    let (status, body) = add_device(&app, &tok, &spki(&keys[0]), PASSWORD).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, revoked) = revoke(&app, &tok, &ids[1]).await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert!(revoked["revoked_at"].is_string());
    let (status, again) = revoke(&app, &tok, &ids[1]).await;
    assert_eq!(status, StatusCode::OK, "revoke is idempotent: {again}");
    assert_eq!(again["revoked_at"], revoked["revoked_at"]);
    assert_eq!(device_events(&pool, &uid, "revoked").await, 1);

    let (status, body) = add_device(&app, &tok, &spki(&keys[3]), PASSWORD).await;
    assert_eq!(status, StatusCode::CREATED, "a slot was freed: {body}");

    // Someone else's device is not found.
    let (_, other_tok) = register(&app).await;
    let (status, body) = revoke(&app, &other_tok, &ids[0]).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, list) = send(
        &app,
        req("GET", "/v1/devices", Some(&tok), None, Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 4, "{list}");
    assert_eq!(
        items.iter().filter(|d| d["revoked_at"].is_null()).count(),
        3
    );
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn frozen_or_blocked_users_cannot_register_a_device() {
    let (app, pool) = app_with(cfg(DeviceBinding::Required)).await;
    let (frozen, frozen_tok) = register(&app).await;
    sqlx::query("UPDATE users SET status = 'frozen' WHERE id = $1")
        .bind(u(&frozen))
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = add_device(&app, &frozen_tok, &spki(&new_key()), PASSWORD).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "frozen: {body}");
    assert_eq!(code(&body), "forbidden");

    let (blocked, blocked_tok) = register(&app).await;
    sqlx::query("INSERT INTO blocked_users (user_id, reason) VALUES ($1, 'test')")
        .bind(u(&blocked))
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = add_device(&app, &blocked_tok, &spki(&new_key()), PASSWORD).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "blocked: {body}");
    assert_eq!(code(&body), "account_blocked");
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn password_guesses_share_the_login_throttle() {
    let (app, pool) = app_with(TestConfig {
        login_limit: RateLimitState::new(2, std::time::Duration::from_secs(900)),
        ..cfg(DeviceBinding::Required)
    })
    .await;
    let (uid, tok) = register(&app).await;
    let key = spki(&new_key());
    for _ in 0..2 {
        let (status, body) = add_device(&app, &tok, &key, "guess-guess").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    let (status, body) = add_device(&app, &tok, &key, PASSWORD).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "throttled: {body}");
    let phone = db_phone(&pool, &uid).await;
    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/auth/login",
            None,
            None,
            json!({"phone": phone, "password": PASSWORD}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "same bucket: {body}");
}

// --- verification on money moves ---

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn required_binding_guards_transfers() {
    let (app, pool) = app_with(cfg(DeviceBinding::Required)).await;
    let admin = admin_token(&app, &pool).await;
    let alice = party(&app, &pool, None).await;
    let bob = party(&app, &pool, None).await;
    admin_deposit(&app, &admin, &alice.wallet, 100_000).await;
    let key = new_key();
    let device = device_id(&app, &alice.token, &key).await;
    let k = Uuid::new_v4().to_string();
    let good = transfer_auth(&alice.id, &k, &alice.wallet, &bob.wallet, 1_000);
    let xfer = || transfer_req(&alice.token, &alice.wallet, &bob.wallet, 1_000, &k);

    let (status, body) = send(&app, xfer()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "unsigned: {body}");
    assert_eq!(code(&body), "device_signature_required");

    let other_key = new_key();
    let bob_key = new_key();
    let bob_device = device_id(&app, &bob.token, &bob_key).await;
    let over_less = transfer_auth(&alice.id, &k, &alice.wallet, &bob.wallet, 999);
    let random_device = Uuid::new_v4().to_string();
    let attempts: Vec<(&str, Request<Body>)> = vec![
        (
            "signature over a different amount",
            signed(xfer(), &device, &sign(&key, &over_less)),
        ),
        (
            "signed by a key that is not the device's",
            signed(xfer(), &device, &sign(&other_key, &good)),
        ),
        (
            "another user's device",
            signed(xfer(), &bob_device, &sign(&bob_key, &good)),
        ),
        (
            "unknown device",
            signed(xfer(), &random_device, &sign(&key, &good)),
        ),
        (
            "device id without a signature",
            with_headers(xfer(), &[("x-device-id", &device)]),
        ),
        (
            "signature that is not base64 DER",
            signed(xfer(), &device, "bm90IGEgc2lnbmF0dXJl"),
        ),
        (
            "device id that is not a uuid",
            signed(xfer(), "phone-1", &sign(&key, &good)),
        ),
    ];
    for (what, request) in attempts {
        let (status, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{what}: {body}");
        assert_eq!(code(&body), "device_signature_invalid", "{what}");
    }
    assert_eq!(balance_of(&app, &alice.token, &alice.wallet).await, 100_000);

    let signature = sign(&key, &good);
    let (status, posted) = send(&app, signed(xfer(), &device, &signature)).await;
    assert_eq!(status, StatusCode::CREATED, "signed: {posted}");
    assert_eq!(posted["status"], "posted");
    assert_eq!(balance_of(&app, &alice.token, &alice.wallet).await, 99_000);
    let used: Option<String> =
        sqlx::query_scalar("SELECT last_used_at::text FROM devices WHERE id = $1")
            .bind(u(&device))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(used.is_some(), "last_used_at is recorded");

    // A replay of the posted key is answered from the stored response, signed or not, and
    // even after the device is revoked; it moves nothing.
    let (status, replay) = send(&app, xfer()).await;
    assert_eq!(status, StatusCode::CREATED, "unsigned replay: {replay}");
    assert_eq!(replay, posted);
    let (status, _) = revoke(&app, &alice.token, &device).await;
    assert_eq!(status, StatusCode::OK);
    let (status, replay) = send(&app, signed(xfer(), &device, &signature)).await;
    assert_eq!(status, StatusCode::CREATED, "replay after revoke: {replay}");
    assert_eq!(replay, posted);
    assert_eq!(balance_of(&app, &alice.token, &alice.wallet).await, 99_000);

    // A revoked device signs nothing new — until the key is registered again (password).
    let k2 = Uuid::new_v4().to_string();
    let next = transfer_auth(&alice.id, &k2, &alice.wallet, &bob.wallet, 500);
    let next_req = || transfer_req(&alice.token, &alice.wallet, &bob.wallet, 500, &k2);
    let (status, body) = send(&app, signed(next_req(), &device, &sign(&key, &next))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "revoked device: {body}");
    assert_eq!(code(&body), "device_signature_invalid");
    let renewed = device_id(&app, &alice.token, &key).await;
    assert_ne!(renewed, device, "a revoked key comes back as a new device");
    let (status, body) = send(&app, signed(next_req(), &renewed, &sign(&key, &next))).await;
    assert_eq!(status, StatusCode::CREATED, "re-registered: {body}");
    assert_eq!(balance_of(&app, &alice.token, &alice.wallet).await, 98_500);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn required_binding_guards_fx() {
    let (app, pool) = app_with(cfg(DeviceBinding::Required)).await;
    let admin = admin_token(&app, &pool).await;
    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/admin/fx-rates",
            Some(&admin),
            None,
            json!({"base": "TJS", "quote": "USD", "rate_num": 1, "rate_den": 10}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "rate: {body}");
    let alice = party(&app, &pool, None).await;
    let usd = create_wallet_cur(&app, &alice.token, "USD").await;
    admin_deposit(&app, &admin, &alice.wallet, 100_000).await;
    let key = new_key();
    let device = device_id(&app, &alice.token, &key).await;
    let k = Uuid::new_v4().to_string();
    let fx = || {
        req(
            "POST",
            "/v1/fx",
            Some(&alice.token),
            Some(&k),
            json!({"from_account": alice.wallet, "to_account": usd, "amount_minor": 10_000}),
        )
    };
    let auth = |currency: &'static str, amount_minor: i64| PaymentAuth {
        kind: MoneyMove::Fx,
        user_id: u(&alice.id),
        idempotency_key: u(&k),
        from_account: Some(u(&alice.wallet)),
        to_account: u(&usd),
        amount_minor,
        currency,
        check_id: None,
    };

    let (status, body) = send(&app, fx()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "unsigned: {body}");
    assert_eq!(code(&body), "device_signature_required");
    for (what, bad) in [
        ("target currency", auth("USD", 10_000)),
        ("other amount", auth("TJS", 20_000)),
        (
            "signed as a transfer",
            PaymentAuth {
                kind: MoneyMove::Transfer,
                ..auth("TJS", 10_000)
            },
        ),
    ] {
        let (status, body) = send(&app, signed(fx(), &device, &sign(&key, &bad))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{what}: {body}");
        assert_eq!(code(&body), "device_signature_invalid", "{what}");
    }
    assert_eq!(balance_of(&app, &alice.token, &alice.wallet).await, 100_000);

    let (status, body) = send(
        &app,
        signed(fx(), &device, &sign(&key, &auth("TJS", 10_000))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "signed fx: {body}");
    assert_eq!(body["debited_minor"], 10_000);
    assert_eq!(balance_of(&app, &alice.token, &alice.wallet).await, 90_000);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn required_binding_guards_app_check_payments_not_terminal_ones() {
    let (app, pool) = app_with(cfg(DeviceBinding::Required)).await;
    let admin = admin_token(&app, &pool).await;
    let merchant = party(&app, &pool, Some("Shop Owner")).await;
    let payer = party(&app, &pool, Some("Firuza Karimova")).await;
    admin_deposit(&app, &admin, &payer.wallet, 50_000).await;
    let key = new_key();
    let device = device_id(&app, &payer.token, &key).await;

    let check_id = Uuid::new_v4().to_string();
    let (status, check) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        1_500,
        &check_id,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "check: {check}");
    let k = Uuid::new_v4().to_string();
    let pay = || {
        req(
            "POST",
            &format!("/v1/checks/{check_id}/pay"),
            Some(&payer.token),
            Some(&k),
            json!({"account": payer.wallet}),
        )
    };
    let good = PaymentAuth {
        kind: MoneyMove::Check,
        user_id: u(&payer.id),
        idempotency_key: u(&k),
        from_account: Some(u(&payer.wallet)),
        to_account: u(&merchant.wallet),
        amount_minor: 1_500,
        currency: "TJS",
        check_id: Some(u(&check_id)),
    };

    let (status, body) = send(&app, pay()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "unsigned: {body}");
    assert_eq!(code(&body), "device_signature_required");
    for (what, bad) in [
        (
            "a smaller amount",
            PaymentAuth {
                amount_minor: 1_000,
                ..good
            },
        ),
        (
            "another merchant wallet",
            PaymentAuth {
                to_account: u(&payer.wallet),
                ..good
            },
        ),
        (
            "another check",
            PaymentAuth {
                check_id: Some(Uuid::new_v4()),
                ..good
            },
        ),
        (
            "no payer wallet",
            PaymentAuth {
                from_account: None,
                ..good
            },
        ),
    ] {
        let (status, body) = send(&app, signed(pay(), &device, &sign(&key, &bad))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{what}: {body}");
        assert_eq!(code(&body), "device_signature_invalid", "{what}");
    }
    let (status, open) = get_check(&app, &payer.token, &check_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(open["status"], "open");

    let (status, paid) = send(&app, signed(pay(), &device, &sign(&key, &good))).await;
    assert_eq!(status, StatusCode::CREATED, "signed pay: {paid}");
    assert_eq!(paid["status"], "posted");
    let (status, replay) = send(&app, pay()).await;
    assert_eq!(status, StatusCode::CREATED, "unsigned replay: {replay}");
    assert_eq!(replay, paid);
    assert_eq!(balance_of(&app, &payer.token, &payer.wallet).await, 48_500);

    // Without an account in the body the server picks the wallet; the payer signed none.
    let second = Uuid::new_v4().to_string();
    let (status, _) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        700,
        &second,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let k2 = Uuid::new_v4().to_string();
    let auth = PaymentAuth {
        idempotency_key: u(&k2),
        from_account: None,
        amount_minor: 700,
        check_id: Some(u(&second)),
        ..good
    };
    let (status, body) = send(
        &app,
        signed(
            req(
                "POST",
                &format!("/v1/checks/{second}/pay"),
                Some(&payer.token),
                Some(&k2),
                json!({}),
            ),
            &device,
            &sign(&key, &auth),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "pay without account: {body}");

    // A fingerprint payment is authenticated by the merchant's terminal, not a payer device.
    let seed = Uuid::new_v4();
    let (status, body) = enroll(&app, &payer.token, 3, seed).await;
    assert_eq!(status, StatusCode::CREATED, "enroll: {body}");
    let third = Uuid::new_v4().to_string();
    let (status, _) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        900,
        &third,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) =
        pay_check(&app, &merchant, &third, seed, &Uuid::new_v4().to_string()).await;
    assert_eq!(status, StatusCode::CREATED, "terminal payment: {body}");
    assert_eq!(balance_of(&app, &payer.token, &payer.wallet).await, 46_900);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn optional_binding_verifies_only_what_is_sent_and_off_ignores_it() {
    let (app, pool) = app_with(cfg(DeviceBinding::Optional)).await;
    let admin = admin_token(&app, &pool).await;
    let alice = party(&app, &pool, None).await;
    let bob = party(&app, &pool, None).await;
    admin_deposit(&app, &admin, &alice.wallet, 10_000).await;
    let key = new_key();
    let device = device_id(&app, &alice.token, &key).await;

    let k1 = Uuid::new_v4().to_string();
    let (status, body) = send(
        &app,
        transfer_req(&alice.token, &alice.wallet, &bob.wallet, 100, &k1),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "unsigned under optional: {body}"
    );

    let k2 = Uuid::new_v4().to_string();
    let wrong = transfer_auth(&alice.id, &k2, &alice.wallet, &bob.wallet, 1);
    let (status, body) = send(
        &app,
        signed(
            transfer_req(&alice.token, &alice.wallet, &bob.wallet, 200, &k2),
            &device,
            &sign(&key, &wrong),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a bad signature is refused: {body}"
    );
    assert_eq!(code(&body), "device_signature_invalid");

    let good = transfer_auth(&alice.id, &k2, &alice.wallet, &bob.wallet, 200);
    let (status, body) = send(
        &app,
        signed(
            transfer_req(&alice.token, &alice.wallet, &bob.wallet, 200, &k2),
            &device,
            &sign(&key, &good),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "signed under optional: {body}");

    let off = router(pool.clone(), cfg(DeviceBinding::Off));
    let k3 = Uuid::new_v4().to_string();
    let (status, body) = send(
        &off,
        signed(
            transfer_req(&alice.token, &alice.wallet, &bob.wallet, 300, &k3),
            &device,
            "garbage",
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "headers ignored when off: {body}"
    );
    assert_eq!(balance_of(&app, &alice.token, &alice.wallet).await, 9_400);
}
