//! External checkpoint signers. A mock Vault exercises retries, re-login,
//! timeouts and the response checks everywhere; the ignored tests run against
//! a real `vault server -dev` (VAULT_ADDR + an admin VAULT_TOKEN), set up by
//! deploy/vault/bootstrap.sh, and check that its AppRole can sign and nothing
//! else.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use base64::Engine as _;
use crypto::{Hash, Sealer, TrustedKeys};
use serde_json::{json, Value};
use workers::env::Env;
use workers::signer::{
    AnySigner, CheckpointSigner, SignerConfig, SignerError, VaultAuth, VaultConfig, VaultSigner,
};
use workers::{seal_all, verify_chain, WorkerError};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

// ---- a mock Vault -------------------------------------------------------------

struct MockState {
    key: Sealer,
    token: String,
    logins: u32,
    fail_next: u32,
    deny_next: u32,
    delay: Duration,
    sign_version: u64,
    key_type: &'static str,
    foreign_signature: bool,
}

type Mock = Arc<Mutex<MockState>>;

fn authorized(state: &MockState, headers: &HeaderMap) -> bool {
    headers.get("x-vault-token").and_then(|v| v.to_str().ok()) == Some(state.token.as_str())
}

fn denied() -> axum::response::Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({"errors": ["permission denied"]})),
    )
        .into_response()
}

async fn mock_vault() -> (String, Mock) {
    let state = Arc::new(Mutex::new(MockState {
        key: Sealer::from_secret_bytes(&[0x5e; 32]),
        token: String::new(),
        logins: 0,
        fail_next: 0,
        deny_next: 0,
        delay: Duration::ZERO,
        sign_version: 1,
        key_type: "ed25519",
        foreign_signature: false,
    }));
    let router = axum::Router::new()
        .route(
            "/v1/auth/approle/login",
            post(|State(m): State<Mock>, Json(body): Json<Value>| async move {
                let mut m = m.lock().unwrap();
                if body["role_id"] != "role" || body["secret_id"] != "secret" {
                    return (StatusCode::BAD_REQUEST, Json(json!({"errors": ["invalid role or secret ID"]})))
                        .into_response();
                }
                m.logins += 1;
                m.token = format!("token-{}", m.logins);
                Json(json!({"auth": {"client_token": m.token, "lease_duration": 3600}})).into_response()
            }),
        )
        .route(
            "/v1/transit/keys/checkpoint-signing",
            get(|State(m): State<Mock>, headers: HeaderMap| async move {
                let m = m.lock().unwrap();
                if !authorized(&m, &headers) {
                    return denied();
                }
                let public_key = B64.encode(m.key.public_key_bytes());
                Json(json!({"data": {
                    "type": m.key_type, "latest_version": 1, "exportable": false,
                    "keys": {"1": {"public_key": public_key}},
                }}))
                .into_response()
            }),
        )
        .route(
            "/v1/transit/sign/checkpoint-signing",
            post(|State(m): State<Mock>, headers: HeaderMap, Json(body): Json<Value>| async move {
                let delay = {
                    let mut m = m.lock().unwrap();
                    if m.fail_next > 0 {
                        m.fail_next -= 1;
                        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"errors": ["Vault is sealed"]})))
                            .into_response();
                    }
                    if m.deny_next > 0 {
                        m.deny_next -= 1;
                        m.token = "revoked".into();
                    }
                    if !authorized(&m, &headers) {
                        return denied();
                    }
                    m.delay
                };
                tokio::time::sleep(delay).await;
                let m = m.lock().unwrap();
                assert_eq!(body["key_version"], 1, "the worker pins the version");
                let input: [u8; 32] = B64.decode(body["input"].as_str().unwrap()).unwrap().try_into().unwrap();
                let signer = if m.foreign_signature { Sealer::generate() } else { Sealer::from_secret_bytes(&[0x5e; 32]) };
                let sig = signer.sign_hash(&Hash::from_bytes(input));
                Json(json!({"data": {"signature": format!("vault:v{}:{}", m.sign_version, B64.encode(sig))}}))
                    .into_response()
            }),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, state)
}

