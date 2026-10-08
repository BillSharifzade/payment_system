//! External anchoring: the backends against an in-test RFC 3161 TSA and mock
//! OpenTimestamps calendars over real HTTP, and — on scratch databases, since
//! the attack tests rewrite the whole chain — anchoring, verification and the
//! proof that re-signing history with the legitimate key is caught.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use common::{key_a, scratch, trusted, Scratch};
use crypto::ots::{Attestation, DetachedTimestamp, Op, Timestamp};
use crypto::rfc3161::test_tsa::{Fault, TestTsa};
use crypto::rfc3161::TsaTrust;
use sqlx::Row;
use workers::anchor::{
    anchor_checkpoint, anchor_imprint, check_anchor_consistency, upgrade_pending, verify_anchors,
    witness_status, Anchor, AnchorConfig, AnchorCursor, AnchorError, AnyAnchor, OtsAnchor,
    TsaAnchor,
};
use workers::env::Env;
use workers::{seal_all, verify_chain, WorkerError};

// ---- an RFC 3161 TSA over HTTP ------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum TsaMode {
    Normal,
    Http500,
    Huge,
}

struct TsaServer {
    url: String,
    tsa: Arc<TestTsa>,
    mode: Arc<Mutex<TsaMode>>,
}

async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    url
}

async fn tsa_server() -> TsaServer {
    let tsa = Arc::new(TestTsa::new());
    let mode = Arc::new(Mutex::new(TsaMode::Normal));
    let state = (tsa.clone(), mode.clone());
    let router =
        axum::Router::new()
            .route(
                "/tsr",
                post(
                    |State((tsa, mode)): State<(Arc<TestTsa>, Arc<Mutex<TsaMode>>)>,
                     body: Bytes| async move {
                        match *mode.lock().unwrap() {
                            TsaMode::Http500 => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                            TsaMode::Huge => vec![0u8; 200_000].into_response(),
                            TsaMode::Normal => (
                                [(header::CONTENT_TYPE, "application/timestamp-reply")],
                                tsa.respond(&body),
                            )
                                .into_response(),
                        }
                    },
                ),
            )
            .with_state(state);
    TsaServer {
        url: format!("{}/tsr", serve(router).await),
        tsa,
        mode,
    }
}

impl TsaServer {
    fn trust(&self) -> TsaTrust {
        TsaTrust::from_pem(self.tsa.ca_pem().as_bytes()).unwrap()
    }

    fn anchor(&self) -> TsaAnchor {
        TsaAnchor::new(self.url.clone(), self.trust(), reqwest::Client::new())
    }
}

// ---- a mock OpenTimestamps calendar --------------------------------------------

#[derive(Default)]
struct CalendarState {
    url: String,
    /// Commitments this calendar promised, with the Bitcoin path once "mined".
    commitments: Vec<Vec<u8>>,
    mined_at: Option<u64>,
    down: bool,
    /// Merkle root each commitment ends in, once mined.
    roots: Vec<[u8; 32]>,
    pending_uri: Option<String>,
}

type Calendar = Arc<Mutex<CalendarState>>;

/// What the calendar commits to Bitcoin for `commitment`: the transaction
/// around it (double SHA-256 = txid) and one Merkle sibling.
fn bitcoin_path(commitment: &[u8], height: u64) -> (Timestamp, [u8; 32]) {
    let mut ts = Timestamp::new(commitment.to_vec());
    let leaf = ts
        .add_op(Op::Prepend(b"\x01\x00\x00\x00 tx-prefix".to_vec()))
        .and_then(|n| n.add_op(Op::Append(b" tx-suffix".to_vec())))
        .and_then(|n| n.add_op(Op::Sha256))
        .and_then(|n| n.add_op(Op::Sha256))
        .and_then(|n| n.add_op(Op::Append([0x5a; 32].to_vec())))
        .and_then(|n| n.add_op(Op::Sha256))
        .and_then(|n| n.add_op(Op::Sha256))
        .unwrap();
    let root: [u8; 32] = leaf.msg().try_into().unwrap();
    leaf.attest(Attestation::Bitcoin(height));
    (ts, root)
}

