//! The checkpoint sealer and its verifier.
//!
//! The sealer batches not-yet-sealed transactions into a Merkle root, chains it
//! to the previous checkpoint's hash, signs the result with Ed25519, and stores
//! it. The verifier independently recomputes everything from the raw ledger and
//! checks the signed chain — so tampering with any historical transaction, or
//! with a checkpoint itself, is detected.
//!
//! # Why checkpoints range over `sealed_seq`, not `seq`
//!
//! `transactions.seq` is assigned at INSERT time, but rows *commit* in a
//! different order, and the sealer can only see committed rows. Sealing by raw
//! `seq` therefore races with in-flight posts: a transaction with a lower `seq`
//! than a sealed checkpoint's upper bound can commit *after* sealing, landing
//! inside a range whose Merkle root was computed without it — it would never be
//! sealed, and verification would report false tampering. Instead, the sealer
//! (a single writer by design) assigns each committed transaction a dense
//! `sealed_seq` in the same database transaction that writes the checkpoint.
//! A checkpoint's covered set is fixed forever at the moment it is created.

use std::collections::HashMap;

use crate::error::{Result, WorkerError};
use crypto::{leaf_hash, merkle_root, sha256, verify_hash, Hash, Sealer};
use sqlx::{PgPool, Postgres, Row};
use uuid::Uuid;

/// Summary of a checkpoint that was just sealed.
#[derive(Debug, Clone)]
pub struct CheckpointSummary {
    pub seq: i64,
    pub from_txn_seq: i64,
    pub to_txn_seq: i64,
    pub txn_count: i64,
    pub merkle_root_hex: String,
    pub checkpoint_hash_hex: String,
}

/// Result of verifying (part of) the checkpoint chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub checkpoints_verified: u64,
    pub transactions_covered: u64,
}

/// Where a verification pass stopped: the last verified checkpoint's seq and
/// hash. Feed it back into [`verify_chain_from`] to verify only what's new.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyState {
    pub last_checkpoint_seq: i64,
    pub last_checkpoint_hash: [u8; 32],
}

impl VerifyState {
    /// The genesis state: nothing verified yet.
    pub fn genesis() -> Self {
        Self {
            last_checkpoint_seq: 0,
            last_checkpoint_hash: [0u8; 32],
        }
    }
}

/// Bind together the bytes a checkpoint commits to, in a fixed layout, and hash.
fn compute_checkpoint_hash(prev: &[u8; 32], root: &Hash, from_seq: i64, to_seq: i64) -> Hash {
    let mut buf = Vec::with_capacity(32 + 32 + 16);
    buf.extend_from_slice(prev);
    buf.extend_from_slice(root.as_bytes());
    buf.extend_from_slice(&from_seq.to_le_bytes());
    buf.extend_from_slice(&to_seq.to_le_bytes());
    sha256(&buf)
}

/// Compute the Merkle leaves for a batch of transactions (in the order given):
/// each leaf hashes the transaction's id and all of its entries in a canonical
/// (entry-id-ordered) layout. Any change to the postings changes the leaf.
///
/// One query for the whole batch — the per-transaction variant was an N+1 that
/// made sealing O(batch) round trips.
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

    // Group each transaction's canonical byte buffer. Rows arrive ordered by
    // entry id within a transaction, matching the layout sealed historically.
    let mut bufs: HashMap<Uuid, Vec<u8>> = HashMap::with_capacity(txn_ids.len());
    for row in rows {
        let txn_id: Uuid = row.try_get("transaction_id")?;
        let account_id: Uuid = row.try_get("account_id")?;
        let direction: String = row.try_get("direction")?;
        let amount_minor: i64 = row.try_get("amount_minor")?;
        let currency: String = row.try_get("currency")?;

        let buf = bufs
            .entry(txn_id)
            .or_insert_with(|| txn_id.as_bytes().to_vec());
        buf.extend_from_slice(account_id.as_bytes());
        buf.push(if direction == "debit" { 0 } else { 1 });
        buf.extend_from_slice(&amount_minor.to_le_bytes());
        buf.extend_from_slice(currency.as_bytes());
    }

    Ok(txn_ids
        .iter()
        .map(|id| {
            let buf = bufs.remove(id).unwrap_or_else(|| id.as_bytes().to_vec());
            leaf_hash(&buf)
        })
        .collect())
}

/// Read the latest checkpoint's (seq, to_txn_seq, checkpoint_hash), or the
/// genesis defaults (0, 0, 32 zero bytes) if there are none yet.
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

