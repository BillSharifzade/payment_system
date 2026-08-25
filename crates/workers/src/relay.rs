//! The outbox relay (DESIGN.md §5.4).
//!
//! Reads unsent rows from the `outbox` table, publishes them, and marks them
//! sent. Delivery is **at-least-once**: we publish *then* mark sent, so a crash
//! in between re-publishes (consumers must therefore be idempotent — they key on
//! the transaction id). Rows are claimed with `FOR UPDATE SKIP LOCKED`, so
//! multiple relay instances can run concurrently without stepping on each other.
//!
//! The transport is abstracted behind [`EventPublisher`]: today a logging
//! publisher; a NATS/JetStream publisher slots in later without changing the
//! relay logic.

use crate::error::Result;
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// An event read from the outbox, ready to publish.
#[derive(Debug, Clone)]
pub struct OutboxEvent {
    pub id: Uuid,
    pub aggregate_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
}

/// Where relayed events go. Implementations must be safe to retry (at-least-once).
pub trait EventPublisher {
    fn publish(
        &self,
        event: &OutboxEvent,
    ) -> impl std::future::Future<Output = std::result::Result<(), PublishError>> + Send;
}

#[derive(Debug, thiserror::Error)]
#[error("failed to publish event: {0}")]
pub struct PublishError(pub String);

/// A publisher that simply logs each event. Useful for dev and tests.
pub struct LoggingPublisher;

impl EventPublisher for LoggingPublisher {
    async fn publish(&self, event: &OutboxEvent) -> std::result::Result<(), PublishError> {
        tracing::info!(
            event_id = %event.id,
            aggregate_id = %event.aggregate_id,
            event_type = %event.event_type,
            "publishing outbox event"
        );
        Ok(())
    }
}

/// Publishes events to NATS. Each event goes to subject `{prefix}.{event_type}`
/// (e.g. `payments.transaction.posted`) with the JSON payload as the body.
#[derive(Clone)]
pub struct NatsPublisher {
    client: async_nats::Client,
    subject_prefix: String,
}

impl NatsPublisher {
    /// Connect to a NATS server (e.g. `nats://localhost:4222`).
    pub async fn connect(
        url: &str,
        subject_prefix: &str,
    ) -> std::result::Result<Self, PublishError> {
        let client = async_nats::connect(url)
            .await
            .map_err(|e| PublishError(format!("nats connect: {e}")))?;
        Ok(Self {
            client,
            subject_prefix: subject_prefix.to_string(),
        })
    }
}

impl EventPublisher for NatsPublisher {
    async fn publish(&self, event: &OutboxEvent) -> std::result::Result<(), PublishError> {
        let subject = format!("{}.{}", self.subject_prefix, event.event_type);
        let payload =
            serde_json::to_vec(&event.payload).map_err(|e| PublishError(e.to_string()))?;
        self.client
            .publish(subject, payload.into())
            .await
            .map_err(|e| PublishError(format!("nats publish: {e}")))?;
        // Ensure the message is on the wire before we mark the row sent.
        self.client
            .flush()
            .await
            .map_err(|e| PublishError(format!("nats flush: {e}")))?;
        Ok(())
    }
}

/// Relay up to `batch_size` unsent events. Returns how many were published.
///
/// Each row is locked with `SKIP LOCKED` so concurrent relays don't double-send,
/// published, then marked `sent_at`. A publish failure leaves the row unsent for
/// the next pass (the transaction commits the successfully-sent rows only).
pub async fn relay_once<P: EventPublisher>(
    pool: &PgPool,
    publisher: &P,
    batch_size: i64,
) -> Result<u64> {
    let mut tx = pool.begin().await?;

    let rows = sqlx::query(
        "SELECT id, aggregate_id, event_type, payload
         FROM outbox
         WHERE sent_at IS NULL
         ORDER BY created_at
         LIMIT $1
         FOR UPDATE SKIP LOCKED",
    )
    .bind(batch_size)
    .fetch_all(&mut *tx)
    .await?;

    let mut published = 0u64;
    for row in rows {
        let event = OutboxEvent {
            id: row.try_get("id")?,
            aggregate_id: row.try_get("aggregate_id")?,
            event_type: row.try_get("event_type")?,
            payload: row.try_get("payload")?,
        };

        // If publishing fails, stop the batch; the locked rows stay unsent and
        // are retried next pass. We still commit the ones already marked.
        if publisher.publish(&event).await.is_err() {
            break;
        }

        sqlx::query("UPDATE outbox SET sent_at = now() WHERE id = $1")
            .bind(event.id)
            .execute(&mut *tx)
            .await?;
        published += 1;
    }

    tx.commit().await?;
    Ok(published)
}

/// Relay everything outstanding, batch by batch. Returns total published.
pub async fn relay_all<P: EventPublisher>(
    pool: &PgPool,
    publisher: &P,
    batch_size: i64,
) -> Result<u64> {
    let mut total = 0;
    loop {
        let n = relay_once(pool, publisher, batch_size).await?;
        total += n;
        if n < batch_size as u64 {
            break;
        }
    }
    Ok(total)
}
