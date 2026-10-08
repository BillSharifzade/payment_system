use std::collections::HashMap;

use crate::error::{Result, WorkerError};
use crate::signer::CheckpointSigner;
use crypto::{leaf_hash, merkle_root, sha256, verify_hash, Hash, SigningError, TrustedKeys};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, Row};
use uuid::Uuid;

/// Upper bounds for one verification page: checkpoints per header query and
/// transactions whose entries are loaded at once. They keep memory and every
/// statement's duration flat however long the chain grows.
pub const VERIFY_PAGE_CHECKPOINTS: i64 = 64;
pub const VERIFY_PAGE_TRANSACTIONS: i64 = 10_000;

#[derive(Debug, Clone)]
pub struct CheckpointSummary {
    pub seq: i64,
    pub from_txn_seq: i64,
    pub to_txn_seq: i64,
    pub txn_count: i64,
    pub merkle_root_hex: String,
    pub checkpoint_hash_hex: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub checkpoints_verified: u64,
    pub transactions_covered: u64,
}

/// Where a verification stopped: the last verified checkpoint. Resuming from it
/// first re-checks that this checkpoint is still the one that was verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyState {
    pub last_checkpoint_seq: i64,
    pub last_checkpoint_hash: [u8; 32],
    pub last_to_txn_seq: i64,
}

