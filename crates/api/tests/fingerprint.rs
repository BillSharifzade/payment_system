mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use api::{BiometricConfig, MatcherBackend};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{post, put};
use axum::{Json, Router};
use base64::Engine;
use biometric::HttpMatcher;
use common::*;
use serde_json::{json, Value};
use uuid::Uuid;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
const FINGER_BYTES: usize = 48;

// A finger is the first 48 bytes; every capture adds 16 bytes of scanner noise, so two
// captures of one finger match but are never byte-identical (like a real scanner).
fn capture(finger: Uuid) -> String {
    let mut bytes: Vec<u8> = finger
        .as_bytes()
        .iter()
        .cycle()
        .take(FINGER_BYTES)
        .copied()
        .collect();
    bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    B64.encode(bytes)
}

#[derive(Default)]
struct Sidecar {
    gallery: Mutex<HashMap<Uuid, Vec<u8>>>,
    put_delay: Mutex<Duration>,
    put_seen: tokio::sync::Notify,
}

type Shared = Arc<Sidecar>;

fn decode(body: &Value) -> Vec<u8> {
    B64.decode(body["template"].as_str().unwrap()).unwrap()
}

fn score(a: &[u8], b: &[u8]) -> f64 {
    if a[..FINGER_BYTES] == b[..FINGER_BYTES] {
        95.0
    } else {
        5.0
    }
}

async fn put_template(
    State(s): State<Shared>,
    Path(id): Path<Uuid>,
    Json(body): Json<Value>,
) -> StatusCode {
    s.gallery.lock().unwrap().insert(id, decode(&body));
    s.put_seen.notify_one();
    let delay = *s.put_delay.lock().unwrap();
    tokio::time::sleep(delay).await;
    StatusCode::NO_CONTENT
}

async fn delete_template(State(s): State<Shared>, Path(id): Path<Uuid>) -> StatusCode {
    match s.gallery.lock().unwrap().remove(&id) {
        Some(_) => StatusCode::NO_CONTENT,
        None => StatusCode::NOT_FOUND,
    }
}

async fn identify(State(s): State<Shared>, Json(body): Json<Value>) -> Json<Value> {
    let probe = decode(&body);
    let limit = body["limit"].as_u64().unwrap() as usize;
    let mut hits: Vec<(Uuid, f64)> = s
        .gallery
        .lock()
        .unwrap()
        .iter()
        .map(|(id, t)| (*id, score(t, &probe)))
        .collect();
    hits.sort_by(|a, b| b.1.total_cmp(&a.1));
    hits.truncate(limit);
    let hits: Vec<Value> = hits
        .into_iter()
        .map(|(id, score)| json!({"enrollment_id": id, "score": score}))
        .collect();
    Json(json!({ "hits": hits }))
}

async fn verify(State(s): State<Shared>, Json(body): Json<Value>) -> Json<Value> {
    let probe = decode(&body);
    let ids: Vec<Uuid> = serde_json::from_value(body["enrollment_ids"].clone()).unwrap();
    let gallery = s.gallery.lock().unwrap();
    let hits: Vec<Value> = ids
        .iter()
        .filter_map(|id| gallery.get(id).map(|t| (id, score(t, &probe))))
        .map(|(id, score)| json!({"enrollment_id": id, "score": score}))
        .collect();
    Json(json!({ "hits": hits }))
}

async fn spawn_sidecar() -> (String, Shared) {
    let shared: Shared = Arc::default();
    let app = Router::new()
        .route(
            "/v1/templates/{id}",
            put(put_template).delete(delete_template),
        )
        .route("/v1/identify", post(identify))
        .route("/v1/verify", post(verify))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), shared)
}

fn http_config(url: &str, identify_scale: f64) -> BiometricConfig {
    let m = HttpMatcher::new(url, Duration::from_secs(10)).unwrap();
    BiometricConfig {
        matcher: MatcherBackend::Http(Arc::new(m)),
        identify: true,
        identify_scale,
        ..BiometricConfig::dev()
    }
}

fn with_biometric(biometric: BiometricConfig) -> TestConfig {
    TestConfig {
        biometric,
        ..TestConfig::default()
    }
}

async fn enroll_capture(
    app: &axum::Router,
    token: &str,
    finger: i16,
    template: &str,
) -> (StatusCode, Value) {
    send(
        app,
        req(
            "POST",
            "/v1/biometric/fingerprints",
            Some(token),
            None,
            json!({"finger": finger, "format": "raw", "template": template, "consent": true}),
        ),
    )
    .await
}

