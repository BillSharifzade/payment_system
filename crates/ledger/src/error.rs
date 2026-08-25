use crate::ids::{AccountId, TransactionId};
use money::{Currency, MoneyError};

/// Everything that can go wrong when validating or posting a transaction.
///
/// These are *correctness* errors. In a payment system every one of them
/// represents either a client mistake or a bug we must never paper over.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LedgerError {
    /// The transaction's debits and credits do not net to zero for some
    /// currency. This is the cardinal sin of double-entry bookkeeping and is
    /// rejected unconditionally — accepting it would create or destroy money.
    #[error("unbalanced transaction: {currency} nets to {net_minor} minor units (must be 0)")]
    Unbalanced { currency: Currency, net_minor: i128 },

    /// A transaction had fewer than two entries. A real movement of money
    /// always touches at least two accounts.
    #[error("transaction must have at least two entries, had {count}")]
    TooFewEntries { count: usize },

    /// An entry's amount was zero or negative. Entry amounts are always a
    /// positive magnitude; the [`crate::Direction`] carries the sign.
    #[error("entry amount must be strictly positive")]
    NonPositiveAmount,

    /// Posting would drive an account that is not allowed to go negative below
    /// zero — e.g. a user trying to spend more than they hold.
    #[error("insufficient funds in account {account}: balance {balance_minor}, attempted change {delta_minor}")]
    InsufficientFunds {
        account: AccountId,
        balance_minor: i128,
        delta_minor: i128,
    },

    /// An entry referenced an account the ledger does not know about.
    #[error("unknown account: {0}")]
    UnknownAccount(AccountId),

    /// An entry's currency did not match the account's currency.
    #[error("currency mismatch for account {account}: account holds {account_currency}, entry was {entry_currency}")]
    AccountCurrencyMismatch {
        account: AccountId,
        account_currency: Currency,
        entry_currency: Currency,
    },

    /// A transaction with this id was already posted. Posting is idempotent on
    /// the transaction id: re-posting the same id is rejected rather than
    /// applied twice.
    #[error("transaction {0} has already been posted")]
    DuplicateTransaction(TransactionId),

    /// Underlying money arithmetic failed (overflow, etc.).
    #[error(transparent)]
    Money(#[from] MoneyError),
}

/// Convenience alias for fallible ledger operations.
pub type Result<T> = core::result::Result<T, LedgerError>;
