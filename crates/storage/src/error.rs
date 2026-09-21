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
}

pub type Result<T> = core::result::Result<T, StorageError>;
