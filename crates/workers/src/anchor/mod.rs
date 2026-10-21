//! External anchoring of checkpoint hashes (DESIGN.md §6.4). A signature only
//! proves that whoever held the signing key wrote a checkpoint — and that key
//! holder can re-sign a rewritten history. An anchor puts the checkpoint hash
//! in front of a witness outside the system: an RFC 3161 timestamp authority
//! (a signed token, verifiable now) or OpenTimestamps (a commitment that ends
//! in a Bitcoin block). Each checkpoint hash commits to every earlier one, so
//! anchoring the newest verified checkpoint anchors all history before it;
//! rewriting anchored history afterwards breaks the anchor's hash, and the
//! verifier reports the chain broken.
//!
//! The witnessed datum is the 32 raw bytes of `checkpoint_hash`; both backends
//! timestamp its SHA-256, so `openssl ts -verify -data <bytes>` and
//! `ots verify` work on the stored proofs unchanged.

mod ots;
mod rfc3161;

use std::future::Future;
use std::time::Duration;

use crypto::ots::DetachedTimestamp;
use crypto::rfc3161::{verify_response, TsaTrust};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::env::{http_url, ConfigError, Env};
use crate::error::{Result, WorkerError};
pub use ots::OtsAnchor;
pub use rfc3161::{TsaAnchor, MAX_CLOCK_SKEW};

pub const KIND_RFC3161: &str = "rfc3161";
pub const KIND_OTS: &str = "ots";
pub const OTS_WITNESS: &str = "opentimestamps";
/// Pending OpenTimestamps proofs are retried for this long; calendars commit
/// within hours, so older ones are lost (and alerted on long before).
pub const OTS_UPGRADE_WINDOW: Duration = Duration::from_secs(14 * 86_400);
/// Bitcoin attestations listed in a report for an auditor to check.
pub const REPORT_BITCOIN_ATTESTATIONS: usize = 5;
const ANCHOR_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const VERIFY_PAGE_ANCHORS: i64 = 256;

#[derive(Debug, thiserror::Error)]
pub enum AnchorError {
    #[error("unreachable: {0}")]
    Transport(String),
    #[error("refused: {0}")]
    Witness(String),
    #[error("invalid proof: {0}")]
    Proof(String),
}

/// A witness's answer for one checkpoint hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// Verifiable as is (RFC 3161 token; Bitcoin-attested OTS proof), as
    /// opposed to a pending OTS calendar promise.
    pub complete: bool,
    /// TimeStampResp DER, or the `.ots` file.
    pub proof: Vec<u8>,
    /// RFC 3161 genTime, Unix seconds.
    pub attested_at: Option<i64>,
    pub bitcoin_height: Option<i64>,
}

pub trait Anchor: Send + Sync {
    fn kind(&self) -> &'static str;
    fn witness(&self) -> &str;

    /// Submits SHA-256(checkpoint_hash) to the witness.
    fn stamp(
        &self,
        checkpoint_hash: &[u8; 32],
    ) -> impl Future<Output = std::result::Result<Stamp, AnchorError>> + Send;

    /// Tries to complete a pending proof; None while the witness is not done.
    fn upgrade(
        &self,
        _checkpoint_hash: &[u8; 32],
        _proof: &[u8],
    ) -> impl Future<Output = std::result::Result<Option<Stamp>, AnchorError>> + Send {
        async { Ok(None) }
    }
}

pub enum AnyAnchor {
    Tsa(TsaAnchor),
    Ots(OtsAnchor),
}

impl Anchor for AnyAnchor {
    fn kind(&self) -> &'static str {
        match self {
            AnyAnchor::Tsa(a) => a.kind(),
            AnyAnchor::Ots(a) => a.kind(),
        }
    }

    fn witness(&self) -> &str {
        match self {
            AnyAnchor::Tsa(a) => a.witness(),
            AnyAnchor::Ots(a) => a.witness(),
        }
    }

    async fn stamp(&self, checkpoint_hash: &[u8; 32]) -> std::result::Result<Stamp, AnchorError> {
        match self {
            AnyAnchor::Tsa(a) => a.stamp(checkpoint_hash).await,
            AnyAnchor::Ots(a) => a.stamp(checkpoint_hash).await,
        }
    }

    async fn upgrade(
        &self,
        checkpoint_hash: &[u8; 32],
        proof: &[u8],
    ) -> std::result::Result<Option<Stamp>, AnchorError> {
        match self {
            AnyAnchor::Tsa(a) => a.upgrade(checkpoint_hash, proof).await,
            AnyAnchor::Ots(a) => a.upgrade(checkpoint_hash, proof).await,
        }
    }
}

