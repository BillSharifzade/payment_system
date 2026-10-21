mod common;

use api::DepositConfig;
use axum::http::StatusCode;
use common::*;
use serde_json::{json, Value};
use uuid::Uuid;

const MAX_MINOR: i64 = 1_000_000;

async fn dual_control_app() -> (axum::Router, sqlx::PgPool) {
    app_with(TestConfig {
        deposits: DepositConfig {
            dual_control: true,
            max_minor: MAX_MINOR,
        },
        ..TestConfig::default()
    })
    .await
}

async fn admin(app: &axum::Router, pool: &sqlx::PgPool) -> (String, String) {
    let (id, tok) = register(app).await;
    make_admin(pool, &id).await;
    (id, tok)
}

async fn request_deposit(
    app: &axum::Router,
    admin: &str,
    key: &str,
    account: &str,
    amount_minor: i64,
) -> (StatusCode, Value) {
    send(
        app,
        req(
            "POST",
            "/v1/deposits",
            Some(admin),
            Some(key),
            json!({"user_account": account, "amount_minor": amount_minor}),
        ),
    )
    .await
}

async fn decide(
    app: &axum::Router,
    admin: &str,
    id: &str,
    action: &str,
    body: Value,
) -> (StatusCode, Value) {
    send(
        app,
        req(
            "POST",
            &format!("/v1/admin/deposits/{id}/{action}"),
            Some(admin),
            None,
            body,
        ),
    )
    .await
}