async fn calendar() -> (String, Calendar) {
    let state: Calendar = Arc::default();
    let router = axum::Router::new()
        .route(
            "/digest",
            post(|State(cal): State<Calendar>, body: Bytes| async move {
                let mut cal = cal.lock().unwrap();
                if cal.down || body.len() != 32 {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                let mut ts = Timestamp::new(body.to_vec());
                let node = ts
                    .add_op(Op::Prepend(cal.commitments.len().to_be_bytes().to_vec()))
                    .and_then(|n| n.add_op(Op::Sha256))
                    .unwrap();
                let uri = cal.pending_uri.clone().unwrap_or_else(|| cal.url.clone());
                node.attest(Attestation::Pending(uri));
                cal.commitments.push(node.msg().to_vec());
                ts.to_bytes().into_response()
            }),
        )
        .route(
            "/timestamp/{commitment}",
            get(
                |State(cal): State<Calendar>, Path(commitment): Path<String>| async move {
                    let mut cal = cal.lock().unwrap();
                    let commitment = hex::decode(commitment).unwrap_or_default();
                    match cal.mined_at {
                        Some(height) if cal.commitments.contains(&commitment) => {
                            let (ts, root) = bitcoin_path(&commitment, height);
                            cal.roots.push(root);
                            ts.to_bytes().into_response()
                        }
                        _ => (
                            StatusCode::NOT_FOUND,
                            "Pending confirmation in Bitcoin blockchain",
                        )
                            .into_response(),
                    }
                },
            ),
        )
        .with_state(state.clone());
    let url = serve(router).await;
    state.lock().unwrap().url = url.clone();
    (url, state)
}

fn checkpoint_hash(tag: &[u8]) -> [u8; 32] {
    *crypto::sha256(tag).as_bytes()
}

// ---- backends, no database --------------------------------------------------------

#[tokio::test]
async fn tsa_anchor_stores_only_verified_tokens() {
    let server = tsa_server().await;
    let anchor = server.anchor();
    let hash = checkpoint_hash(b"cp-1");

    let stamp = anchor.stamp(&hash).await.unwrap();
    assert!(stamp.complete && stamp.bitcoin_height.is_none());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!((stamp.attested_at.unwrap() - now).abs() < 5);
    let token = crypto::rfc3161::verify_response(
        &stamp.proof,
        &anchor_imprint(&hash),
        None,
        Some(&server.trust()),
    )
    .unwrap();
    assert_eq!(token.gen_time, stamp.attested_at.unwrap());

    for (fault, expect) in [
        (Fault::WrongNonce, "nonce"),
        (Fault::Skewed(3_600), "from our clock"),
        (Fault::Skewed(-3_600), "from our clock"),
        (Fault::ForeignKey, "signature"),
        (Fault::Rejected, "rejected"),
    ] {
        server.tsa.set_fault(fault);
        let err = anchor.stamp(&hash).await.unwrap_err();
        assert!(matches!(err, AnchorError::Proof(_)), "{fault:?}: {err}");
        assert!(err.to_string().contains(expect), "{fault:?}: {err}");
    }
    server.tsa.set_fault(Fault::None);

    let stranger = TsaAnchor::new(
        server.url.clone(),
        TsaTrust::from_pem(TestTsa::new().ca_pem().as_bytes()).unwrap(),
        reqwest::Client::new(),
    );
    assert!(stranger
        .stamp(&hash)
        .await
        .unwrap_err()
        .to_string()
        .contains("untrusted"));

    *server.mode.lock().unwrap() = TsaMode::Http500;
    assert!(matches!(
        anchor.stamp(&hash).await,
        Err(AnchorError::Witness(_))
    ));
    *server.mode.lock().unwrap() = TsaMode::Huge;
    assert!(anchor
        .stamp(&hash)
        .await
        .unwrap_err()
        .to_string()
        .contains("larger than"));
    let nowhere = TsaAnchor::new(
        "http://127.0.0.1:1/tsr".into(),
        server.trust(),
        reqwest::Client::new(),
    );
    assert!(matches!(
        nowhere.stamp(&hash).await,
        Err(AnchorError::Transport(_))
    ));
}

#[tokio::test]
async fn ots_anchor_merges_calendars_and_upgrades_to_a_bitcoin_attestation() {
    let (url_a, cal_a) = calendar().await;
    let (url_b, cal_b) = calendar().await;
    let anchor = OtsAnchor::new(vec![url_a.clone(), url_b.clone()], reqwest::Client::new());
    let hash = checkpoint_hash(b"cp-ots");

    let stamp = anchor.stamp(&hash).await.unwrap();
    assert!(!stamp.complete);
    let proof = DetachedTimestamp::parse(&stamp.proof).unwrap();
    assert_eq!(
        proof.digest(),
        anchor_imprint(&hash),
        "a standard .ots for the hash bytes"
    );
    let mut pending: Vec<_> = proof
        .timestamp
        .attestations()
        .into_iter()
        .map(|(_, a)| a.clone())
        .collect();
    let mut expected = vec![
        Attestation::Pending(url_a.clone()),
        Attestation::Pending(url_b.clone()),
    ];
    pending.sort();
    expected.sort();
    assert_eq!(pending, expected);

    assert_eq!(
        anchor.upgrade(&hash, &stamp.proof).await.unwrap(),
        None,
        "not mined yet"
    );
    cal_b.lock().unwrap().mined_at = Some(860_000);
    let done = anchor.upgrade(&hash, &stamp.proof).await.unwrap().unwrap();
    assert!(done.complete);
    assert_eq!(done.bitcoin_height, Some(860_000));
    let upgraded = DetachedTimestamp::parse(&done.proof).unwrap();
    let root = cal_b.lock().unwrap().roots[0];
    assert_eq!(
        upgraded.timestamp.bitcoin_attestations().unwrap(),
        [(860_000, root)]
    );
    // The pending promise of the other calendar is kept, as `ots upgrade` does.
    assert_eq!(upgraded.timestamp.attestations().len(), 3);
    assert!(anchor
        .upgrade(&checkpoint_hash(b"other"), &stamp.proof)
        .await
        .is_err());

    // One calendar down: the other's promise is enough. Both down: an error.
    cal_a.lock().unwrap().down = true;
    let partial = DetachedTimestamp::parse(&anchor.stamp(&hash).await.unwrap().proof).unwrap();
    assert_eq!(partial.timestamp.attestations().len(), 1);
    cal_b.lock().unwrap().down = true;
    assert!(matches!(
        anchor.stamp(&hash).await,
        Err(AnchorError::Transport(_))
    ));
    cal_a.lock().unwrap().down = false;
    cal_b.lock().unwrap().down = false;

    // A calendar naming an arbitrary host for upgrades is not followed.
    cal_a.lock().unwrap().pending_uri = Some("http://127.0.0.1:1".into());
    let lone = OtsAnchor::new(vec![url_a], reqwest::Client::new());
    let stamp = lone.stamp(&hash).await.unwrap();
    cal_a.lock().unwrap().mined_at = Some(1);
    assert_eq!(lone.upgrade(&hash, &stamp.proof).await.unwrap(), None);
}

// ---- with a database ------------------------------------------------------------

async fn anchor_count(db: &Scratch) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM checkpoint_anchors")
        .fetch_one(&db.pool)
        .await
        .unwrap()
}

