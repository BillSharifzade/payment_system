use std::time::Duration;

use crate::error::Result;
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct OutboxEvent {
    pub id: Uuid,
    pub aggregate_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
}

pub trait EventPublisher: Send + Sync {
    fn publish(
        &self,
        event: &OutboxEvent,
    ) -> impl std::future::Future<Output = std::result::Result<(), PublishError>> + Send;

    fn publish_batch(
        &self,
        events: &[OutboxEvent],
    ) -> impl std::future::Future<Output = std::result::Result<usize, PublishError>> + Send {
        async move {
            let mut n = 0;
            for e in events {
                self.publish(e).await?;
                n += 1;
            }
            Ok(n)
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("failed to publish event: {0}")]
pub struct PublishError(pub String);

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

pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct NatsPublisher {
    js: async_nats::jetstream::Context,
    subject_prefix: String,
    stream: String,
}

impl NatsPublisher {
    pub fn stream_name(subject_prefix: &str) -> String {
        subject_prefix
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect()
    }

    pub fn stream(&self) -> &str {
        &self.stream
    }

    pub async fn connect(
        url: &str,
        subject_prefix: &str,
    ) -> std::result::Result<Self, PublishError> {
        let client = async_nats::ConnectOptions::new()
            .retry_on_initial_connect()
            .connect(url)
            .await
            .map_err(|e| PublishError(format!("nats connect: {e}")))?;
        let js = async_nats::jetstream::new(client);
        let stream = Self::stream_name(subject_prefix);
        js.get_or_create_stream(async_nats::jetstream::stream::Config {
            name: stream.clone(),
            subjects: vec![format!("{subject_prefix}.>")],
            retention: async_nats::jetstream::stream::RetentionPolicy::Limits,
            duplicate_window: Duration::from_secs(600),
            ..Default::default()
        })
        .await
        .map_err(|e| PublishError(format!("jetstream stream {stream}: {e}")))?;
        Ok(Self {
            js,
            subject_prefix: subject_prefix.to_string(),
            stream,
        })
    }

    fn message(
        &self,
        event: &OutboxEvent,
    ) -> std::result::Result<(String, async_nats::HeaderMap, bytes::Bytes), PublishError> {
        let subject = format!("{}.{}", self.subject_prefix, event.event_type);
        let payload =
            serde_json::to_vec(&event.payload).map_err(|e| PublishError(e.to_string()))?;
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(async_nats::header::NATS_MESSAGE_ID, event.id.to_string());
        Ok((subject, headers, payload.into()))
    }
}

impl EventPublisher for NatsPublisher {
    async fn publish(&self, event: &OutboxEvent) -> std::result::Result<(), PublishError> {
        let (subject, headers, payload) = self.message(event)?;
        let ack = self
            .js
            .publish_with_headers(subject, headers, payload)
            .await
            .map_err(|e| PublishError(format!("nats publish: {e}")))?;
        ack.await
            .map_err(|e| PublishError(format!("jetstream ack: {e}")))?;
        Ok(())
    }

    async fn publish_batch(
        &self,
        events: &[OutboxEvent],
    ) -> std::result::Result<usize, PublishError> {
        let mut acks = Vec::with_capacity(events.len());
        for event in events {
            let (subject, headers, payload) = self.message(event)?;
            match self
                .js
                .publish_with_headers(subject, headers, payload)
                .await
            {
                Ok(ack) => acks.push(ack),
                Err(e) => {
                    tracing::warn!(error = %e, "nats publish failed; cutting batch short");
                    break;
                }
            }
        }
        let mut accepted = 0;
        for ack in acks {
            match ack.await {
                Ok(_) => accepted += 1,
                Err(e) => {
                    tracing::warn!(error = %e, "jetstream ack failed; cutting batch short");
                    break;
                }
            }
        }
        Ok(accepted)
    }
}

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
         ORDER BY created_at, id
         LIMIT $1
         FOR UPDATE SKIP LOCKED",
    )
    .bind(batch_size)
    .fetch_all(&mut *tx)
    .await?;
    if rows.is_empty() {
        tx.commit().await?;
        return Ok(0);
    }

    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        events.push(OutboxEvent {
            id: row.try_get("id")?,
            aggregate_id: row.try_get("aggregate_id")?,
            event_type: row.try_get("event_type")?,
            payload: row.try_get("payload")?,
        });
    }

    let accepted = match tokio::time::timeout(
        PUBLISH_TIMEOUT
            .mul_f64(events.len().max(1) as f64 / 100.0)
            .max(PUBLISH_TIMEOUT),
        publisher.publish_batch(&events),
    )
    .await
    {
        Ok(Ok(n)) => n,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "publish failed; no rows marked this pass");
            0
        }
        Err(_) => {
            tracing::warn!("publish timed out; rows stay unsent for the next pass");
            0
        }
    };

    if accepted > 0 {
        let ids: Vec<Uuid> = events[..accepted].iter().map(|e| e.id).collect();
        sqlx::query("UPDATE outbox SET sent_at = now() WHERE id = ANY($1)")
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(accepted as u64)
}

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

pub async fn outbox_lag(pool: &PgPool) -> Result<(i64, f64)> {
    let row = sqlx::query(
        "SELECT COUNT(*)::BIGINT AS unsent,
                COALESCE(EXTRACT(EPOCH FROM (now() - MIN(created_at))), 0)::FLOAT8 AS oldest_age
         FROM outbox WHERE sent_at IS NULL",
    )
    .fetch_one(pool)
    .await?;
    Ok((row.try_get("unsent")?, row.try_get("oldest_age")?))
}

#[cfg(test)]
mod tests {
    use super::NatsPublisher;

    #[test]
    fn stream_name_follows_the_prefix() {
        assert_eq!(NatsPublisher::stream_name("payments"), "PAYMENTS");
        assert_eq!(NatsPublisher::stream_name("test-abc"), "TEST-ABC");
        assert_eq!(NatsPublisher::stream_name("a.b*c>d e"), "A_B_C_D_E");
    }
}
