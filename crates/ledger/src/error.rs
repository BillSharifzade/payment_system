use crate::ids::{AccountId, TransactionId};
use money::{Currency, MoneyError};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LedgerError {
    #[error("unbalanced transaction: {currency} nets to {net_minor} minor units (must be 0)")]
    Unbalanced { currency: Currency, net_minor: i128 },

    #[error("transaction must have at least two entries, had {count}")]
    TooFewEntries { count: usize },

    #[error("entry amount must be strictly positive")]
    NonPositiveAmount,

    #[error("insufficient funds in account {account}: balance {balance_minor}, attempted change {delta_minor}")]
    InsufficientFunds {
        account: AccountId,
        balance_minor: i128,
        delta_minor: i128,
    },

    #[error("unknown account: {0}")]
    UnknownAccount(AccountId),

    #[error("currency mismatch for account {account}: account holds {account_currency}, entry was {entry_currency}")]
    AccountCurrencyMismatch {
        account: AccountId,
        account_currency: Currency,
        entry_currency: Currency,
    },

    #[error("transaction {0} has already been posted")]
    DuplicateTransaction(TransactionId),

    #[error(transparent)]
    Money(#[from] MoneyError),
}

pub type Result<T> = core::result::Result<T, LedgerError>;
