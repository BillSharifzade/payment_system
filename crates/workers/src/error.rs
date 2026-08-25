/// Errors from background workers.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("signing error: {0}")]
    Signing(#[from] crypto::SigningError),

    /// The checkpoint chain failed verification — this is a tamper alarm.
    #[error("checkpoint chain broken at seq {seq}: {reason}")]
    ChainBroken { seq: i64, reason: String },

    #[error("inconsistent stored data: {0}")]
    DataIntegrity(String),
}

pub type Result<T> = core::result::Result<T, WorkerError>;
