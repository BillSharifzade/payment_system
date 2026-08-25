//! Retention pruning for operational tables that would otherwise grow forever.
//!
//! Only *operational* data is pruned. The ledger itself (`transactions`,
//! `entries`, `balances`, `checkpoints`) is append-only history and is never
//! touched, and `screening_events` is a compliance audit trail that must be
//! kept (regulators, not disk space, decide its retention).

use crate::error::Result;
use sqlx::PgPool;

/// How long each operational table keeps its rows.
#[derive(Debug, Clone)]
pub struct RetentionConfig {
    /// Published outbox rows (`sent_at` set) older than this are deleted.
    /// Unsent rows are never touched.
    pub outbox_sent_days: i32,
    /// Idempotency keys older than this are deleted. Clients must retry within
    /// this window for replay semantics; afterwards the ledger's transaction-id
    /// primary key still prevents double-posting (a very late retry gets a
    /// conflict, never a double-spend).
    pub idempotency_days: i32,
    /// Refresh tokens expired or revoked for longer than this are deleted.
    pub refresh_tokens_days: i32,
    /// Uploaded KYC documents that no submission ever cited are deleted (file
    /// and row) after this many days. Documents referenced by a submission are
    /// compliance records and are never touched.
    pub kyc_orphan_days: i32,
    /// Where the document files live (the same volume the API writes to).
    /// `None` disables document pruning — rows are kept too, so no file is
    /// ever orphaned by deleting its record.
    pub document_dir: Option<std::path::PathBuf>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            outbox_sent_days: 7,
            idempotency_days: 30,
            refresh_tokens_days: 30,
            kyc_orphan_days: 7,
            document_dir: None,
        }
    }
}

/// Rows deleted by one pruning pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionReport {
    pub outbox: u64,
    pub idempotency_keys: u64,
    pub refresh_tokens: u64,
    pub kyc_documents: u64,
}

/// Delete operational rows past their retention window. Safe to run at any
/// cadence; each statement is bounded by the age predicate, not a row count.
pub async fn prune_expired(pool: &PgPool, config: &RetentionConfig) -> Result<RetentionReport> {
    let outbox = sqlx::query(
        "DELETE FROM outbox
         WHERE sent_at IS NOT NULL AND sent_at < now() - make_interval(days => $1)",
    )
    .bind(config.outbox_sent_days)
    .execute(pool)
    .await?
    .rows_affected();

    let idempotency_keys = sqlx::query(
        "DELETE FROM idempotency_keys
         WHERE created_at < now() - make_interval(days => $1)",
    )
    .bind(config.idempotency_days)
    .execute(pool)
    .await?
    .rows_affected();

    let refresh_tokens = sqlx::query(
        "DELETE FROM refresh_tokens
         WHERE COALESCE(revoked_at, expires_at) < now() - make_interval(days => $1)",
    )
    .bind(config.refresh_tokens_days)
    .execute(pool)
    .await?
    .rows_affected();

    // KYC documents never cited by any submission: delete the file first, then
    // the row — a row without a file is harmless, a file without a row would
    // never be revisited.
    let mut kyc_documents = 0u64;
    if let Some(dir) = &config.document_dir {
        let orphans: Vec<String> = sqlx::query_scalar(
            "SELECT d.document_ref FROM kyc_documents d
             WHERE d.created_at < now() - make_interval(days => $1)
               AND NOT EXISTS (
                   SELECT 1 FROM kyc_submissions s WHERE s.document_ref = d.document_ref
               )",
        )
        .bind(config.kyc_orphan_days)
        .fetch_all(pool)
        .await?;

        for document_ref in orphans {
            match tokio::fs::remove_file(dir.join(&document_ref)).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(%document_ref, error = %e, "failed to delete orphan KYC document; keeping its row");
                    continue;
                }
            }
            sqlx::query("DELETE FROM kyc_documents WHERE document_ref = $1")
                .bind(&document_ref)
                .execute(pool)
                .await?;
            kyc_documents += 1;
        }
    }

    Ok(RetentionReport {
        outbox,
        idempotency_keys,
        refresh_tokens,
        kyc_documents,
    })
}
