//! Failures between the steps of the TigerBeetle posting protocol, through the API: the
//! in-process cluster model drops or loses exactly the request under test. Runs in every lane
//! (it builds its own backend) in a scratch database: its books live in a model that dies with
//! the test, so they must not join a database other suites reconcile.

mod common;

use std::sync::Arc;

use api::Ledger;
use axum::http::StatusCode;
use common::*;
use ledger::AccountId;
use ledger_tigerbeetle::testkit::recover_all;
use ledger_tigerbeetle::{AnyTb, Fault, HybridLedger, Op, SimTb, TbClient, TbConfig, TbLedger};
use sqlx::postgres::PgPoolOptions;
use storage::PostgresLedger;
use uuid::Uuid;

/// (posted, available) of a wallet, read from the cluster.
async fn held(ledger: &HybridLedger<AnyTb>, wallet: &str) -> (i128, i128) {
    let id = AccountId(Uuid::parse_str(wallet).unwrap());
    let b = ledger.tb().balances(&[id]).await.unwrap()[0];
    (b.posted.minor_units(), b.available.minor_units())
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn lost_tigerbeetle_requests_settle_exactly_once() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let name = format!("api_tb_faults_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin_pool)
        .await
        .unwrap();
    let (base, _) = url.rsplit_once('/').expect("DATABASE_URL ends in /dbname");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&format!("{base}/{name}"))
        .await
        .unwrap();
    let pg = PostgresLedger::new(pool.clone());
    pg.migrate().await.unwrap();

    let sim = Arc::new(SimTb::new());
    let tb = AnyTb::Sim(sim.clone());
    let cfg = TbConfig::new(tb.cluster_id(), "test");
    let ledger = HybridLedger::new(TbLedger::new(tb, cfg), pg);
    ledger.open_system_accounts().await.unwrap();
    let app = router_on(Ledger::TigerBeetle(ledger.clone()), TestConfig::default());

    let admin = admin_token(&app, &pool).await;
    let alice = party(&app, &pool, None).await;
    let bob = party(&app, &pool, None).await;
    admin_deposit(&app, &admin, &alice.wallet, 10_000).await;

    // 1. The post after COMMIT is lost: the payment stands (201, in the statement), the
    //    cluster still holds it as a reservation until recovery posts it, exactly once.
    sim.inject(Fault::Pass(Op::CreateTransfers));
    sim.inject(Fault::Drop(Op::CreateTransfers));
    let key = Uuid::new_v4().to_string();
    let (status, body) =
        transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 1_000, &key).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(held(&ledger, &alice.wallet).await, (10_000, 9_000));
    assert_eq!(held(&ledger, &bob.wallet).await, (0, 0));
    let (status, replay) =
        transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 1_000, &key).await;
    assert_eq!(status, StatusCode::CREATED, "the stored response: {replay}");
    assert_eq!(replay, body);
    assert_eq!(recover_all(&ledger).await.posted, 1);
    assert_eq!(balance_of(&app, &alice.token, &alice.wallet).await, 9_000);
    assert_eq!(balance_of(&app, &bob.token, &bob.wallet).await, 1_000);
    assert_eq!(held(&ledger, &alice.wallet).await, (9_000, 9_000));

    // 2. The reservation's reply is lost: the request cannot know what the cluster did, so it
    //    answers 503 retry_later; the retry with the same key posts once, and recovery voids
    //    the orphaned reservation (no commit was ever recorded for it).
    sim.inject(Fault::LoseReply(Op::CreateTransfers));
    let key = Uuid::new_v4().to_string();
    let (status, body) = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 500, &key).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "retry_later");
    assert_eq!(held(&ledger, &alice.wallet).await, (9_000, 8_500));
    let (status, body) = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 500, &key).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(recover_all(&ledger).await.voided, 1);
    assert_eq!(held(&ledger, &alice.wallet).await, (8_500, 8_500));
    assert_eq!(balance_of(&app, &bob.token, &bob.wallet).await, 1_500);

    // The cluster equals the journal for every account the test touched.
    let ids: Vec<AccountId> = sqlx::query_scalar("SELECT id FROM accounts")
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(AccountId)
        .collect();
    assert_eq!(ledger.reconcile(&ids).await.unwrap(), vec![]);

    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {name} WITH (FORCE)"))
        .execute(&admin_pool)
        .await
        .unwrap();
}