async fn audit_count(pool: &sqlx::PgPool, admin_id: &str, action: &str, target: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM admin_actions WHERE admin_id = $1 AND action = $2 AND target = $3",
    )
    .bind(Uuid::parse_str(admin_id).unwrap())
    .bind(action)
    .bind(target)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn deposits_need_a_second_admin() {
    let (app, pool) = dual_control_app().await;
    let (customer_id, customer) = register(&app).await;
    let wallet = create_wallet(&app, &customer).await;
    let (maker_id, maker) = admin(&app, &pool).await;
    let (checker_id, checker) = admin(&app, &pool).await;
    let settlement_before = system_total(&pool, "system_settlement", "TJS").await;

    let key = Uuid::new_v4().to_string();
    let (status, d) = request_deposit(&app, &maker, &key, &wallet, 5_000).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{d}");
    assert_eq!(d["id"], key);
    assert_eq!(d["transaction_id"], key);
    assert_eq!(d["status"], "pending_approval");
    assert_eq!(d["user_account"], wallet);
    assert_eq!(d["amount_minor"], 5_000);
    assert_eq!(d["currency"], "TJS");
    assert_eq!(
        d["customer_phone"],
        json!(db_phone(&pool, &customer_id).await)
    );
    assert_eq!(d["requested_by"], maker_id);
    assert!(d["requested_at"].as_str().unwrap().ends_with('Z'), "{d}");
    assert!(d["decided_by"].is_null() && d["decided_at"].is_null() && d["reason"].is_null());

    let (status, again) = request_deposit(&app, &maker, &key, &wallet, 5_000).await;
    assert_eq!(status, StatusCode::ACCEPTED, "replay: {again}");
    assert_eq!(again, d);
    let (status, body) = request_deposit(&app, &maker, &key, &wallet, 6_000).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "idempotency_conflict");
    assert_eq!(balance_of(&app, &customer, &wallet).await, 0);

    let (status, body) = decide(&app, &maker, &key, "approve", Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "dual_control_required");
    let (status, _) = decide(&app, &customer, &key, "approve", Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-admin approve");
    assert_eq!(balance_of(&app, &customer, &wallet).await, 0);

    let (status, posted) = decide(&app, &checker, &key, "approve", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{posted}");
    assert_eq!(posted["status"], "posted");
    assert_eq!(posted["decided_by"], checker_id);
    assert!(posted["decided_at"].is_string());
    assert_eq!(balance_of(&app, &customer, &wallet).await, 5_000);
    assert_eq!(
        system_total(&pool, "system_settlement", "TJS").await - settlement_before,
        -5_000
    );

    let (status, body) = decide(&app, &checker, &key, "approve", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "approve is idempotent: {body}");
    assert_eq!(body["status"], "posted");
    let (status, body) = decide(&app, &maker, &key, "reject", json!({"reason": "late"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(balance_of(&app, &customer, &wallet).await, 5_000);
    let (status, body) = request_deposit(&app, &maker, &key, &wallet, 5_000).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "posted");

    let (status, got) = send(
        &app,
        req(
            "GET",
            &format!("/v1/admin/deposits/{key}"),
            Some(&checker),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, posted);
    let (status, list) = send(
        &app,
        req(
            "GET",
            "/v1/admin/deposits?status=posted&limit=200",
            Some(&maker),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert!(list["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["id"] == key));
    let (status, _) = send(
        &app,
        req(
            "GET",
            "/v1/admin/deposits",
            Some(&customer),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    assert_eq!(
        audit_count(&pool, &maker_id, "deposit.request", &wallet).await,
        1
    );
    assert_eq!(
        audit_count(&pool, &checker_id, "deposit.approve", &key).await,
        1
    );

    let (status, tx) = send(
        &app,
        req(
            "GET",
            &format!("/v1/transactions/{key}"),
            Some(&customer),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tx}");
    assert_eq!(tx["kind"], "deposit");
    assert_eq!(tx["entries"][0]["direction"], "credit");

    let (status, s) = send(
        &app,
        req("GET", "/v1/admin/status", Some(&maker), None, Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(s["deposit_dual_control"], true);
    assert_eq!(s["deposit_max_minor"], MAX_MINOR);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn deposit_rules_owners_limits_and_withdrawal() {
    let (app, pool) = dual_control_app().await;
    let (maker_id, maker) = admin(&app, &pool).await;
    let maker_wallet = create_wallet(&app, &maker).await;
    let (_owner_id, owner) = admin(&app, &pool).await;
    let owner_wallet = create_wallet(&app, &owner).await;
    let (_, checker) = admin(&app, &pool).await;

    let (status, body) = request_deposit(
        &app,
        &maker,
        &Uuid::new_v4().to_string(),
        &maker_wallet,
        100,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "own wallet: {body}");
    assert_eq!(body["error"]["code"], "forbidden");
    let (status, body) = request_deposit(
        &app,
        &maker,
        &Uuid::new_v4().to_string(),
        &owner_wallet,
        MAX_MINOR + 1,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "limit_exceeded");

    let key = Uuid::new_v4().to_string();
    let (status, body) = request_deposit(&app, &maker, &key, &owner_wallet, MAX_MINOR).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (status, body) = decide(&app, &owner, &key, "approve", Value::Null).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "wallet owner approving: {body}"
    );
    assert_eq!(body["error"]["code"], "dual_control_required");

    let (status, body) = decide(&app, &maker, &key, "reject", json!({"reason": "  "})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = send(
        &app,
        req(
            "POST",
            &format!("/v1/transactions/{key}/void"),
            Some(&maker),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a pending deposit is withdrawn, not voided: {body}"
    );

    let (status, rejected) = decide(
        &app,
        &maker,
        &key,
        "reject",
        json!({"reason": "wrong customer"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "requester withdraws: {rejected}");
    assert_eq!(rejected["status"], "rejected");
    assert_eq!(rejected["reason"], "wrong customer");
    assert_eq!(rejected["decided_by"], maker_id);
    let (status, body) = decide(&app, &checker, &key, "approve", Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "conflict");
    let (status, _) = decide(&app, &checker, &key, "reject", json!({"reason": "again"})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(balance_of(&app, &owner, &owner_wallet).await, 0);
    assert_eq!(
        audit_count(&pool, &maker_id, "deposit.reject", &key).await,
        1
    );

    let missing = Uuid::new_v4();
    let (status, _) = decide(&app, &checker, &missing.to_string(), "approve", Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = decide(
        &app,
        &checker,
        &missing.to_string(),
        "reject",
        json!({"reason": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &app,
        req(
            "GET",
            &format!("/v1/admin/deposits/{missing}"),
            Some(&checker),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &app,
        req(
            "GET",
            "/v1/admin/deposits?status=bogus",
            Some(&checker),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn concurrent_approvals_post_exactly_once() {
    let (app, pool) = dual_control_app().await;
    let (_, customer) = register(&app).await;
    let wallet = create_wallet(&app, &customer).await;
    let (_, maker) = admin(&app, &pool).await;
    let key = Uuid::new_v4().to_string();
    let (status, _) = request_deposit(&app, &maker, &key, &wallet, 7_777).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let mut checkers = Vec::new();
    for _ in 0..6 {
        checkers.push(admin(&app, &pool).await);
    }
    let mut handles = Vec::new();
    for (_, tok) in checkers {
        let (app, key) = (app.clone(), key.clone());
        handles.push(tokio::spawn(async move {
            decide(&app, &tok, &key, "approve", Value::Null).await
        }));
    }
    for h in handles {
        let (status, body) = h.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "posted");
    }
    assert_eq!(balance_of(&app, &customer, &wallet).await, 7_777);
    let approvals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM admin_actions WHERE action = 'deposit.approve' AND target = $1",
    )
    .bind(&key)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(approvals, 1);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn pending_queue_pages_oldest_first_and_is_counted() {
    let (app, pool) = dual_control_app().await;
    let (_, customer) = register(&app).await;
    let wallet = create_wallet(&app, &customer).await;
    let (_, maker) = admin(&app, &pool).await;
    let mut mine = Vec::new();
    for amount in [101, 102, 103] {
        let key = Uuid::new_v4().to_string();
        let (status, _) = request_deposit(&app, &maker, &key, &wallet, amount).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        mine.push(key);
    }
    let (_, metrics) = send(
        &app,
        req("GET", "/v1/admin/metrics", Some(&maker), None, Value::Null),
    )
    .await;
    assert!(
        metrics["pending_deposits"].as_i64().unwrap() >= 3,
        "{metrics}"
    );
    let (_, status) = send(
        &app,
        req("GET", "/v1/admin/status", Some(&maker), None, Value::Null),
    )
    .await;
    assert!(
        status["pending_deposits"].as_i64().unwrap() >= 3,
        "{status}"
    );

    let mut seen: Vec<(String, String)> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let uri = match &cursor {
            None => "/v1/admin/deposits?status=pending_approval&limit=2".to_string(),
            Some(c) => format!(
                "/v1/admin/deposits?status=pending_approval&limit=2&cursor={}",
                c.replace('+', "%2B")
                    .replace(' ', "%20")
                    .replace('|', "%7C")
            ),
        };
        let (status, page) = send(&app, req("GET", &uri, Some(&maker), None, Value::Null)).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        for d in page["items"].as_array().unwrap() {
            assert_eq!(d["status"], "pending_approval");
            seen.push((
                d["requested_at"].as_str().unwrap().to_string(),
                d["id"].as_str().unwrap().to_string(),
            ));
        }
        match page["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    let ids: Vec<&String> = seen.iter().map(|(_, id)| id).collect();
    for key in &mine {
        assert_eq!(ids.iter().filter(|id| **id == key).count(), 1, "{key} once");
    }
    assert!(seen.windows(2).all(|w| w[0] <= w[1]), "oldest first");
    for key in &mine {
        let (status, _) = decide(&app, &maker, key, "reject", json!({"reason": "cleanup"})).await;
        assert_eq!(status, StatusCode::OK);
    }
}

// A void can race the request's creation (both only insert); the approval then cannot post.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn a_deposit_whose_key_was_voided_is_never_approved() {
    let (app, pool) = dual_control_app().await;
    let (customer_id, customer) = register(&app).await;
    let wallet = create_wallet(&app, &customer).await;
    let (_, maker) = admin(&app, &pool).await;
    let (_, checker) = admin(&app, &pool).await;
    let key = Uuid::new_v4().to_string();
    let (status, _) = request_deposit(&app, &maker, &key, &wallet, 900).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = Uuid::parse_str(&key).unwrap();
    sqlx::query("INSERT INTO transactions (id) VALUES ($1)")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO voided_transactions (id, voided_by) VALUES ($1, $2)")
        .bind(id)
        .bind(Uuid::parse_str(&customer_id).unwrap())
        .execute(&pool)
        .await
        .unwrap();

    let (status, body) = decide(&app, &checker, &key, "approve", Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "voided");
    assert_eq!(balance_of(&app, &customer, &wallet).await, 0);
    let (_, d) = send(
        &app,
        req(
            "GET",
            &format!("/v1/admin/deposits/{key}"),
            Some(&checker),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(d["status"], "pending_approval");
    let (status, _) = decide(&app, &maker, &key, "reject", json!({"reason": "voided"})).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn admin_lookup_shows_the_verified_name_for_deposit_confirmation() {
    let (app, pool) = dual_control_app().await;
    let (_, maker) = admin(&app, &pool).await;
    let (named_id, _) = register(&app).await;
    let (plain_id, _) = register(&app).await;
    approve_named_kyc(&pool, &named_id, "Rustam Emomali").await;
    for (id, want) in [
        (&named_id, json!("Rustam Emomali")),
        (&plain_id, Value::Null),
    ] {
        let phone = db_phone(&pool, id).await;
        let uri = format!("/v1/admin/users?phone={phone}");
        let (status, user) = send(&app, req("GET", &uri, Some(&maker), None, Value::Null)).await;
        assert_eq!(status, StatusCode::OK, "{user}");
        assert_eq!(user["full_name"], want);
    }
}
