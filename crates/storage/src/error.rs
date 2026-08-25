use ledger::LedgerError;

/// Errors from the durable ledger store: either a violated accounting rule
/// (wrapping [`LedgerError`]) or an infrastructure failure (database, mapping).
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// An accounting rule was violated (unbalanced, insufficient funds, etc.).
    /// These are the same correctness errors the pure engine raises.
    #[error(transparent)]
    Ledger(#[from] LedgerError),

    /// A database-level failure.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// An amount exceeded what the persistence layer can store (i64 minor units).
    /// In practice unreachable for real balances; surfaced rather than truncated.
    #[error("amount {0} exceeds the storable range")]
    AmountTooLarge(i128),

    /// A row held a value the code does not recognise (e.g. an account_type
    /// string not in the enum) — indicates schema/code drift.
    #[error("inconsistent stored data: {0}")]
    DataIntegrity(String),

    /// The requested currency is not registered in the `currencies` table.
    #[error("unknown currency: {0}")]
    UnknownCurrency(String),
}

pub type Result<T> = core::result::Result<T, StorageError>;