fn config(addr: &str, auth: VaultAuth) -> VaultConfig {
    VaultConfig {
        addr: addr.to_string(),
        transit_mount: "transit".into(),
        key_name: "checkpoint-signing".into(),
        auth,
        ca_cert: None,
        timeout: Duration::from_secs(2),
        retries: 2,
    }
}

fn approle() -> VaultAuth {
    VaultAuth::AppRole {
        mount: "approle".into(),
        role_id: "role".into(),
        secret_id: "secret".into(),
    }
}

async fn signs(signer: &VaultSigner) -> Result<(), SignerError> {
    let hash = crypto::sha256(b"checkpoint");
    let sig = signer.sign(&hash).await?;
    crypto::verify_hash(&signer.public_key(), &hash, &sig).expect("verifies under the Vault key");
    Ok(())
}

#[tokio::test]
async fn vault_signer_retries_relogs_in_and_checks_what_vault_returns() {
    let (url, mock) = mock_vault().await;
    let signer = VaultSigner::connect(config(&url, approle())).await.unwrap();
    assert_eq!(
        signer.public_key(),
        Sealer::from_secret_bytes(&[0x5e; 32]).public_key_bytes()
    );
    signs(&signer).await.unwrap();
    assert_eq!(mock.lock().unwrap().logins, 1);

    // A sealed/standby Vault (503) is retried, up to VAULT_RETRIES.
    mock.lock().unwrap().fail_next = 2;
    signs(&signer).await.unwrap();
    mock.lock().unwrap().fail_next = 3;
    let err = signs(&signer).await.unwrap_err();
    assert!(
        matches!(err, SignerError::Unavailable(ref m) if m.contains("503")),
        "{err}"
    );
    mock.lock().unwrap().fail_next = 0;

    // A revoked or expired token (403): one fresh AppRole login, then success.
    mock.lock().unwrap().deny_next = 1;
    signs(&signer).await.unwrap();
    assert_eq!(mock.lock().unwrap().logins, 2);

    // A signature by another key version is refused.
    mock.lock().unwrap().sign_version = 2;
    assert!(
        matches!(signs(&signer).await, Err(SignerError::Rejected(m)) if m.contains("key version 2"))
    );
    mock.lock().unwrap().sign_version = 1;

    // Too slow: each attempt times out; the whole call stays bounded.
    mock.lock().unwrap().delay = Duration::from_millis(1_500);
    let mut cfg = config(&url, approle());
    cfg.timeout = Duration::from_millis(200);
    cfg.retries = 1;
    mock.lock().unwrap().delay = Duration::ZERO;
    let slow = VaultSigner::connect(cfg).await.unwrap();
    mock.lock().unwrap().delay = Duration::from_millis(1_500);
    let started = Instant::now();
    assert!(matches!(
        signs(&slow).await,
        Err(SignerError::Unavailable(_))
    ));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    mock.lock().unwrap().delay = Duration::ZERO;
}

#[tokio::test]
async fn vault_signer_refuses_misconfiguration_at_connect() {
    let (url, mock) = mock_vault().await;
    let bad_role = VaultAuth::AppRole {
        mount: "approle".into(),
        role_id: "role".into(),
        secret_id: "wrong".into(),
    };
    assert!(
        matches!(VaultSigner::connect(config(&url, bad_role)).await, Err(SignerError::Rejected(m)) if m.contains("400"))
    );
    assert!(matches!(
        VaultSigner::connect(config(&url, VaultAuth::Token("nope".into()))).await,
        Err(SignerError::Rejected(m)) if m.contains("403")
    ));
    mock.lock().unwrap().key_type = "ecdsa-p256";
    assert!(
        matches!(VaultSigner::connect(config(&url, approle())).await, Err(SignerError::Rejected(m)) if m.contains("not ed25519"))
    );
    mock.lock().unwrap().key_type = "ed25519";
    // The self-test signature catches a key that is not the one advertised.
    mock.lock().unwrap().foreign_signature = true;
    assert!(
        matches!(VaultSigner::connect(config(&url, approle())).await, Err(SignerError::Rejected(m)) if m.contains("test signature"))
    );
    mock.lock().unwrap().foreign_signature = false;
    let mut cfg = config("http://127.0.0.1:1", approle());
    cfg.retries = 0;
    assert!(matches!(
        VaultSigner::connect(cfg).await,
        Err(SignerError::Unavailable(_))
    ));
}