fn broken(result: workers::Result<impl std::fmt::Debug>) -> (i64, String) {
    match result {
        Err(WorkerError::ChainBroken { seq, reason }) => (seq, reason),
        other => panic!("expected ChainBroken, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL (DATABASE_URL, superuser: creates scratch databases)"]
async fn anchors_are_append_only_move_forward_and_complete_as_new_rows() {
    let db = scratch("forward").await;
    let tsa = tsa_server().await;
    let (cal_url, cal) = calendar().await;
    let ots = OtsAnchor::new(vec![cal_url], reqwest::Client::new());
    db.post_transfers(5).await;
    seal_all(&db.pool, &key_a(), 2).await.unwrap();
    let first = db.latest().await;

    let tsa_anchor = tsa.anchor();
    let rec = anchor_checkpoint(
        &db.pool,
        &tsa_anchor,
        first.last_checkpoint_seq,
        &first.last_checkpoint_hash,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(rec.complete);
    assert_eq!(
        anchor_checkpoint(
            &db.pool,
            &tsa_anchor,
            first.last_checkpoint_seq,
            &first.last_checkpoint_hash
        )
        .await
        .unwrap(),
        None,
        "already anchored"
    );
    assert_eq!(
        anchor_checkpoint(&db.pool, &tsa_anchor, 1, &checkpoint_hash(b"older"))
            .await
            .unwrap(),
        None,
        "an older checkpoint is covered by the newer anchor"
    );
    // A hash that is not the checkpoint's is never stored.
    let wrong = anchor_checkpoint(
        &db.pool,
        &ots,
        first.last_checkpoint_seq,
        &checkpoint_hash(b"forged"),
    )
    .await;
    assert!(broken(wrong)
        .1
        .contains("changed while it was being anchored"));
    assert_eq!(anchor_count(&db).await, 1);
    let pending = anchor_checkpoint(
        &db.pool,
        &ots,
        first.last_checkpoint_seq,
        &first.last_checkpoint_hash,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!pending.complete);

    // The runtime cannot rewrite or delete an anchor.
    for sql in [
        "UPDATE checkpoint_anchors SET witness = 'x'",
        "DELETE FROM checkpoint_anchors",
        "TRUNCATE checkpoint_anchors",
    ] {
        let err = sqlx::query(sql)
            .execute(&db.pool)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("append-only"), "{sql}: {err}");
    }

    // New checkpoints wait for the next anchor; the OTS proof for Bitcoin.
    db.post_transfers(1).await;
    seal_all(&db.pool, &key_a(), 500).await.unwrap();
    let status = witness_status(&db.pool, "rfc3161", &tsa.url).await.unwrap();
    assert_eq!(status.anchored_through_seq, first.last_checkpoint_seq);
    assert!(status.unanchored_age_seconds >= 0.0 && status.anchor_age_seconds.is_some());
    let ots_status = witness_status(&db.pool, "ots", "opentimestamps")
        .await
        .unwrap();
    assert_eq!(
        (
            ots_status.submitted_through_seq,
            ots_status.anchored_through_seq
        ),
        (first.last_checkpoint_seq, 0)
    );
    assert!(ots_status.unconfirmed_age_seconds >= 0.0 && ots_status.anchor_age_seconds.is_none());

    let report = upgrade_pending(&db.pool, &ots, 10).await.unwrap();
    assert_eq!((report.checked, report.upgraded), (1, 0));
    cal.lock().unwrap().mined_at = Some(870_000);
    let report = upgrade_pending(&db.pool, &ots, 10).await.unwrap();
    assert_eq!((report.checked, report.upgraded), (1, 1));
    assert_eq!(
        upgrade_pending(&db.pool, &ots, 10).await.unwrap().checked,
        0,
        "upgraded once"
    );
    let rows = sqlx::query("SELECT status, bitcoin_height, upgrades FROM checkpoint_anchors WHERE kind = 'ots' ORDER BY id")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "the pending row stays; the completion is a new row"
    );
    assert_eq!(rows[0].get::<String, _>("status"), "pending");
    assert_eq!(
        rows[1].get::<Option<i64>, _>("bitcoin_height"),
        Some(870_000)
    );
    assert_eq!(
        witness_status(&db.pool, "ots", "opentimestamps")
            .await
            .unwrap()
            .anchored_through_seq,
        first.last_checkpoint_seq
    );

    let (report, _) = verify_anchors(&db.pool, Some(&tsa.trust()), &AnchorCursor::default())
        .await
        .unwrap();
    assert_eq!(
        (
            report.verified,
            report.rfc3161,
            report.ots_pending,
            report.ots_bitcoin
        ),
        (3, 1, 1, 1)
    );
    assert_eq!(report.anchored_through_seq, first.last_checkpoint_seq);
    assert_eq!(report.bitcoin[0].height, 870_000);
    assert_eq!(report.bitcoin[0].merkle_root, cal.lock().unwrap().roots[0]);
    db.drop_db().await;
}

/// The gap DESIGN §6.5 left open: whoever holds the signing key can rewrite
/// history and re-sign every checkpoint. Anchors close it.
#[tokio::test]
#[ignore = "requires PostgreSQL (DATABASE_URL, superuser: creates scratch databases)"]
async fn history_re_signed_with_the_trusted_key_fails_once_anchored() {
    let db = scratch("resign").await;
    let tsa = tsa_server().await;
    let (cal_url, cal) = calendar().await;
    let ots = OtsAnchor::new(vec![cal_url], reqwest::Client::new());
    db.post_transfers(6).await;
    seal_all(&db.pool, &key_a(), 3).await.unwrap();
    let anchored = db.latest().await;
    for anchor in [AnyAnchor::Tsa(tsa.anchor()), AnyAnchor::Ots(ots)] {
        anchor_checkpoint(
            &db.pool,
            &anchor,
            anchored.last_checkpoint_seq,
            &anchored.last_checkpoint_hash,
        )
        .await
        .unwrap()
        .unwrap();
        if let AnyAnchor::Ots(ref ots) = anchor {
            cal.lock().unwrap().mined_at = Some(880_000);
            assert_eq!(
                upgrade_pending(&db.pool, ots, 10).await.unwrap().upgraded,
                1
            );
        }
    }
    verify_chain(&db.pool, &trusted()).await.unwrap();
    check_anchor_consistency(&db.pool).await.unwrap();
    let (_, cursor) = verify_anchors(&db.pool, Some(&tsa.trust()), &AnchorCursor::default())
        .await
        .unwrap();

    // The attack: change a settled transfer (both legs, so it still balances),
    // drop every checkpoint, unseal everything and reseal with the SAME,
    // trusted key.
    db.as_superuser(&[
        "UPDATE entries SET amount_minor = amount_minor + 50000
         WHERE transaction_id = (SELECT id FROM transactions ORDER BY seq OFFSET 2 LIMIT 1)",
        "DELETE FROM checkpoints",
        "UPDATE transactions SET sealed_seq = NULL",
    ])
    .await;
    seal_all(&db.pool, &key_a(), 3).await.unwrap();
    let report = verify_chain(&db.pool, &trusted())
        .await
        .expect("on its own, the re-signed chain verifies");
    assert!(report.checkpoints_verified >= 3, "{report:?}");
    assert_ne!(
        db.latest().await.last_checkpoint_hash,
        anchored.last_checkpoint_hash
    );

    // The anchors still hold the old hash.
    let (seq, reason) = broken(check_anchor_consistency(&db.pool).await);
    assert_eq!(seq, anchored.last_checkpoint_seq);
    assert!(
        reason.contains("history was rewritten after it was anchored"),
        "{reason}"
    );
    let (_, reason) =
        broken(verify_anchors(&db.pool, Some(&tsa.trust()), &AnchorCursor::default()).await);
    assert!(reason.contains("rewritten"), "{reason}");

    // Rewriting the anchors' hash column too: the proofs commit to the old
    // hash, and no one without the TSA's key (or Bitcoin) can make new ones.
    db.as_superuser(&[
        "UPDATE checkpoint_anchors a SET checkpoint_hash = c.checkpoint_hash
                       FROM checkpoints c WHERE c.seq = a.checkpoint_seq",
    ])
    .await;
    check_anchor_consistency(&db.pool).await.unwrap();
    let (_, reason) =
        broken(verify_anchors(&db.pool, Some(&tsa.trust()), &AnchorCursor::default()).await);
    assert!(
        reason.contains("different message imprint") || reason.contains("another digest"),
        "{reason}"
    );
    let (_, reason) = broken(verify_anchors(&db.pool, None, &AnchorCursor::default()).await);
    assert!(
        reason.contains("different message imprint"),
        "even without TSA certificates: {reason}"
    );

    // The verify-chain command says so too.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_payment-workers"))
        .arg("verify-chain")
        .env("DATABASE_URL", &db.url)
        .env("WORKER_TRUSTED_PUBLIC_KEYS", key_a().public_key_hex())
        .env_remove("WORKER_SIGNING_KEY")
        .env_remove("WORKER_SIGNING_KEY_FILE")
        .env_remove("ANCHOR_RFC3161_CERTS_FILE")
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(1), "{json}");
    assert_eq!(json["status"], "broken");

    // Deleting the anchors outright leaves no proof in the database — but a
    // verifier that already saw them notices (and so does the
    // checkpoint_anchored_through_seq history in Prometheus).
    db.as_superuser(&["DELETE FROM checkpoint_anchors"]).await;
    let (_, reason) = broken(verify_anchors(&db.pool, Some(&tsa.trust()), &cursor).await);
    assert!(reason.contains("was removed"), "{reason}");
    db.drop_db().await;
}

