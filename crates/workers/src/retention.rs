use crate::error::Result;
use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct RetentionConfig {
    pub outbox_sent_days: i32,
    pub idempotency_days: i32,
    pub refresh_tokens_days: i32,
    pub kyc_orphan_days: i32,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionReport {
    pub outbox: u64,
    pub idempotency_keys: u64,
    pub refresh_tokens: u64,
    pub kyc_documents: u64,
}

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
