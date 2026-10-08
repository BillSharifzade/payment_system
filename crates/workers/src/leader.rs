use crate::error::Result;
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, PgConnection};

/// Advisory-lock key electing the one replica that seals, verifies, reconciles
/// and prunes ("PAYMWRK1"). The outbox relay is safe on every replica.
pub const LEADER_LOCK_KEY: i64 = 0x5041_594D_5752_4B31;

/// Leadership = holding a session-level advisory lock on a dedicated
/// connection. The lock lives exactly as long as that session: if the process
/// dies or its connection drops, Postgres releases it and a standby takes over.
/// Server-side TCP keepalives bound how long a vanished leader's session (and
/// lock) can linger. Correctness never depends on it — the checkpoint seq and
/// sealed_seq unique indexes, the seal-once trigger and the reconcile watermark
/// row lock make a brief overlap of two leaders fail safely — it only stops
/// replicas from fighting over the same work.
pub struct LeaderLock {
    conn: PgConnection,
    key: i64,
}

impl LeaderLock {
    pub async fn try_acquire(options: &PgConnectOptions, key: i64) -> Result<Option<Self>> {
        let options = options.clone().options([
            ("tcp_keepalives_idle", "10"),
            ("tcp_keepalives_interval", "5"),
            ("tcp_keepalives_count", "3"),
        ]);
        let mut conn = PgConnection::connect_with(&options).await?;
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(key)
            .fetch_one(&mut conn)
            .await?;
        if acquired {
            Ok(Some(Self { conn, key }))
        } else {
            conn.close().await?;
            Ok(None)
        }
    }

    /// A live session still holds its session-level lock.
    pub async fn check(&mut self) -> Result<()> {
        self.conn.ping().await?;
        Ok(())
    }

    pub async fn release(mut self) -> Result<()> {
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(self.key)
            .execute(&mut self.conn)
            .await?;
        self.conn.close().await?;
        Ok(())
    }
}