fn verify_chain_cmd(
    url: &str,
    certs: Option<&std::path::Path>,
) -> (Option<i32>, serde_json::Value) {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_payment-workers"));
    cmd.arg("verify-chain")
        .env("DATABASE_URL", url)
        .env("WORKER_TRUSTED_PUBLIC_KEYS", key_a().public_key_hex())
        .env_remove("WORKER_SIGNING_KEY")
        .env_remove("WORKER_SIGNING_KEY_FILE")
        .env_remove("ANCHOR_RFC3161_CERTS_FILE");
    if let Some(certs) = certs {
        cmd.env("ANCHOR_RFC3161_CERTS_FILE", certs);
    }
    let out = cmd.output().unwrap();
    let json = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
    (out.status.code(), json)
}

#[tokio::test]
#[ignore = "requires PostgreSQL (DATABASE_URL, superuser: creates scratch databases)"]
async fn verify_chain_reports_anchor_coverage() {
    let db = scratch("report").await;
    let tsa = tsa_server().await;
    db.post_transfers(3).await;
    seal_all(&db.pool, &key_a(), 2).await.unwrap();

    let (code, json) = verify_chain_cmd(&db.url, None);
    assert_eq!(code, Some(0), "{json}");
    assert_eq!(json["anchored_through_seq"], 0);
    assert!(json["anchor_age_seconds"].is_null());

    let anchored = db.latest().await;
    anchor_checkpoint(
        &db.pool,
        &tsa.anchor(),
        anchored.last_checkpoint_seq,
        &anchored.last_checkpoint_hash,
    )
    .await
    .unwrap();
    db.post_transfers(1).await;
    seal_all(&db.pool, &key_a(), 500).await.unwrap();
    let certs = std::env::temp_dir().join(format!("tsa-{}.pem", std::process::id()));
    std::fs::write(&certs, tsa.tsa.ca_pem()).unwrap();

    let (code, json) = verify_chain_cmd(&db.url, Some(&certs));
    assert_eq!(code, Some(0), "{json}");
    assert_eq!(json["status"], "intact");
    assert_eq!(json["anchored_through_seq"], anchored.last_checkpoint_seq);
    assert!(json["anchor_age_seconds"].as_i64().unwrap() < 60, "{json}");
    assert_eq!(
        json["unanchored_checkpoints"],
        db.latest().await.last_checkpoint_seq - anchored.last_checkpoint_seq
    );
    assert_eq!(json["anchors"]["rfc3161"], 1);
    assert_eq!(json["anchors"]["rfc3161_signatures_trusted"], true);

    // Without the TSA certificates the imprint is still checked; the report
    // says the signatures were not.
    let (code, json) = verify_chain_cmd(&db.url, None);
    assert_eq!(
        (code, &json["anchors"]["rfc3161_signatures_trusted"]),
        (Some(0), &serde_json::json!(false))
    );
    // Certificates of another TSA: the token does not chain — broken.
    std::fs::write(&certs, TestTsa::new().ca_pem()).unwrap();
    let (code, json) = verify_chain_cmd(&db.url, Some(&certs));
    assert_eq!(code, Some(1), "{json}");
    assert!(
        json["reason"].as_str().unwrap().contains("untrusted TSA"),
        "{json}"
    );
    std::fs::write(&certs, "not a certificate").unwrap();
    assert_eq!(verify_chain_cmd(&db.url, Some(&certs)).0, Some(2));
    std::fs::remove_file(certs).unwrap();
    db.drop_db().await;
}

