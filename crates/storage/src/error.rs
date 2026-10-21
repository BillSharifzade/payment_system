use ledger::LedgerError;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("amount {0} exceeds the storable range")]
    AmountTooLarge(i128),

    #[error("inconsistent stored data: {0}")]
    DataIntegrity(String),

    #[error("unknown currency: {0}")]
    UnknownCurrency(String),

    #[error("{message}")]
    Rejected { rule: String, message: String },

    /// The ledger backend (TigerBeetle) did not answer in time, or the attempt lost its
    /// reservation before it committed. Nothing was posted that recovery will not settle;
    /// retrying with the same transaction id is safe.
    #[error("ledger backend unavailable: {0}")]
    Unavailable(String),
}

pub type Result<T> = core::result::Result<T, StorageError>;