async fn pay_with(
    app: &axum::Router,
    merchant: &Party,
    check_id: &str,
    template: &str,
    payer_phone: Option<&str>,
) -> (StatusCode, Value) {
    let mut body = json!({"format": "raw", "template": template});
    if let Some(p) = payer_phone {
        body["payer_phone"] = json!(p);
    }
    let key = Uuid::new_v4().to_string();
    send(app, probe_request(merchant, check_id, &key, body)).await
}

async fn new_check(app: &axum::Router, merchant: &Party, amount_minor: i64) -> String {
    let key = Uuid::new_v4().to_string();
    let (status, body) = create_check(
        app,
        &merchant.token,
        &merchant.wallet,
        amount_minor,
        &key,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    key
}

// (status, failed attempts); every finished attempt must have given its slot back.
async fn check_counter(pool: &sqlx::PgPool, check_id: &str) -> (String, i32) {
    let (status, failed, in_flight): (String, i32, i32) = sqlx::query_as(
        "SELECT status, failed_attempts, attempts_in_flight FROM checks WHERE id = $1",
    )
    .bind(Uuid::parse_str(check_id).unwrap())
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(in_flight, 0, "a finished attempt kept its slot");
    (status, failed)
}

async fn events(pool: &sqlx::PgPool, check_id: &str, outcome: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM biometric_events WHERE check_id = $1 AND outcome = $2")
        .bind(Uuid::parse_str(check_id).unwrap())
        .bind(outcome)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn terminals_are_admin_issued_shown_once_and_revocable() {
    let (app, pool) = app().await;
    let admin = admin_token(&app, &pool).await;
    let merchant = party(&app, &pool, None).await;
    let other_merchant = party(&app, &pool, None).await;
    let customer = party(&app, &pool, Some("Firuza Karimova")).await;
    admin_deposit(&app, &admin, &customer.wallet, 50_000).await;
    let seed = Uuid::new_v4();
    assert_eq!(
        enroll(&app, &customer.token, 4, seed).await.0,
        StatusCode::CREATED
    );

    let create = |tok: &str, merchant_id: &str, label: &str| {
        req(
            "POST",
            "/v1/admin/terminals",
            Some(tok),
            None,
            json!({"merchant_user_id": merchant_id, "label": label}),
        )
    };
    assert_eq!(
        send(&app, create(&merchant.token, &merchant.id, "x"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, _) = send(&app, create(&admin, &Uuid::new_v4().to_string(), "x")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&app, create(&admin, &merchant.id, "   ")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, t) = send(&app, create(&admin, &merchant.id, " Till 1 ")).await;
    assert_eq!(status, StatusCode::CREATED, "{t}");
    let terminal_id = t["id"].as_str().unwrap().to_string();
    let api_key = t["api_key"].as_str().unwrap().to_string();
    assert_eq!(api_key.len(), 64);
    assert!(api_key.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(t["label"], "Till 1");
    assert_eq!(t["merchant_user_id"], merchant.id);
    assert!(t["revoked_at"].is_null() && t["last_used_at"].is_null());
    let stored: Vec<u8> = sqlx::query_scalar("SELECT key_hash FROM terminals WHERE id = $1")
        .bind(Uuid::parse_str(&terminal_id).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        stored,
        biometric::hash_terminal_key(&api_key).to_vec(),
        "only the hash is stored"
    );

    let list_uri = format!("/v1/admin/terminals?merchant_user_id={}", merchant.id);
    let (status, list) = send(&app, req("GET", &list_uri, Some(&admin), None, Value::Null)).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let items = list["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        2,
        "the party's own terminal and the new one: {list}"
    );
    assert!(items.iter().all(|i| i.get("api_key").is_none()));

    let till = Party {
        terminal: api_key.clone(),
        ..merchant.clone()
    };
    let check = new_check(&app, &merchant, 20_000).await;
    let template = template_b64(seed);
    let bare = req(
        "POST",
        &format!("/v1/checks/{check}/pay/fingerprint"),
        Some(&merchant.token),
        Some(&Uuid::new_v4().to_string()),
        json!({"format": "raw", "template": template}),
    );
    for (who, request) in [
        ("no key", bare),
        (
            "unknown key",
            probe_request(
                &Party {
                    terminal: "f".repeat(64),
                    ..merchant.clone()
                },
                &check,
                &Uuid::new_v4().to_string(),
                json!({"format": "raw", "template": template}),
            ),
        ),
        (
            "another merchant's terminal",
            probe_request(
                &Party {
                    terminal: other_merchant.terminal.clone(),
                    ..merchant.clone()
                },
                &check,
                &Uuid::new_v4().to_string(),
                json!({"format": "raw", "template": template}),
            ),
        ),
    ] {
        let (status, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{who}: {body}");
        assert_eq!(body["error"]["code"], "terminal_unauthorized");
    }
    assert_eq!(check_counter(&pool, &check).await, ("open".to_string(), 0));

    let (status, paid) = pay_check(&app, &till, &check, seed, &Uuid::new_v4().to_string()).await;
    assert_eq!(status, StatusCode::CREATED, "{paid}");
    assert_eq!(paid["payer_name"], "Firuza K.");
    let (_, viewed) = get_check(&app, &merchant.token, &check).await;
    assert_eq!(viewed["payer_name"], "Firuza K.");
    let event_terminal: Option<Uuid> = sqlx::query_scalar(
        "SELECT terminal_id FROM biometric_events WHERE check_id = $1 AND outcome = 'paid'",
    )
    .bind(Uuid::parse_str(&check).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(event_terminal.unwrap().to_string(), terminal_id);
    let (_, list) = send(&app, req("GET", &list_uri, Some(&admin), None, Value::Null)).await;
    let used = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == terminal_id.as_str())
        .unwrap();
    assert!(used["last_used_at"].is_string(), "{used}");

    let revoke_uri = format!("/v1/admin/terminals/{terminal_id}/revoke");
    let (status, revoked) = send(
        &app,
        req("POST", &revoke_uri, Some(&admin), None, Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert!(revoked["revoked_at"].is_string());
    assert!(revoked.get("api_key").is_none());
    let (status, again) = send(
        &app,
        req("POST", &revoke_uri, Some(&admin), None, Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["revoked_at"], revoked["revoked_at"]);
    let (status, _) = send(
        &app,
        req(
            "POST",
            &format!("/v1/admin/terminals/{}/revoke", Uuid::new_v4()),
            Some(&admin),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let check2 = new_check(&app, &merchant, 1_000).await;
    let (status, body) = pay_check(&app, &till, &check2, seed, &Uuid::new_v4().to_string()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "revoked terminal: {body}");
    let (status, body) =
        pay_check(&app, &merchant, &check2, seed, &Uuid::new_v4().to_string()).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "the merchant's other terminal still works: {body}"
    );

    let audited: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM admin_actions
         WHERE target = $1 AND action IN ('terminal.create', 'terminal.revoke')",
    )
    .bind(&terminal_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audited, 2);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn one_to_one_verification_by_payer_phone() {
    let (app, pool) = app_with(with_biometric(BiometricConfig::dev())).await;
    let admin = admin_token(&app, &pool).await;
    let merchant = party(&app, &pool, None).await;
    let alice = party(&app, &pool, Some("Bilal Sharifzade")).await;
    let bob = party(&app, &pool, None).await;
    admin_deposit(&app, &admin, &alice.wallet, 10_000).await;
    let (alice_seed, bob_seed) = (Uuid::new_v4(), Uuid::new_v4());
    assert_eq!(
        enroll(&app, &alice.token, 1, alice_seed).await.0,
        StatusCode::CREATED
    );
    assert_eq!(
        enroll(&app, &bob.token, 1, bob_seed).await.0,
        StatusCode::CREATED
    );
    let (alice_phone, bob_phone) = (
        db_phone(&pool, &alice.id).await,
        db_phone(&pool, &bob.id).await,
    );
    let (_, config) = send(
        &app,
        req(
            "GET",
            "/v1/config",
            Some(&merchant.token),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(config["biometric_identify"], false);

    let check = new_check(&app, &merchant, 5_000).await;
    let alice_finger = template_b64(alice_seed);
    let (status, body) = pay_with(&app, &merchant, &check, &alice_finger, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "1:N is off: {body}");
    let (status, _) = pay_with(&app, &merchant, &check, &alice_finger, Some("12")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = pay_with(&app, &merchant, &check, &alice_finger, Some(&bob_phone)).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "alice's finger is not bob's: {body}"
    );
    assert_eq!(body["error"]["code"], "no_match");
    let unknown = format!("99{:012}", Uuid::new_v4().as_u128() % 1_000_000_000_000);
    let (status, body) = pay_with(&app, &merchant, &check, &alice_finger, Some(&unknown)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(balance_of(&app, &alice.token, &alice.wallet).await, 10_000);

    let typed = format!("+{} {}", &alice_phone[..3], &alice_phone[3..]);
    let (status, paid) = pay_with(&app, &merchant, &check, &alice_finger, Some(&typed)).await;
    assert_eq!(status, StatusCode::CREATED, "{paid}");
    assert_eq!(paid["payer_name"], "Bilal S.");
    assert_eq!(balance_of(&app, &alice.token, &alice.wallet).await, 5_000);
    assert_eq!(check_counter(&pool, &check).await, ("paid".to_string(), 2));
    assert_eq!(events(&pool, &check, "no_match").await, 2);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn failed_attempts_lock_the_check_exactly() {
    let (app, pool) = app_with(with_biometric(BiometricConfig {
        identify: true,
        max_attempts: 3,
        ..BiometricConfig::dev()
    }))
    .await;
    let admin = admin_token(&app, &pool).await;
    let merchant = party(&app, &pool, None).await;
    let customer = party(&app, &pool, None).await;
    let broke = party(&app, &pool, None).await;
    admin_deposit(&app, &admin, &customer.wallet, 10_000).await;
    let (seed, broke_seed) = (Uuid::new_v4(), Uuid::new_v4());
    assert_eq!(
        enroll(&app, &customer.token, 6, seed).await.0,
        StatusCode::CREATED
    );
    assert_eq!(
        enroll(&app, &broke.token, 6, broke_seed).await.0,
        StatusCode::CREATED
    );

    let check = new_check(&app, &merchant, 1_000).await;
    for _ in 0..2 {
        let (status, body) = pay_check(
            &app,
            &merchant,
            &check,
            Uuid::new_v4(),
            &Uuid::new_v4().to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }
    let (status, body) = pay_check(
        &app,
        &merchant,
        &check,
        Uuid::new_v4(),
        &Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "third failure: {body}");
    assert_eq!(body["error"]["code"], "check_locked");
    let (_, viewed) = get_check(&app, &merchant.token, &check).await;
    assert_eq!(viewed["status"], "cancelled");
    let (status, body) =
        pay_check(&app, &merchant, &check, seed, &Uuid::new_v4().to_string()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "check_locked");
    assert_eq!(events(&pool, &check, "no_match").await, 3);
    assert_eq!(
        balance_of(&app, &customer.token, &customer.wallet).await,
        10_000
    );

    // Slots are reserved before the matcher runs: 8 simultaneous probes get exactly 3
    // evaluations; the rest are told another attempt is in progress, or that the check is locked.
    let raced = new_check(&app, &merchant, 1_000).await;
    let mut handles = Vec::new();
    for _ in 0..8 {
        let (app, m, check) = (app.clone(), merchant.clone(), raced.clone());
        handles.push(tokio::spawn(async move {
            pay_check(
                &app,
                &m,
                &check,
                Uuid::new_v4(),
                &Uuid::new_v4().to_string(),
            )
            .await
        }));
    }
    for h in handles {
        let (status, body) = h.await.unwrap();
        let code = body["error"]["code"].as_str().unwrap().to_string();
        assert!(
            matches!(
                (status, code.as_str()),
                (StatusCode::NOT_FOUND, "no_match")
                    | (StatusCode::CONFLICT, "check_locked")
                    | (StatusCode::CONFLICT, "conflict")
            ),
            "{status}: {body}"
        );
    }
    assert_eq!(events(&pool, &raced, "no_match").await, 3);
    assert_eq!(
        check_counter(&pool, &raced).await,
        ("cancelled".to_string(), 3)
    );

    // An identified payer who is refused (no funds) does not use up the check's attempts.
    let open = new_check(&app, &merchant, 1_000).await;
    for _ in 0..4 {
        let (status, body) = pay_check(
            &app,
            &merchant,
            &open,
            broke_seed,
            &Uuid::new_v4().to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"]["code"], "insufficient_funds");
    }
    assert_eq!(check_counter(&pool, &open).await, ("open".to_string(), 0));
    let (status, body) = pay_check(&app, &merchant, &open, seed, &Uuid::new_v4().to_string()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn http_matcher_verifies_refuses_replays_and_scales_identification() {
    let (url, sidecar) = spawn_sidecar().await;
    let pool = migrated_pool().await;
    let strict = router(pool.clone(), with_biometric(http_config(&url, 1_000.0)));
    let unscaled = router(pool.clone(), with_biometric(http_config(&url, 0.0)));
    let dead = router(
        pool.clone(),
        with_biometric(http_config("http://127.0.0.1:1", 0.0)),
    );
    let admin = admin_token(&strict, &pool).await;
    let merchant = party(&strict, &pool, None).await;
    let customer = party(&strict, &pool, Some("Zarina Davlatova")).await;
    let other = party(&strict, &pool, None).await;
    admin_deposit(&strict, &admin, &customer.wallet, 50_000).await;
    let (finger, other_finger) = (Uuid::new_v4(), Uuid::new_v4());
    let enrolment = capture(finger);
    let (status, e) = enroll_capture(&strict, &customer.token, 2, &enrolment).await;
    assert_eq!(status, StatusCode::CREATED, "{e}");
    let enrollment_id = Uuid::parse_str(e["id"].as_str().unwrap()).unwrap();
    assert!(sidecar.gallery.lock().unwrap().contains_key(&enrollment_id));
    sidecar.gallery.lock().unwrap().remove(&enrollment_id);
    let (status, _) = enroll_capture(&strict, &customer.token, 2, &enrolment).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a retried enrolment re-pushes the template"
    );
    assert!(sidecar.gallery.lock().unwrap().contains_key(&enrollment_id));
    let (status, _) = enroll_capture(&strict, &other.token, 2, &capture(other_finger)).await;
    assert_eq!(status, StatusCode::CREATED);
    let (customer_phone, other_phone) = (
        db_phone(&pool, &customer.id).await,
        db_phone(&pool, &other.id).await,
    );

    let first = new_check(&strict, &merchant, 20_000).await;
    let probe = capture(finger);
    let (status, body) = pay_with(
        &strict,
        &merchant,
        &first,
        &capture(finger),
        Some(&other_phone),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "1:1 against the wrong person: {body}"
    );
    let (status, paid) = pay_with(&strict, &merchant, &first, &probe, Some(&customer_phone)).await;
    assert_eq!(status, StatusCode::CREATED, "1:1 verify: {paid}");
    assert_eq!(paid["payer_name"], "Zarina D.");

    let second = new_check(&strict, &merchant, 10_000).await;
    let (status, body) = pay_with(&strict, &merchant, &second, &probe, Some(&customer_phone)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "probe_replayed");
    assert_eq!(events(&pool, &second, "replayed").await, 1);
    let (status, body) = pay_with(&strict, &merchant, &second, &capture(finger), None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "1:N against a gallery of >= 2 at scale 1000: {body}"
    );
    let (status, body) = pay_with(
        &dead,
        &merchant,
        &second,
        &capture(finger),
        Some(&customer_phone),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "retry_later");
    assert_eq!(
        check_counter(&pool, &second).await,
        ("open".to_string(), 2),
        "matcher outage costs no attempt"
    );
    let (status, body) = pay_with(&unscaled, &merchant, &second, &capture(finger), None).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "1:N at the 1:1 threshold: {body}"
    );
    assert_eq!(check_counter(&pool, &second).await, ("paid".to_string(), 2));
    assert_eq!(
        balance_of(&strict, &customer.token, &customer.wallet).await,
        20_000
    );

    let (status, _) = send(
        &strict,
        req(
            "DELETE",
            &format!("/v1/biometric/fingerprints/{enrollment_id}"),
            Some(&customer.token),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!sidecar.gallery.lock().unwrap().contains_key(&enrollment_id));
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn a_slow_matcher_during_enrolment_does_not_stall_payments() {
    let (url, sidecar) = spawn_sidecar().await;
    *sidecar.put_delay.lock().unwrap() = Duration::from_secs(3);
    let (app, pool) = app_with(with_biometric(http_config(&url, 0.0))).await;
    let admin = admin_token(&app, &pool).await;
    let alice = party(&app, &pool, None).await;
    let bob = party(&app, &pool, None).await;
    admin_deposit(&app, &admin, &alice.wallet, 5_000).await;

    let enrolling = {
        let (app, tok) = (app.clone(), alice.token.clone());
        tokio::spawn(async move { enroll_capture(&app, &tok, 9, &capture(Uuid::new_v4())).await })
    };
    tokio::time::timeout(Duration::from_secs(5), sidecar.put_seen.notified())
        .await
        .expect("enrolment reached the matcher");
    let started = std::time::Instant::now();
    let key = Uuid::new_v4().to_string();
    let (status, body) =
        transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 1_000, &key).await;
    let took = started.elapsed();
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(
        took < Duration::from_millis(1_500),
        "transfer waited {took:?} on the enrolment"
    );
    let (status, body) = enrolling.await.unwrap();
    assert_eq!(status, StatusCode::CREATED, "{body}");
}