/// The binary end to end: elected leader, first full verification, then the
/// anchor loop stamps the newest verified checkpoint with both witnesses,
/// upgrades the OTS proof once "mined", and exports the metrics.
#[tokio::test]
#[ignore = "requires PostgreSQL (DATABASE_URL, superuser: creates scratch databases)"]
async fn the_worker_anchors_verified_checkpoints_and_exports_metrics() {
    let db = scratch("e2e").await;
    let tsa = tsa_server().await;
    let (cal_url, cal) = calendar().await;
    db.post_transfers(4).await;
    let dir = std::env::temp_dir().join(format!("anchor-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("tsa.pem"), tsa.tsa.ca_pem()).unwrap();
    let metrics_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_payment-workers"))
        .env("DATABASE_URL", &db.url)
        .env("APP_ENV", "dev")
        .env("WORKER_SIGNING_KEY", hex::encode([0xA1u8; 32]))
        .env("WORKER_INTERVAL_SECS", "1")
        .env("VERIFY_INTERVAL_SECS", "1")
        .env("ANCHOR_INTERVAL_SECS", "10")
        .env("ANCHOR_RFC3161_URLS", &tsa.url)
        .env("ANCHOR_RFC3161_CERTS_FILE", dir.join("tsa.pem"))
        .env("ANCHOR_OTS_CALENDARS", &cal_url)
        .env("METRICS_ADDR", format!("127.0.0.1:{metrics_port}"))
        .env("WORKER_HEARTBEAT_FILE", dir.join("heartbeat"))
        .env("DOCUMENT_STORE_DIR", &dir)
        .env("RUST_LOG", "warn")
        .env_remove("NATS_URL")
        .env_remove("WORKER_SIGNER")
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let wait_for = |sql: &'static str| {
        let pool = db.pool.clone();
        async move {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let n: i64 = sqlx::query_scalar(sql).fetch_one(&pool).await.unwrap();
                if n > 0 {
                    return;
                }
                assert!(Instant::now() < deadline, "timed out waiting for: {sql}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };
    wait_for("SELECT COUNT(*) FROM checkpoint_anchors WHERE kind = 'rfc3161'").await;
    wait_for("SELECT COUNT(*) FROM checkpoint_anchors WHERE kind = 'ots'").await;
    cal.lock().unwrap().mined_at = Some(890_000);
    wait_for("SELECT COUNT(*) FROM checkpoint_anchors WHERE status = 'complete' AND kind = 'ots'")
        .await;

    // Anchored = the checkpoint the verifier had verified, still intact.
    check_anchor_consistency(&db.pool).await.unwrap();
    verify_anchors(&db.pool, Some(&tsa.trust()), &AnchorCursor::default())
        .await
        .unwrap();
    let anchored: i64 = sqlx::query_scalar("SELECT MAX(checkpoint_seq) FROM checkpoint_anchors")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(anchored >= 1);

    let deadline = Instant::now() + Duration::from_secs(90);
    let metrics = loop {
        let body = reqwest::get(format!("http://127.0.0.1:{metrics_port}/metrics"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        if body.contains("checkpoint_anchored_through_seq{witness=\"opentimestamps\"} ")
            && !body.contains("checkpoint_anchored_through_seq{witness=\"opentimestamps\"} 0")
        {
            break body;
        }
        assert!(
            Instant::now() < deadline,
            "metrics never showed the OTS anchor:\n{body}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    for needle in [
        format!("checkpoint_anchored_through_seq{{witness=\"{}\"}}", tsa.url),
        "checkpoint_anchor_unanchored_age_seconds".to_string(),
        "checkpoint_anchor_unconfirmed_age_seconds{witness=\"opentimestamps\"} 0".to_string(),
        "checkpoint_anchor_last_success_timestamp_seconds".to_string(),
        "worker_signer_info{signer=\"local\"} 1".to_string(),
        "checkpoint_signer_ready 1".to_string(),
        "ledger_chain_verified 1".to_string(),
    ] {
        assert!(metrics.contains(&needle), "missing {needle}:\n{metrics}");
    }

    child.start_kill().unwrap();
    child.wait().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    db.drop_db().await;
}

// ---- real public witnesses (network) -----------------------------------------

/// Third-party services: only with TAMPER_LIVE_ENDPOINTS=1, never in the
/// regular ignored lane (their uptime is not ours).
fn live() -> bool {
    common::external(
        "live public witnesses (TAMPER_LIVE_ENDPOINTS=1)",
        std::env::var_os("TAMPER_LIVE_ENDPOINTS").is_some(),
    )
}

/// freetsa.org's root is pinned by the operator in production; the test
/// fetches it (FREETSA_CA_PEM may point to a local copy instead).
#[tokio::test]
#[ignore = "needs outbound HTTPS to freetsa.org and TAMPER_LIVE_ENDPOINTS=1"]
async fn real_freetsa_token_verifies() {
    if !live() {
        return;
    }
    let pem = match std::env::var("FREETSA_CA_PEM") {
        Ok(path) => std::fs::read(path).unwrap(),
        Err(_) => reqwest::get("https://freetsa.org/files/cacert.pem")
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .to_vec(),
    };
    let anchor = TsaAnchor::new(
        "https://freetsa.org/tsr".into(),
        TsaTrust::from_pem(&pem).unwrap(),
        reqwest::Client::new(),
    );
    let stamp = anchor
        .stamp(&checkpoint_hash(b"payment-system freetsa test"))
        .await
        .unwrap();
    assert!(stamp.complete && stamp.attested_at.is_some());
}

#[tokio::test]
#[ignore = "needs outbound HTTPS to the public OpenTimestamps calendars and TAMPER_LIVE_ENDPOINTS=1"]
async fn real_ots_calendars_return_pending_proofs() {
    if !live() {
        return;
    }
    let cfg = AnchorConfig::from_env(&Env::from_pairs([(
        "ANCHOR_OTS_CALENDARS",
        "https://a.pool.opentimestamps.org,https://b.pool.opentimestamps.org",
    )]))
    .unwrap();
    let anchors = cfg.anchors().unwrap();
    let stamp = anchors[0]
        .stamp(&checkpoint_hash(b"payment-system ots test"))
        .await
        .unwrap();
    let proof = DetachedTimestamp::parse(&stamp.proof).unwrap();
    assert!(proof
        .timestamp
        .attestations()
        .iter()
        .all(|(_, a)| matches!(a, Attestation::Pending(uri) if uri.starts_with("https://"))));
}