impl VerifyState {
    pub fn genesis() -> Self {
        Self {
            last_checkpoint_seq: 0,
            last_checkpoint_hash: [0u8; 32],
            last_to_txn_seq: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyPage {
    pub report: VerifyReport,
    pub state: VerifyState,
    /// No checkpoint after `state` existed when the page was read.
    pub done: bool,
}

fn compute_checkpoint_hash(prev: &[u8; 32], root: &Hash, from_seq: i64, to_seq: i64) -> Hash {
    let mut buf = Vec::with_capacity(32 + 32 + 16);
    buf.extend_from_slice(prev);
    buf.extend_from_slice(root.as_bytes());
    buf.extend_from_slice(&from_seq.to_le_bytes());
    buf.extend_from_slice(&to_seq.to_le_bytes());
    sha256(&buf)
}

fn push_entry(buf: &mut Vec<u8>, row: &PgRow) -> Result<()> {
    let account_id: Uuid = row.try_get("account_id")?;
    let direction: String = row.try_get("direction")?;
    let amount_minor: i64 = row.try_get("amount_minor")?;
    let currency: String = row.try_get("currency")?;
    buf.extend_from_slice(account_id.as_bytes());
    buf.push(if direction == "debit" { 0 } else { 1 });
    buf.extend_from_slice(&amount_minor.to_le_bytes());
    buf.extend_from_slice(currency.as_bytes());
    Ok(())
}

async fn transaction_leaves<'e, E>(executor: E, txn_ids: &[Uuid]) -> Result<Vec<Hash>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let rows = sqlx::query(
        "SELECT transaction_id, account_id, direction, amount_minor, currency
         FROM entries WHERE transaction_id = ANY($1)
         ORDER BY transaction_id, id",
    )
    .bind(txn_ids)
    .fetch_all(executor)
    .await?;

    let mut bufs: HashMap<Uuid, Vec<u8>> = HashMap::with_capacity(txn_ids.len());
    for row in rows {
        let txn_id: Uuid = row.try_get("transaction_id")?;
        let buf = bufs
            .entry(txn_id)
            .or_insert_with(|| txn_id.as_bytes().to_vec());
        push_entry(buf, &row)?;
    }

    Ok(txn_ids
        .iter()
        .map(|id| {
            let buf = bufs.remove(id).unwrap_or_else(|| id.as_bytes().to_vec());
            leaf_hash(&buf)
        })
        .collect())
}

/// Leaves of every transaction with `sealed_seq` in `from..=to`, in sealed
/// order, built exactly as the sealer built them (an entry-less transaction's
/// leaf is its id alone).
async fn sealed_leaves(pool: &PgPool, from: i64, to: i64) -> Result<Vec<(i64, Hash)>> {
    let rows = sqlx::query(
        "SELECT t.sealed_seq, t.id, e.account_id, e.direction, e.amount_minor, e.currency
         FROM transactions t LEFT JOIN entries e ON e.transaction_id = t.id
         WHERE t.sealed_seq BETWEEN $1 AND $2
         ORDER BY t.sealed_seq, e.id",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;

    let mut leaves = Vec::new();
    let mut current: Option<(i64, Vec<u8>)> = None;
    for row in &rows {
        let seq: i64 = row.try_get("sealed_seq")?;
        if current.as_ref().is_none_or(|(s, _)| *s != seq) {
            if let Some((s, buf)) = current.take() {
                leaves.push((s, leaf_hash(&buf)));
            }
            let id: Uuid = row.try_get("id")?;
            current = Some((seq, id.as_bytes().to_vec()));
        }
        if row.try_get::<Option<Uuid>, _>("account_id")?.is_some() {
            push_entry(&mut current.as_mut().expect("set above").1, row)?;
        }
    }
    if let Some((s, buf)) = current {
        leaves.push((s, leaf_hash(&buf)));
    }
    Ok(leaves)
}

async fn latest_checkpoint(pool: &PgPool) -> Result<(i64, i64, [u8; 32])> {
    let row = sqlx::query(
        "SELECT seq, to_txn_seq, checkpoint_hash FROM checkpoints ORDER BY seq DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;

    match row {
        None => Ok((0, 0, [0u8; 32])),
        Some(row) => {
            let seq: i64 = row.try_get("seq")?;
            let to_txn_seq: i64 = row.try_get("to_txn_seq")?;
            let hash_vec: Vec<u8> = row.try_get("checkpoint_hash")?;
            let hash: [u8; 32] = hash_vec
                .try_into()
                .map_err(|_| WorkerError::DataIntegrity("checkpoint_hash not 32 bytes".into()))?;
            Ok((seq, to_txn_seq, hash))
        }
    }
}

pub async fn seal_next_batch<S: CheckpointSigner>(
    pool: &PgPool,
    signer: &S,
    batch_size: i64,
) -> Result<Option<CheckpointSummary>> {
    let (last_cp_seq, last_to_seq, prev_hash) = latest_checkpoint(pool).await?;

    let mut db = pool.begin().await?;

    // `t.sealed_seq IS NULL` is re-checked against the latest row version if a
    // concurrent sealer got there first, so a stale batch stamps nothing.
    let rows = sqlx::query(
        "WITH batch AS (
             SELECT id, row_number() OVER (ORDER BY seq) AS rn
             FROM transactions
             WHERE sealed_seq IS NULL
             ORDER BY seq
             LIMIT $2
         )
         UPDATE transactions t
         SET sealed_seq = $1 + b.rn
         FROM batch b
         WHERE t.id = b.id AND t.sealed_seq IS NULL
         RETURNING t.id, t.sealed_seq",
    )
    .bind(last_to_seq)
    .bind(batch_size)
    .fetch_all(&mut *db)
    .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    let mut batch: Vec<(i64, Uuid)> = Vec::with_capacity(rows.len());
    for row in &rows {
        let sealed_seq: i64 = row.try_get("sealed_seq")?;
        let id: Uuid = row.try_get("id")?;
        batch.push((sealed_seq, id));
    }
    batch.sort_unstable_by_key(|(seq, _)| *seq);

    let from_seq = batch.first().expect("non-empty").0;
    let to_seq = batch.last().expect("non-empty").0;
    if to_seq - from_seq + 1 != batch.len() as i64 {
        return Err(WorkerError::DataIntegrity(format!(
            "sealed range {from_seq}..={to_seq} is not dense ({} rows)",
            batch.len()
        )));
    }
    let txn_ids: Vec<Uuid> = batch.iter().map(|(_, id)| *id).collect();

    let leaves = transaction_leaves(&mut *db, &txn_ids).await?;
    let root = merkle_root(&leaves).expect("non-empty leaves");
    let checkpoint_hash = compute_checkpoint_hash(&prev_hash, &root, from_seq, to_seq);
    // An external signer answers over the network while this transaction
    // holds the batch; its signature is checked before anything is written.
    let public_key = signer.public_key();
    let signature = signer.sign(&checkpoint_hash).await?;
    verify_hash(&public_key, &checkpoint_hash, &signature)?;
    let new_seq = last_cp_seq + 1;

    sqlx::query(
        "INSERT INTO checkpoints
           (id, seq, from_txn_seq, to_txn_seq, txn_count,
            merkle_root, prev_checkpoint_hash, checkpoint_hash, signature, public_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(Uuid::new_v4())
    .bind(new_seq)
    .bind(from_seq)
    .bind(to_seq)
    .bind(batch.len() as i64)
    .bind(root.as_bytes().to_vec())
    .bind(prev_hash.to_vec())
    .bind(checkpoint_hash.as_bytes().to_vec())
    .bind(signature.to_vec())
    .bind(public_key.to_vec())
    .execute(&mut *db)
    .await?;

    db.commit().await?;

    Ok(Some(CheckpointSummary {
        seq: new_seq,
        from_txn_seq: from_seq,
        to_txn_seq: to_seq,
        txn_count: batch.len() as i64,
        merkle_root_hex: root.to_hex(),
        checkpoint_hash_hex: checkpoint_hash.to_hex(),
    }))
}

pub async fn seal_all<S: CheckpointSigner>(
    pool: &PgPool,
    signer: &S,
    batch_size: i64,
) -> Result<u64> {
    let mut count = 0;
    while seal_next_batch(pool, signer, batch_size).await?.is_some() {
        count += 1;
    }
    Ok(count)
}

/// Full verification of the chain from genesis against `trusted`.
pub async fn verify_chain(pool: &PgPool, trusted: &TrustedKeys) -> Result<VerifyReport> {
    let (report, _) = verify_chain_from(pool, trusted, &VerifyState::genesis()).await?;
    Ok(report)
}

/// Verifies every checkpoint after `state`, page by page.
pub async fn verify_chain_from(
    pool: &PgPool,
    trusted: &TrustedKeys,
    state: &VerifyState,
) -> Result<(VerifyReport, VerifyState)> {
    let mut report = VerifyReport::default();
    let mut state = *state;
    loop {
        let page = verify_chain_page(pool, trusted, &state).await?;
        report.checkpoints_verified += page.report.checkpoints_verified;
        report.transactions_covered += page.report.transactions_covered;
        state = page.state;
        if page.done {
            return Ok((report, state));
        }
    }
}

struct Header {
    seq: i64,
    from: i64,
    to: i64,
    root: [u8; 32],
    hash: [u8; 32],
}

fn broken(seq: i64, reason: impl Into<String>) -> WorkerError {
    WorkerError::ChainBroken {
        seq,
        reason: reason.into(),
    }
}

fn bytes<const N: usize>(row: &PgRow, column: &str, seq: i64) -> Result<[u8; N]> {
    row.try_get::<Vec<u8>, _>(column)?
        .try_into()
        .map_err(|_| broken(seq, format!("{column} is not {N} bytes")))
}

/// Authenticates a checkpoint row without touching the ledger: position in
/// the chain, the hash over (prev, root, range), and a signature by a TRUSTED
/// key. Only then is its range used to load transactions, so a forged row can
/// neither pass nor make the verifier read an arbitrary range.
fn authenticate(
    row: &PgRow,
    expected_seq: i64,
    expected_prev: &[u8; 32],
    trusted: &TrustedKeys,
) -> Result<Header> {
    let seq: i64 = row.try_get("seq")?;
    if seq != expected_seq {
        return Err(broken(seq, format!("expected seq {expected_seq}")));
    }
    let from: i64 = row.try_get("from_txn_seq")?;
    let to: i64 = row.try_get("to_txn_seq")?;
    let root: [u8; 32] = bytes(row, "merkle_root", seq)?;
    let prev: [u8; 32] = bytes(row, "prev_checkpoint_hash", seq)?;
    let hash: [u8; 32] = bytes(row, "checkpoint_hash", seq)?;
    let signature: [u8; 64] = bytes(row, "signature", seq)?;
    let public_key: [u8; 32] = bytes(row, "public_key", seq)?;

    if &prev != expected_prev {
        return Err(broken(
            seq,
            "prev_checkpoint_hash does not match previous checkpoint",
        ));
    }
    if from < 1 || to < from {
        return Err(broken(seq, format!("invalid range {from}..={to}")));
    }
    let recomputed = compute_checkpoint_hash(&prev, &Hash::from_bytes(root), from, to);
    if recomputed.as_bytes() != &hash {
        return Err(broken(seq, "checkpoint hash mismatch"));
    }
    trusted
        .verify(seq, &public_key, &recomputed, &signature)
        .map_err(|e| match e {
            SigningError::UntrustedKey => broken(seq, "untrusted signing key"),
            SigningError::RetiredKey(last) => broken(
                seq,
                format!("signed by a key retired after checkpoint {last}"),
            ),
            _ => broken(seq, "invalid signature"),
        })?;
    Ok(Header {
        seq,
        from,
        to,
        root,
        hash,
    })
}

/// Verifies the next bounded page of checkpoints after `state`. The first
/// error is the earliest broken checkpoint.
pub async fn verify_chain_page(
    pool: &PgPool,
    trusted: &TrustedKeys,
    state: &VerifyState,
) -> Result<VerifyPage> {
    let rows = sqlx::query(
        "SELECT seq, from_txn_seq, to_txn_seq, merkle_root, prev_checkpoint_hash,
                checkpoint_hash, signature, public_key
         FROM checkpoints WHERE seq >= $1 ORDER BY seq LIMIT $2",
    )
    .bind(state.last_checkpoint_seq)
    .bind(VERIFY_PAGE_CHECKPOINTS + 1)
    .fetch_all(pool)
    .await?;

    let mut rows = rows.as_slice();
    if state.last_checkpoint_seq > 0 {
        let anchor_ok = match rows.first() {
            Some(row) => {
                row.try_get::<i64, _>("seq")? == state.last_checkpoint_seq
                    && row.try_get::<Vec<u8>, _>("checkpoint_hash")? == state.last_checkpoint_hash
            }
            None => false,
        };
        if !anchor_ok {
            return Err(broken(
                state.last_checkpoint_seq,
                "previously verified checkpoint was altered or removed",
            ));
        }
        rows = &rows[1..];
    }
    let mut done = (rows.len() as i64) < VERIFY_PAGE_CHECKPOINTS;
    if !done {
        rows = &rows[..VERIFY_PAGE_CHECKPOINTS as usize];
    }

    let mut headers = Vec::with_capacity(rows.len());
    let mut header_error = None;
    let mut budget = VERIFY_PAGE_TRANSACTIONS;
    for row in rows {
        let (expected_seq, expected_prev) = headers.last().map_or(
            (state.last_checkpoint_seq + 1, state.last_checkpoint_hash),
            |h: &Header| (h.seq + 1, h.hash),
        );
        if !headers.is_empty() && budget <= 0 {
            done = false;
            break;
        }
        match authenticate(row, expected_seq, &expected_prev, trusted) {
            Ok(h) => {
                budget -= h.to - h.from + 1;
                headers.push(h);
            }
            Err(e) => {
                header_error = Some(e);
                break;
            }
        }
    }

    let mut report = VerifyReport::default();
    let mut verified = *state;
    if let Some(last) = headers.last() {
        // Loaded from just past the previous checkpoint so a sealed transaction
        // in a gap between two ranges is caught instead of skipped.
        let leaves = sealed_leaves(pool, state.last_to_txn_seq + 1, last.to).await?;
        let mut next = 0;
        for h in &headers {
            if let Some((seq, _)) = leaves.get(next).filter(|(seq, _)| *seq < h.from) {
                return Err(broken(
                    h.seq,
                    format!("transaction at sealed_seq {seq} is not covered by any checkpoint"),
                ));
            }
            let start = next;
            while leaves.get(next).is_some_and(|(seq, _)| *seq <= h.to) {
                next += 1;
            }
            let covered: Vec<Hash> = leaves[start..next].iter().map(|(_, l)| *l).collect();
            let root = merkle_root(&covered)
                .ok_or_else(|| broken(h.seq, "checkpoint covers no transactions"))?;
            if root.as_bytes() != &h.root {
                return Err(broken(
                    h.seq,
                    "Merkle root does not match the transactions (ledger tampering)",
                ));
            }
            report.checkpoints_verified += 1;
            report.transactions_covered += covered.len() as u64;
            verified = VerifyState {
                last_checkpoint_seq: h.seq,
                last_checkpoint_hash: h.hash,
                last_to_txn_seq: h.to,
            };
        }
    }
    if let Some(e) = header_error {
        return Err(e);
    }

    if done {
        // Sealing stamps sealed_seq and writes the checkpoint atomically, so a
        // sealed transaction beyond the newest checkpoint was put there by hand
        // (or its checkpoint was deleted). One statement, one snapshot.
        let stray: Option<i64> = sqlx::query_scalar(
            "SELECT MIN(sealed_seq) FROM transactions
             WHERE sealed_seq > COALESCE(
                 (SELECT to_txn_seq FROM checkpoints ORDER BY seq DESC LIMIT 1), 0)",
        )
        .fetch_one(pool)
        .await?;
        if let Some(seq) = stray {
            return Err(broken(
                verified.last_checkpoint_seq,
                format!(
                    "transaction at sealed_seq {seq} is sealed but not covered by any checkpoint"
                ),
            ));
        }
    }

    Ok(VerifyPage {
        report,
        state: verified,
        done,
    })
}