#[tokio::test]
async fn vault_token_file_is_reread_after_a_403() {
    let (url, mock) = mock_vault().await;
    let dir = std::env::temp_dir().join(format!("vault-token-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("token");
    mock.lock().unwrap().token = "agent-token-1".into();
    std::fs::write(&file, "agent-token-1\n").unwrap();
    let signer = VaultSigner::connect(config(&url, VaultAuth::TokenFile(file.clone())))
        .await
        .unwrap();
    // Vault Agent renews into the sink file; the old token stops working.
    mock.lock().unwrap().token = "agent-token-2".into();
    std::fs::write(&file, "agent-token-2\n").unwrap();
    signs(&signer).await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

// ---- a real Vault (vault server -dev) -------------------------------------------

fn vault_env() -> (String, String) {
    (
        std::env::var("VAULT_ADDR").expect("VAULT_ADDR (e.g. http://127.0.0.1:8200)"),
        std::env::var("VAULT_TOKEN").expect("VAULT_TOKEN: an admin token (the dev root token)"),
    )
}

struct Bootstrapped {
    key: String,
    dir: PathBuf,
    public_key: [u8; 32],
}

/// Runs deploy/vault/bootstrap.sh with a fresh key and role name.
fn bootstrap() -> Bootstrapped {
    let (addr, token) = vault_env();
    let tag = format!(
        "t{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(format!("vault-bootstrap-{tag}"));
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../deploy/vault/bootstrap.sh"
    );
    let out = std::process::Command::new("bash")
        .arg(script)
        .arg(&dir)
        .env("VAULT_ADDR", addr)
        .env("VAULT_TOKEN", token)
        .env("VAULT_TRANSIT_KEY", &tag)
        .env("VAULT_APPROLE_ROLE", &tag)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let hex = stdout
        .lines()
        .find_map(|l| l.strip_prefix("checkpoint public key: "))
        .expect("bootstrap prints the public key");
    Bootstrapped {
        key: tag,
        dir,
        public_key: hex::decode(hex).unwrap().try_into().unwrap(),
    }
}

impl Bootstrapped {
    fn signer_config(&self) -> SignerConfig {
        let (addr, _) = vault_env();
        let (role, secret) = (
            self.dir.join("vault_role_id"),
            self.dir.join("vault_secret_id"),
        );
        SignerConfig::from_env(&Env::from_pairs([
            ("WORKER_SIGNER", "vault"),
            ("VAULT_ADDR", addr.as_str()),
            ("VAULT_ROLE_ID_FILE", role.to_str().unwrap()),
            ("VAULT_SECRET_ID_FILE", secret.to_str().unwrap()),
            ("VAULT_TRANSIT_KEY", self.key.as_str()),
        ]))
        .unwrap()
    }

    async fn login(&self) -> String {
        let (addr, _) = vault_env();
        let read = |f: &str| {
            std::fs::read_to_string(self.dir.join(f))
                .unwrap()
                .trim()
                .to_string()
        };
        let resp: Value = reqwest::Client::new()
            .post(format!("{addr}/v1/auth/approle/login"))
            .json(&json!({"role_id": read("vault_role_id"), "secret_id": read("vault_secret_id")}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        resp["auth"]["client_token"].as_str().unwrap().to_string()
    }
}

async fn vault_status(method: reqwest::Method, path: &str, token: &str, body: Value) -> u16 {
    let (addr, _) = vault_env();
    reqwest::Client::new()
        .request(method, format!("{addr}/v1/{path}"))
        .header("X-Vault-Token", token)
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
#[ignore = "requires Vault (VAULT_ADDR + admin VAULT_TOKEN, e.g. vault server -dev), vault CLI and jq"]
async fn bootstrapped_vault_approle_signs_and_can_do_nothing_else() {
    if !common::vault_configured() {
        return;
    }
    let boot = bootstrap();
    let AnySigner::Vault(signer) = boot.signer_config().connect().await.unwrap() else {
        panic!("a vault signer")
    };
    assert_eq!(
        signer.public_key(),
        boot.public_key,
        "bootstrap printed the key the worker uses"
    );
    signs(&signer).await.unwrap();

    let token = boot.login().await;
    let key = &boot.key;
    let input = B64.encode([7u8; 32]);
    use reqwest::Method;
    // Sign-only: exactly the two allowed parameters, this key, this path.
    assert_eq!(
        vault_status(
            Method::POST,
            &format!("transit/sign/{key}"),
            &token,
            json!({"input": input})
        )
        .await,
        200
    );
    for (method, path, body) in [
        (
            Method::GET,
            format!("transit/export/signing-key/{key}"),
            json!({}),
        ),
        (
            Method::POST,
            format!("transit/keys/{key}/rotate"),
            json!({}),
        ),
        (
            Method::POST,
            format!("transit/keys/{key}/config"),
            json!({"exportable": true}),
        ),
        (Method::DELETE, format!("transit/keys/{key}"), json!({})),
        (
            Method::POST,
            "transit/keys/another".to_string(),
            json!({"type": "ed25519"}),
        ),
        (
            Method::POST,
            format!("transit/sign/{key}/sha2-512"),
            json!({"input": input}),
        ),
        (
            Method::POST,
            format!("transit/sign/{key}"),
            json!({"input": input, "prehashed": true}),
        ),
        (
            Method::POST,
            format!("transit/sign/{key}"),
            json!({"batch_input": [{"input": input}]}),
        ),
        (
            Method::POST,
            format!("transit/verify/{key}"),
            json!({"input": input, "signature": "vault:v1:AA=="}),
        ),
        (Method::GET, "sys/policies/acl".to_string(), json!({})),
        (Method::GET, "auth/token/lookup-self".to_string(), json!({})),
    ] {
        assert_eq!(
            vault_status(method.clone(), &path, &token, body).await,
            403,
            "{method} {path}"
        );
    }
    // Non-exportable even for an admin.
    let (_, admin) = vault_env();
    assert_ne!(
        vault_status(
            Method::GET,
            &format!("transit/export/signing-key/{key}"),
            &admin,
            json!({})
        )
        .await,
        200
    );

    // Rotating the key in Vault does not silently switch the worker's key:
    // it keeps signing with the version it pinned at connect time.
    assert_eq!(
        vault_status(
            Method::POST,
            &format!("transit/keys/{key}/rotate"),
            &admin,
            json!({})
        )
        .await,
        200
    );
    signs(&signer).await.unwrap();
    assert_eq!(signer.public_key(), boot.public_key);
    let AnySigner::Vault(fresh) = boot.signer_config().connect().await.unwrap() else {
        panic!()
    };
    assert_ne!(
        fresh.public_key(),
        boot.public_key,
        "a reconnect sees the new version: it must be pinned first"
    );

    // Token auth (e.g. a Vault Agent sink file) works as well.
    let file = boot.dir.join("token");
    std::fs::write(&file, &token).unwrap();
    let (addr, _) = vault_env();
    let mut cfg = VaultConfig {
        addr,
        transit_mount: "transit".into(),
        key_name: key.clone(),
        auth: VaultAuth::TokenFile(file),
        ca_cert: None,
        timeout: Duration::from_secs(5),
        retries: 1,
    };
    signs(&VaultSigner::connect(cfg.clone()).await.unwrap())
        .await
        .unwrap();
    cfg.key_name = "no-such-key".into();
    assert!(
        matches!(VaultSigner::connect(cfg).await, Err(SignerError::Rejected(m)) if m.contains("403"))
    );
    std::fs::remove_dir_all(boot.dir).unwrap();
}

#[tokio::test]
#[ignore = "requires Vault (VAULT_ADDR + admin VAULT_TOKEN), vault CLI, jq and PostgreSQL (DATABASE_URL)"]
async fn a_vault_signed_chain_verifies_against_the_pinned_key_only() {
    if !common::vault_configured() {
        return;
    }
    let boot = bootstrap();
    let signer = boot.signer_config().connect().await.unwrap();
    let db = common::scratch("vault").await;
    db.post_transfers(3).await;
    // Rotation local -> Vault: history signed by the local key, then Vault;
    // the local key is retired after the last checkpoint it signed.
    seal_all(&db.pool, &common::key_a(), 2).await.unwrap();
    let last_local = db.latest().await.last_checkpoint_seq;
    db.post_transfers(2).await;
    seal_all(&db.pool, &signer, 2).await.unwrap();

    let both = TrustedKeys::new()
        .with_retired(common::key_a().public_key_bytes(), last_local)
        .unwrap()
        .with(boot.public_key)
        .unwrap();
    verify_chain(&db.pool, &both).await.unwrap();
    // Not pinned yet -> the Vault-signed checkpoints are untrusted.
    let only_local = common::trusted();
    assert!(matches!(
        verify_chain(&db.pool, &only_local).await,
        Err(WorkerError::ChainBroken { reason, .. }) if reason == "untrusted signing key"
    ));
    let only_vault = TrustedKeys::new().with(boot.public_key).unwrap();
    assert!(
        verify_chain(&db.pool, &only_vault).await.is_err(),
        "the old key must stay pinned for history"
    );
    db.drop_db().await;
    std::fs::remove_dir_all(boot.dir).unwrap();
}

/// A signer that returns garbage: nothing is written, the batch stays unsealed.
struct LyingSigner(Sealer);

impl CheckpointSigner for LyingSigner {
    fn public_key(&self) -> [u8; 32] {
        self.0.public_key_bytes()
    }

    async fn sign(&self, hash: &Hash) -> Result<[u8; 64], SignerError> {
        Ok(Sealer::generate().sign_hash(hash))
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL (DATABASE_URL, superuser: creates scratch databases)"]
async fn a_signature_that_does_not_verify_is_never_written() {
    let db = common::scratch("lying").await;
    db.post_transfers(2).await;
    let err = seal_all(&db.pool, &LyingSigner(common::key_a()), 500)
        .await
        .unwrap_err();
    assert!(matches!(err, WorkerError::Signing(_)), "{err}");
    let (checkpoints, sealed): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM checkpoints), (SELECT COUNT(*) FROM transactions WHERE sealed_seq IS NOT NULL)",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!((checkpoints, sealed), (0, 0), "rolled back with the batch");
    assert!(seal_all(&db.pool, &common::key_a(), 500).await.unwrap() > 0);
    verify_chain(&db.pool, &common::trusted()).await.unwrap();
    db.drop_db().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL (DATABASE_URL, superuser: creates scratch databases)"]
async fn a_retired_key_cannot_sign_new_checkpoints() {
    let db = common::scratch("retired").await;
    let (old, new) = (common::key_a(), Sealer::from_secret_bytes(&[0xB2; 32]));
    db.post_transfers(2).await;
    seal_all(&db.pool, &old, 500).await.unwrap();
    let last_old = db.latest().await.last_checkpoint_seq;
    db.post_transfers(1).await;
    seal_all(&db.pool, &new, 500).await.unwrap();
    let list = format!(
        "{}@{last_old},{}",
        old.public_key_hex(),
        new.public_key_hex()
    );
    let trusted = TrustedKeys::from_hex_list(&list).unwrap();
    verify_chain(&db.pool, &trusted).await.unwrap();

    // Whoever kept a copy of the old key signs the next checkpoint with it.
    db.post_transfers(1).await;
    seal_all(&db.pool, &old, 500).await.unwrap();
    let forged = db.latest().await.last_checkpoint_seq;
    assert!(matches!(
        verify_chain(&db.pool, &trusted).await,
        Err(WorkerError::ChainBroken { seq, reason })
            if seq == forged && reason == format!("signed by a key retired after checkpoint {last_old}")
    ));
    db.drop_db().await;
}

async fn metrics_text(port: u16) -> String {
    match reqwest::get(format!("http://127.0.0.1:{port}/metrics")).await {
        Ok(resp) => resp.text().await.unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// The binary with WORKER_SIGNER=vault: seals through Vault once the key is
/// pinned; with an unpinned key it stays up (relay, duties) but never seals.
#[tokio::test]
#[ignore = "requires Vault (VAULT_ADDR + admin VAULT_TOKEN), vault CLI, jq and PostgreSQL (DATABASE_URL)"]
async fn the_worker_seals_through_vault_only_with_a_pinned_key() {
    if !common::vault_configured() {
        return;
    }
    let boot = bootstrap();
    let (addr, _) = vault_env();
    let db = common::scratch("vault_e2e").await;
    db.post_transfers(3).await;
    let spawn = |trusted: String, port: u16| {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_payment-workers"))
            .env("DATABASE_URL", &db.url)
            .env("APP_ENV", "dev")
            .env("WORKER_SIGNER", "vault")
            .env("VAULT_ADDR", &addr)
            .env("VAULT_ROLE_ID_FILE", boot.dir.join("vault_role_id"))
            .env("VAULT_SECRET_ID_FILE", boot.dir.join("vault_secret_id"))
            .env("VAULT_TRANSIT_KEY", &boot.key)
            .env("WORKER_TRUSTED_PUBLIC_KEYS", trusted)
            .env("WORKER_INTERVAL_SECS", "1")
            .env("METRICS_ADDR", format!("127.0.0.1:{port}"))
            .env(
                "WORKER_HEARTBEAT_FILE",
                boot.dir.join(format!("heartbeat-{port}")),
            )
            .env("DOCUMENT_STORE_DIR", &boot.dir)
            .env("RUST_LOG", "error")
            .env_remove("WORKER_SIGNING_KEY")
            .env_remove("WORKER_SIGNING_KEY_FILE")
            .env_remove("VAULT_TOKEN")
            .env_remove("NATS_URL")
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    };
    let free_port = || {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };

    // Unpinned: up and running, signer not ready, nothing sealed.
    let port = free_port();
    let mut unpinned = spawn(common::key_a().public_key_hex(), port);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let m = metrics_text(port).await;
        if m.contains("checkpoint_signer_ready 0")
            && m.contains("checkpoint_signer_failures_total{signer=\"vault\"}")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "signer never reported unavailable:\n{m}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(
        unpinned.try_wait().unwrap().is_none(),
        "an unpinned signer must not take the worker down"
    );
    let sealed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM checkpoints")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(sealed, 0);
    unpinned.start_kill().unwrap();
    unpinned.wait().await.unwrap();

    // Pinned: every checkpoint carries the Vault key.
    let port = free_port();
    let both = format!(
        "{},{}",
        common::key_a().public_key_hex(),
        hex::encode(boot.public_key)
    );
    let mut pinned = spawn(both, port);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let keys: Vec<Vec<u8>> = sqlx::query_scalar("SELECT DISTINCT public_key FROM checkpoints")
            .fetch_all(&db.pool)
            .await
            .unwrap();
        if !keys.is_empty() {
            assert_eq!(keys, [boot.public_key.to_vec()]);
            break;
        }
        assert!(Instant::now() < deadline, "nothing sealed through Vault");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let m = metrics_text(port).await;
    assert!(
        m.contains("worker_signer_info{signer=\"vault\"} 1")
            && m.contains("checkpoint_signer_ready 1"),
        "{m}"
    );
    pinned.start_kill().unwrap();
    pinned.wait().await.unwrap();
    let vault_only = TrustedKeys::new().with(boot.public_key).unwrap();
    verify_chain(&db.pool, &vault_only).await.unwrap();

    // A leftover local key next to WORKER_SIGNER=vault is a startup error.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_payment-workers"))
        .env("DATABASE_URL", &db.url)
        .env("WORKER_SIGNER", "vault")
        .env("VAULT_ADDR", &addr)
        .env("VAULT_TOKEN", "x")
        .env("WORKER_SIGNING_KEY", hex::encode([0xA1u8; 32]))
        .env("METRICS_ADDR", format!("127.0.0.1:{}", free_port()))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && stderr.contains("remove it"),
        "{stderr}"
    );
    db.drop_db().await;
    std::fs::remove_dir_all(boot.dir).unwrap();
}