/// Seal the next batch (up to `batch_size` transactions). Returns `None` when
/// there is nothing new to seal.
///
/// The `sealed_seq` assignment and the checkpoint row are written in ONE
/// database transaction, so a crash can never leave transactions assigned but
/// uncovered (or vice versa).
pub async fn seal_next_batch(
    pool: &PgPool,
    sealer: &Sealer,
    batch_size: i64,
) -> Result<Option<CheckpointSummary>> {
    let (last_cp_seq, last_to_seq, prev_hash) = latest_checkpoint(pool).await?;

    let mut db = pool.begin().await?;

    // Claim the next batch of committed-but-unsealed transactions, in insert
    // order, assigning each a dense sealed_seq. Only committed rows are visible
    // here, so the covered set is final the moment we commit.
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
         WHERE t.id = b.id
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
    let txn_ids: Vec<Uuid> = batch.iter().map(|(_, id)| *id).collect();

    let leaves = transaction_leaves(&mut *db, &txn_ids).await?;
    let root = merkle_root(&leaves).expect("non-empty leaves");
    let checkpoint_hash = compute_checkpoint_hash(&prev_hash, &root, from_seq, to_seq);
    let signature = sealer.sign_hash(&checkpoint_hash);
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
    .bind(sealer.public_key_bytes().to_vec())
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

/// Seal everything outstanding, batch by batch. Returns the number of
/// checkpoints written.
pub async fn seal_all(pool: &PgPool, sealer: &Sealer, batch_size: i64) -> Result<u64> {
    let mut count = 0;
    while seal_next_batch(pool, sealer, batch_size).await?.is_some() {
        count += 1;
    }
    Ok(count)
}

/// Independently verify the entire checkpoint chain against the raw ledger,
/// from genesis. See [`verify_chain_from`] for the incremental variant.
pub async fn verify_chain(pool: &PgPool) -> Result<VerifyReport> {
    let (report, _) = verify_chain_from(pool, &VerifyState::genesis()).await?;
    Ok(report)
}

/// Verify every checkpoint after `state` against the raw ledger.
///
/// For each checkpoint, in order, this re-derives the Merkle root from the
/// transactions it covers (by `sealed_seq`), recomputes the checkpoint hash,
/// checks linkage to the previous checkpoint, and verifies the Ed25519
/// signature. Any discrepancy is a [`WorkerError::ChainBroken`] — a tamper
/// alarm. Returns the report plus the new [`VerifyState`], so callers can keep
/// verification incremental instead of re-reading all of history every pass.
pub async fn verify_chain_from(
    pool: &PgPool,
    state: &VerifyState,
) -> Result<(VerifyReport, VerifyState)> {
    let checkpoints = sqlx::query(
        "SELECT seq, from_txn_seq, to_txn_seq, merkle_root, prev_checkpoint_hash,
                checkpoint_hash, signature, public_key
         FROM checkpoints WHERE seq > $1 ORDER BY seq",
    )
    .bind(state.last_checkpoint_seq)
    .fetch_all(pool)
    .await?;

    let mut expected_prev = state.last_checkpoint_hash;
    let mut last_seq = state.last_checkpoint_seq;
    let mut transactions_covered = 0u64;

    for (expected_seq, row) in (state.last_checkpoint_seq + 1..).zip(&checkpoints) {
        let seq: i64 = row.try_get("seq")?;
        let from_seq: i64 = row.try_get("from_txn_seq")?;
        let to_seq: i64 = row.try_get("to_txn_seq")?;
        let stored_root: Vec<u8> = row.try_get("merkle_root")?;
        let prev: Vec<u8> = row.try_get("prev_checkpoint_hash")?;
        let stored_hash: Vec<u8> = row.try_get("checkpoint_hash")?;
        let signature: Vec<u8> = row.try_get("signature")?;
        let public_key: Vec<u8> = row.try_get("public_key")?;

        if seq != expected_seq {
            return Err(WorkerError::ChainBroken {
                seq,
                reason: format!("expected seq {expected_seq}"),
            });
        }
        if prev != expected_prev {
            return Err(WorkerError::ChainBroken {
                seq,
                reason: "prev_checkpoint_hash does not match previous checkpoint".into(),
            });
        }

        // Re-derive the Merkle root from the actual transactions in range.
        let txn_rows = sqlx::query(
            "SELECT id FROM transactions WHERE sealed_seq BETWEEN $1 AND $2 ORDER BY sealed_seq",
        )
        .bind(from_seq)
        .bind(to_seq)
        .fetch_all(pool)
        .await?;
        let mut txn_ids = Vec::with_capacity(txn_rows.len());
        for r in &txn_rows {
            txn_ids.push(r.try_get::<Uuid, _>("id")?);
        }
        let leaves = transaction_leaves(pool, &txn_ids).await?;
        let recomputed_root = merkle_root(&leaves).ok_or(WorkerError::ChainBroken {
            seq,
            reason: "checkpoint covers no transactions".into(),
        })?;
        if recomputed_root.as_bytes().as_slice() != stored_root.as_slice() {
            return Err(WorkerError::ChainBroken {
                seq,
                reason: "Merkle root does not match the transactions (ledger tampering)".into(),
            });
        }

        // Recompute and check the signed checkpoint hash.
        let prev_arr: [u8; 32] = prev
            .clone()
            .try_into()
            .map_err(|_| WorkerError::DataIntegrity("prev hash not 32 bytes".into()))?;
        let recomputed_hash =
            compute_checkpoint_hash(&prev_arr, &recomputed_root, from_seq, to_seq);
        if recomputed_hash.as_bytes().as_slice() != stored_hash.as_slice() {
            return Err(WorkerError::ChainBroken {
                seq,
                reason: "checkpoint hash mismatch".into(),
            });
        }

        // Verify the signature.
        let pk: [u8; 32] = public_key
            .try_into()
            .map_err(|_| WorkerError::DataIntegrity("public_key not 32 bytes".into()))?;
        let sig: [u8; 64] = signature
            .try_into()
            .map_err(|_| WorkerError::DataIntegrity("signature not 64 bytes".into()))?;
        verify_hash(&pk, &recomputed_hash, &sig).map_err(|_| WorkerError::ChainBroken {
            seq,
            reason: "invalid signature".into(),
        })?;

        transactions_covered += txn_ids.len() as u64;
        last_seq = seq;
        expected_prev = stored_hash
            .try_into()
            .map_err(|_| WorkerError::DataIntegrity("checkpoint hash not 32 bytes".into()))?;
    }

    Ok((
        VerifyReport {
            checkpoints_verified: checkpoints.len() as u64,
            transactions_covered,
        },
        VerifyState {
            last_checkpoint_seq: last_seq,
            last_checkpoint_hash: expected_prev,
        },
    ))
}