/// The message imprint both backends timestamp.
pub fn anchor_imprint(checkpoint_hash: &[u8; 32]) -> [u8; 32] {
    *crypto::sha256(checkpoint_hash).as_bytes()
}

async fn read_capped(
    mut resp: reqwest::Response,
    max: usize,
) -> std::result::Result<Vec<u8>, AnchorError> {
    let url = resp.url().to_string();
    let too_large = || AnchorError::Witness(format!("{url}: response larger than {max} bytes"));
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| AnchorError::Transport(format!("{url}: {e}")))?
    {
        if body.len() + chunk.len() > max {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// ANCHOR_* settings. Anchoring is off unless a backend is configured.
#[derive(Debug)]
pub struct AnchorConfig {
    pub interval: Duration,
    pub tsa_urls: Vec<String>,
    /// Trust anchors for RFC 3161 tokens — also what verify-chain checks the
    /// stored tokens against.
    pub tsa_trust: Option<TsaTrust>,
    pub ots_calendars: Vec<String>,
}

impl AnchorConfig {
    pub fn from_env(env: &Env) -> std::result::Result<Self, ConfigError> {
        let urls = |key: &str| -> std::result::Result<Vec<String>, ConfigError> {
            env.list(key)?.iter().map(|u| http_url(key, u)).collect()
        };
        let tsa_urls = urls("ANCHOR_RFC3161_URLS")?;
        let ots_calendars = urls("ANCHOR_OTS_CALENDARS")?;
        let tsa_trust = match env.get("ANCHOR_RFC3161_CERTS_FILE")? {
            None => None,
            Some(path) => {
                let pem = std::fs::read(&path)
                    .map_err(|e| ConfigError(format!("ANCHOR_RFC3161_CERTS_FILE={path}: {e}")))?;
                Some(
                    TsaTrust::from_pem(&pem).map_err(|e| {
                        ConfigError(format!("ANCHOR_RFC3161_CERTS_FILE={path}: {e}"))
                    })?,
                )
            }
        };
        if !tsa_urls.is_empty() && tsa_trust.is_none() {
            return Err(ConfigError(
                "ANCHOR_RFC3161_URLS needs ANCHOR_RFC3161_CERTS_FILE (the TSA certificates tokens must chain to)".into(),
            ));
        }
        let secs: u64 = env.parse("ANCHOR_INTERVAL_SECS")?.unwrap_or(3_600);
        if !(10..=86_400).contains(&secs) {
            return Err(ConfigError(format!(
                "ANCHOR_INTERVAL_SECS={secs} is outside 10..=86400"
            )));
        }
        Ok(Self {
            interval: Duration::from_secs(secs),
            tsa_urls,
            tsa_trust,
            ots_calendars,
        })
    }

    pub fn enabled(&self) -> bool {
        !self.tsa_urls.is_empty() || !self.ots_calendars.is_empty()
    }

    pub fn anchors(&self) -> std::result::Result<Vec<AnyAnchor>, ConfigError> {
        let http = reqwest::Client::builder()
            .timeout(ANCHOR_HTTP_TIMEOUT)
            .user_agent(concat!("payment-workers/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ConfigError(format!("anchor HTTP client: {e}")))?;
        let mut anchors: Vec<AnyAnchor> = self
            .tsa_urls
            .iter()
            .map(|url| {
                let trust = self.tsa_trust.clone().expect("checked in from_env");
                AnyAnchor::Tsa(TsaAnchor::new(url.clone(), trust, http.clone()))
            })
            .collect();
        if !self.ots_calendars.is_empty() {
            anchors.push(AnyAnchor::Ots(OtsAnchor::new(
                self.ots_calendars.clone(),
                http,
            )));
        }
        Ok(anchors)
    }
}

/// A stored anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorRecord {
    pub id: Uuid,
    pub checkpoint_seq: i64,
    pub complete: bool,
}

async fn insert_anchor(
    pool: &PgPool,
    kind: &str,
    witness: &str,
    seq: i64,
    hash: &[u8; 32],
    stamp: &Stamp,
    upgrades: Option<Uuid>,
) -> Result<AnchorRecord> {
    let id = Uuid::now_v7();
    // Written only if the checkpoint still has the hash that was anchored.
    let inserted = sqlx::query(
        "INSERT INTO checkpoint_anchors
           (id, checkpoint_seq, checkpoint_hash, kind, witness, status, proof,
            attested_at, bitcoin_height, upgrades)
         SELECT $1, c.seq, c.checkpoint_hash, $4, $5, $6, $7,
                to_timestamp($8::double precision), $9, $10
         FROM checkpoints c WHERE c.seq = $2 AND c.checkpoint_hash = $3",
    )
    .bind(id)
    .bind(seq)
    .bind(hash.as_slice())
    .bind(kind)
    .bind(witness)
    .bind(if stamp.complete {
        "complete"
    } else {
        "pending"
    })
    .bind(&stamp.proof)
    .bind(stamp.attested_at.map(|t| t as f64))
    .bind(stamp.bitcoin_height)
    .bind(upgrades)
    .execute(pool)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Err(WorkerError::ChainBroken {
            seq,
            reason: "the checkpoint changed while it was being anchored".into(),
        });
    }
    Ok(AnchorRecord {
        id,
        checkpoint_seq: seq,
        complete: stamp.complete,
    })
}

/// Anchors checkpoint `seq` (whose verified hash is `hash`) unless this
/// witness already anchored it or a later one.
pub async fn anchor_checkpoint<A: Anchor>(
    pool: &PgPool,
    anchor: &A,
    seq: i64,
    hash: &[u8; 32],
) -> Result<Option<AnchorRecord>> {
    let last: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(checkpoint_seq) FROM checkpoint_anchors WHERE kind = $1 AND witness = $2",
    )
    .bind(anchor.kind())
    .bind(anchor.witness())
    .fetch_one(pool)
    .await?;
    if last.is_some_and(|l| l >= seq) {
        return Ok(None);
    }
    let stamp = anchor.stamp(hash).await?;
    insert_anchor(
        pool,
        anchor.kind(),
        anchor.witness(),
        seq,
        hash,
        &stamp,
        None,
    )
    .await
    .map(Some)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpgradeReport {
    pub checked: u64,
    pub upgraded: u64,
    pub failed: u64,
}

/// Tries to complete up to `limit` pending proofs of this witness. Each
/// completed proof is a NEW row naming the pending one (`upgrades`); nothing
/// is ever updated.
pub async fn upgrade_pending<A: Anchor>(
    pool: &PgPool,
    anchor: &A,
    limit: i64,
) -> Result<UpgradeReport> {
    let rows = sqlx::query(
        "SELECT p.id, p.checkpoint_seq, p.checkpoint_hash, p.proof
         FROM checkpoint_anchors p
         WHERE p.kind = $1 AND p.witness = $2 AND p.status = 'pending'
           AND p.created_at > now() - make_interval(secs => $3)
           AND NOT EXISTS (SELECT 1 FROM checkpoint_anchors u WHERE u.upgrades = p.id)
         ORDER BY p.created_at LIMIT $4",
    )
    .bind(anchor.kind())
    .bind(anchor.witness())
    .bind(OTS_UPGRADE_WINDOW.as_secs_f64())
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut report = UpgradeReport::default();
    for row in rows {
        report.checked += 1;
        let id: Uuid = row.try_get("id")?;
        let seq: i64 = row.try_get("checkpoint_seq")?;
        let hash: [u8; 32] = bytes32(&row, "checkpoint_hash", seq)?;
        let proof: Vec<u8> = row.try_get("proof")?;
        match anchor.upgrade(&hash, &proof).await {
            Ok(None) => {}
            Ok(Some(stamp)) => {
                match insert_anchor(
                    pool,
                    anchor.kind(),
                    anchor.witness(),
                    seq,
                    &hash,
                    &stamp,
                    Some(id),
                )
                .await
                {
                    Ok(_) => report.upgraded += 1,
                    // Another leader upgraded it first.
                    Err(WorkerError::Database(sqlx::Error::Database(e)))
                        if e.is_unique_violation() => {}
                    Err(e) => return Err(e),
                }
            }
            Err(e) => {
                report.failed += 1;
                tracing::warn!(checkpoint_seq = seq, witness = anchor.witness(), error = %e, "anchor upgrade failed");
            }
        }
    }
    Ok(report)
}

fn broken(seq: i64, reason: impl Into<String>) -> WorkerError {
    WorkerError::ChainBroken {
        seq,
        reason: reason.into(),
    }
}

fn bytes32(row: &PgRow, column: &str, seq: i64) -> Result<[u8; 32]> {
    row.try_get::<Vec<u8>, _>(column)?
        .try_into()
        .map_err(|_| broken(seq, format!("anchor {column} is not 32 bytes")))
}

/// Every anchor still names an existing checkpoint with the hash it anchored.
/// One cheap statement, run on every verification tick: a rewrite of anchored
/// history — even one re-signed with the legitimate key — fails here.
pub async fn check_anchor_consistency(pool: &PgPool) -> Result<()> {
    let row = sqlx::query(
        "SELECT a.checkpoint_seq, a.kind, a.witness, c.seq IS NULL AS missing
         FROM checkpoint_anchors a LEFT JOIN checkpoints c ON c.seq = a.checkpoint_seq
         WHERE c.checkpoint_hash IS DISTINCT FROM a.checkpoint_hash
         ORDER BY a.checkpoint_seq LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    match row {
        None => Ok(()),
        Some(row) => {
            let seq: i64 = row.try_get("checkpoint_seq")?;
            let witness: String = row.try_get("witness")?;
            Err(if row.try_get("missing")? {
                broken(
                    seq,
                    format!("anchored checkpoint is gone (anchored by {witness})"),
                )
            } else {
                rewritten(seq, &witness)
            })
        }
    }
}

fn rewritten(seq: i64, witness: &str) -> WorkerError {
    broken(
        seq,
        format!("checkpoint hash differs from the one anchored by {witness}: history was rewritten after it was anchored"),
    )
}

/// Where an anchor verification stopped: the last verified anchor, re-checked
/// on resume so a deleted anchor is noticed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnchorCursor {
    pub last: Option<(Uuid, i64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitcoinCommitment {
    pub checkpoint_seq: i64,
    pub height: u64,
    /// Header byte order; `bitcoin-cli getblockheader` prints it reversed.
    pub merkle_root: [u8; 32],
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnchorReport {
    pub verified: u64,
    pub rfc3161: u64,
    /// RFC 3161 tokens checked without TSA certificates (no
    /// ANCHOR_RFC3161_CERTS_FILE): imprint and self-consistency only.
    pub rfc3161_untrusted: u64,
    pub ots_pending: u64,
    pub ots_bitcoin: u64,
    /// Highest checkpoint with a complete anchor (0: none).
    pub anchored_through_seq: i64,
    /// Newest complete anchor: genTime, or when a Bitcoin proof was stored.
    pub newest_anchor_at: Option<i64>,
    /// The newest Bitcoin attestations: each still needs the block header at
    /// `height` to have this Merkle root (`ots verify`, or a node).
    pub bitcoin: Vec<BitcoinCommitment>,
}

impl AnchorReport {
    pub fn absorb(&mut self, other: AnchorReport) {
        self.verified += other.verified;
        self.rfc3161 += other.rfc3161;
        self.rfc3161_untrusted += other.rfc3161_untrusted;
        self.ots_pending += other.ots_pending;
        self.ots_bitcoin += other.ots_bitcoin;
        self.anchored_through_seq = self.anchored_through_seq.max(other.anchored_through_seq);
        self.newest_anchor_at = self.newest_anchor_at.max(other.newest_anchor_at);
        self.bitcoin.extend(other.bitcoin);
        self.bitcoin
            .sort_by_key(|b| std::cmp::Reverse(b.checkpoint_seq));
        self.bitcoin.truncate(REPORT_BITCOIN_ATTESTATIONS);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorPage {
    pub report: AnchorReport,
    pub cursor: AnchorCursor,
    pub done: bool,
}

/// Verifies one page of anchors after `cursor`: each still matches its
/// checkpoint, and its proof commits to that checkpoint hash — RFC 3161
/// tokens cryptographically (against `tsa` when given), OpenTimestamps proofs
/// by recomputing every operation up to the attestations.
pub async fn verify_anchors_page(
    pool: &PgPool,
    tsa: Option<&TsaTrust>,
    cursor: &AnchorCursor,
) -> Result<AnchorPage> {
    if let Some((id, seq)) = cursor.last {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM checkpoint_anchors WHERE id = $1)")
                .bind(id)
                .fetch_one(pool)
                .await?;
        if !exists {
            return Err(broken(
                seq,
                format!("previously verified anchor {id} was removed"),
            ));
        }
    }
    let rows = sqlx::query(
        "SELECT a.id, a.checkpoint_seq, a.checkpoint_hash, a.kind, a.witness, a.status, a.proof,
                EXTRACT(EPOCH FROM a.attested_at)::bigint AS attested_at,
                EXTRACT(EPOCH FROM a.created_at)::bigint AS created_at,
                a.bitcoin_height, a.upgrades, c.checkpoint_hash AS current_hash,
                u.checkpoint_seq AS upgraded_seq, u.kind AS upgraded_kind, u.status AS upgraded_status
         FROM checkpoint_anchors a
         LEFT JOIN checkpoints c ON c.seq = a.checkpoint_seq
         LEFT JOIN checkpoint_anchors u ON u.id = a.upgrades
         WHERE $1::uuid IS NULL OR a.id > $1
         ORDER BY a.id LIMIT $2",
    )
    .bind(cursor.last.map(|(id, _)| id))
    .bind(VERIFY_PAGE_ANCHORS)
    .fetch_all(pool)
    .await?;

    let mut report = AnchorReport::default();
    let mut cursor = *cursor;
    for row in &rows {
        verify_anchor_row(row, tsa, &mut report)?;
        cursor.last = Some((row.try_get("id")?, row.try_get("checkpoint_seq")?));
    }
    Ok(AnchorPage {
        report,
        cursor,
        done: (rows.len() as i64) < VERIFY_PAGE_ANCHORS,
    })
}

/// All anchors after `cursor`.
pub async fn verify_anchors(
    pool: &PgPool,
    tsa: Option<&TsaTrust>,
    cursor: &AnchorCursor,
) -> Result<(AnchorReport, AnchorCursor)> {
    let mut report = AnchorReport::default();
    let mut cursor = *cursor;
    loop {
        let page = verify_anchors_page(pool, tsa, &cursor).await?;
        report.absorb(page.report);
        cursor = page.cursor;
        if page.done {
            return Ok((report, cursor));
        }
    }
}

fn verify_anchor_row(row: &PgRow, tsa: Option<&TsaTrust>, report: &mut AnchorReport) -> Result<()> {
    let seq: i64 = row.try_get("checkpoint_seq")?;
    let witness: String = row.try_get("witness")?;
    let kind: String = row.try_get("kind")?;
    let status: String = row.try_get("status")?;
    let proof: Vec<u8> = row.try_get("proof")?;
    let hash = bytes32(row, "checkpoint_hash", seq)?;
    match row.try_get::<Option<Vec<u8>>, _>("current_hash")? {
        None => {
            return Err(broken(
                seq,
                format!("anchored checkpoint is gone (anchored by {witness})"),
            ))
        }
        Some(current) if current != hash => return Err(rewritten(seq, &witness)),
        Some(_) => {}
    }
    let imprint = anchor_imprint(&hash);
    let invalid =
        |e: &dyn std::fmt::Display| broken(seq, format!("{kind} anchor by {witness}: {e}"));

    let anchored_at = match (kind.as_str(), status.as_str()) {
        (KIND_RFC3161, "complete") => {
            let token = verify_response(&proof, &imprint, None, tsa).map_err(|e| invalid(&e))?;
            if row.try_get::<Option<i64>, _>("attested_at")? != Some(token.gen_time) {
                return Err(invalid(&"attested_at does not match the token's genTime"));
            }
            report.rfc3161 += 1;
            report.rfc3161_untrusted += u64::from(!token.trusted);
            Some(token.gen_time)
        }
        (KIND_OTS, status) => {
            let proof = DetachedTimestamp::parse(&proof).map_err(|e| invalid(&e))?;
            if proof.digest() != imprint {
                return Err(invalid(&"the proof is for another digest"));
            }
            let bitcoin = proof
                .timestamp
                .bitcoin_attestations()
                .map_err(|e| invalid(&e))?;
            if status == "pending" {
                if proof.timestamp.attestations().is_empty() {
                    return Err(invalid(&"the proof attests nothing"));
                }
                report.ots_pending += 1;
                None
            } else {
                let height = bitcoin.iter().map(|(h, _)| *h as i64).min();
                if height.is_none() || row.try_get::<Option<i64>, _>("bitcoin_height")? != height {
                    return Err(invalid(
                        &"bitcoin_height does not match the proof's Bitcoin attestations",
                    ));
                }
                let upgraded = (
                    row.try_get::<Option<i64>, _>("upgraded_seq")?,
                    row.try_get::<Option<String>, _>("upgraded_kind")?,
                    row.try_get::<Option<String>, _>("upgraded_status")?,
                );
                if upgraded != (Some(seq), Some(KIND_OTS.into()), Some("pending".into())) {
                    return Err(invalid(
                        &"a complete proof must upgrade a pending proof of the same checkpoint",
                    ));
                }
                report.ots_bitcoin += 1;
                report
                    .bitcoin
                    .extend(
                        bitcoin
                            .into_iter()
                            .map(|(height, merkle_root)| BitcoinCommitment {
                                checkpoint_seq: seq,
                                height,
                                merkle_root,
                            }),
                    );
                report
                    .bitcoin
                    .sort_by_key(|b| std::cmp::Reverse(b.checkpoint_seq));
                report.bitcoin.truncate(REPORT_BITCOIN_ATTESTATIONS);
                Some(row.try_get("created_at")?)
            }
        }
        _ => return Err(invalid(&format!("unknown kind/status {kind}/{status}"))),
    };
    report.verified += 1;
    if let Some(at) = anchored_at {
        report.anchored_through_seq = report.anchored_through_seq.max(seq);
        report.newest_anchor_at = report.newest_anchor_at.max(Some(at));
    }
    Ok(())
}

/// One witness's anchoring state, for metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct WitnessStatus {
    /// Highest checkpoint sent to the witness (pending or complete).
    pub submitted_through_seq: i64,
    /// Highest checkpoint with a complete anchor.
    pub anchored_through_seq: i64,
    /// Seconds since the newest anchor (genTime / stored), None without any.
    pub anchor_age_seconds: Option<f64>,
    /// Age of the oldest checkpoint not yet sent to the witness (0: none).
    pub unanchored_age_seconds: f64,
    /// Age of the oldest pending proof still waiting for Bitcoin (0: none).
    pub unconfirmed_age_seconds: f64,
}

pub async fn witness_status(pool: &PgPool, kind: &str, witness: &str) -> Result<WitnessStatus> {
    let row = sqlx::query(
        "WITH w AS (
             SELECT COALESCE(MAX(checkpoint_seq), 0) AS submitted,
                    COALESCE(MAX(checkpoint_seq) FILTER (WHERE status = 'complete'), 0) AS anchored,
                    EXTRACT(EPOCH FROM now() - MAX(COALESCE(attested_at, created_at))
                                               FILTER (WHERE status = 'complete'))::float8 AS age
             FROM checkpoint_anchors WHERE kind = $1 AND witness = $2)
         SELECT w.submitted, w.anchored, w.age,
                COALESCE(EXTRACT(EPOCH FROM now() - (
                    SELECT created_at FROM checkpoints WHERE seq > w.submitted ORDER BY seq LIMIT 1
                ))::float8, 0) AS unanchored,
                COALESCE((
                    SELECT EXTRACT(EPOCH FROM now() - MIN(p.created_at))::float8
                    FROM checkpoint_anchors p
                    WHERE p.kind = $1 AND p.witness = $2 AND p.status = 'pending'
                      AND p.created_at > now() - make_interval(secs => $3)
                      AND NOT EXISTS (SELECT 1 FROM checkpoint_anchors u WHERE u.upgrades = p.id)
                ), 0) AS unconfirmed
         FROM w",
    )
    .bind(kind)
    .bind(witness)
    .bind(OTS_UPGRADE_WINDOW.as_secs_f64())
    .fetch_one(pool)
    .await?;
    Ok(WitnessStatus {
        submitted_through_seq: row.try_get("submitted")?,
        anchored_through_seq: row.try_get("anchored")?,
        anchor_age_seconds: row.try_get("age")?,
        unanchored_age_seconds: row.try_get("unanchored")?,
        unconfirmed_age_seconds: row.try_get("unconfirmed")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(pairs: &[(&str, &str)]) -> std::result::Result<AnchorConfig, ConfigError> {
        AnchorConfig::from_env(&Env::from_pairs(pairs.iter().copied()))
    }

    #[test]
    fn anchoring_knobs_are_strict() {
        let off = parse(&[]).unwrap();
        assert!(!off.enabled());
        assert_eq!(off.interval, Duration::from_secs(3600));
        let off = parse(&[("ANCHOR_RFC3161_URLS", ""), ("ANCHOR_OTS_CALENDARS", " ")]).unwrap();
        assert!(!off.enabled());

        let ots = parse(&[(
            "ANCHOR_OTS_CALENDARS",
            "https://a.pool.opentimestamps.org/, https://b.pool.opentimestamps.org",
        )])
        .unwrap();
        assert!(ots.enabled());
        assert_eq!(
            ots.ots_calendars,
            [
                "https://a.pool.opentimestamps.org",
                "https://b.pool.opentimestamps.org"
            ]
        );
        assert_eq!(ots.anchors().unwrap().len(), 1);

        assert!(parse(&[("ANCHOR_RFC3161_URLS", "https://freetsa.org/tsr")])
            .unwrap_err()
            .0
            .contains("CERTS_FILE"));
        assert!(parse(&[("ANCHOR_RFC3161_CERTS_FILE", "/nonexistent.pem")]).is_err());
        assert!(parse(&[("ANCHOR_OTS_CALENDARS", "a.pool.opentimestamps.org")]).is_err());
        assert!(parse(&[("ANCHOR_OTS_CALENDARS", "https://x,,https://y")]).is_err());
        assert!(parse(&[("ANCHOR_INTERVAL_SECS", "0")]).is_err());
        assert!(parse(&[("ANCHOR_INTERVAL_SECS", "1h")]).is_err());
        assert!(parse(&[("ANCHOR_INTERVAL_SECS", "86401")]).is_err());

        let dir = std::env::temp_dir().join(format!("anchor-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (good, bad) = (dir.join("tsa.pem"), dir.join("bad.pem"));
        std::fs::write(&good, crypto::rfc3161::test_tsa::TestTsa::new().ca_pem()).unwrap();
        std::fs::write(
            &bad,
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let tsa = parse(&[
            (
                "ANCHOR_RFC3161_URLS",
                "https://tsa.example/tsr,http://tsa2.example",
            ),
            ("ANCHOR_RFC3161_CERTS_FILE", good.to_str().unwrap()),
            ("ANCHOR_INTERVAL_SECS", "600"),
        ])
        .unwrap();
        assert_eq!(tsa.interval, Duration::from_secs(600));
        assert_eq!(tsa.anchors().unwrap().len(), 2);
        assert!(parse(&[("ANCHOR_RFC3161_CERTS_FILE", bad.to_str().unwrap())]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
