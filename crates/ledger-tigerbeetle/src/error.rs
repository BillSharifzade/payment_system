use ledger::LedgerError;
use storage::StorageError;

use crate::client::{ClientError, TransferResult};

#[derive(Debug, thiserror::Error)]
pub enum TbError {
    /// The ledger's own taxonomy (`StorageError::Ledger` for the rules a transaction broke,
    /// `Rejected` for a guard, `Database` for Postgres).
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// TigerBeetle did not answer in time. Nothing is lost or half-done: recovery voids or
    /// posts whatever reservation the call left behind. Retry with the same transaction id.
    #[error("tigerbeetle unavailable: {0}")]
    Unavailable(String),

    /// The attempt lost its reservation before it committed (recovery resolved it first, or
    /// it would have expired). Nothing was posted. Retry with the same transaction id.
    #[error("retry: {0}")]
    Retry(&'static str),

    /// TigerBeetle answered something the protocol rules out: an invariant is broken.
    #[error("tigerbeetle protocol violation: {0}")]
    Protocol(String),
}

pub type Result<T> = core::result::Result<T, TbError>;

impl From<LedgerError> for TbError {
    fn from(e: LedgerError) -> Self {
        Self::Storage(StorageError::Ledger(e))
    }
}

impl From<sqlx::Error> for TbError {
    fn from(e: sqlx::Error) -> Self {
        Self::Storage(StorageError::Database(e))
    }
}

impl From<ClientError> for TbError {
    fn from(e: ClientError) -> Self {
        Self::Unavailable(e.0)
    }
}

impl From<money::MoneyError> for TbError {
    fn from(e: money::MoneyError) -> Self {
        LedgerError::Money(e).into()
    }
}

/// For callers that speak `storage`'s taxonomy (the API): the two retryable cases become
/// `Unavailable`, a protocol violation is a data-integrity fault.
impl From<TbError> for StorageError {
    fn from(e: TbError) -> Self {
        match e {
            TbError::Storage(e) => e,
            TbError::Unavailable(m) => StorageError::Unavailable(m),
            TbError::Retry(m) => StorageError::Unavailable(m.to_string()),
            TbError::Protocol(m) => {
                StorageError::DataIntegrity(format!("tigerbeetle protocol violation: {m}"))
            }
        }
    }
}

impl TbError {
    pub fn as_ledger(&self) -> Option<&LedgerError> {
        match self {
            Self::Storage(StorageError::Ledger(e)) => Some(e),
            _ => None,
        }
    }
}

/// What a failed ledger leg (a reservation or a direct transfer) means for the caller. The
/// codes the protocol never provokes (bad flags, ids, timestamps, imports, closing) are
/// `Protocol`: the request was malformed, which is a bug here, not a user error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegFailure {
    /// A balance limit: the debited (or, for a debit-normal limited account, credited)
    /// account would go below zero.
    Insufficient {
        debit_side: bool,
    },
    UnknownAccount {
        debit_side: bool,
    },
    /// The account registry and the cluster disagree on an account's currency.
    LedgerMismatch,
    Overflow,
    Protocol,
}

pub fn leg_failure(r: TransferResult) -> LegFailure {
    type R = TransferResult;
    match r {
        R::EXCEEDS_CREDITS => LegFailure::Insufficient { debit_side: true },
        R::EXCEEDS_DEBITS => LegFailure::Insufficient { debit_side: false },
        R::DEBIT_ACCOUNT_NOT_FOUND => LegFailure::UnknownAccount { debit_side: true },
        R::CREDIT_ACCOUNT_NOT_FOUND => LegFailure::UnknownAccount { debit_side: false },
        R::ACCOUNTS_MUST_HAVE_THE_SAME_LEDGER
        | R::TRANSFER_MUST_HAVE_THE_SAME_LEDGER_AS_ACCOUNTS => LegFailure::LedgerMismatch,
        R::OVERFLOWS_DEBITS_PENDING
        | R::OVERFLOWS_CREDITS_PENDING
        | R::OVERFLOWS_DEBITS_POSTED
        | R::OVERFLOWS_CREDITS_POSTED
        | R::OVERFLOWS_DEBITS
        | R::OVERFLOWS_CREDITS
        | R::OVERFLOWS_TIMEOUT => LegFailure::Overflow,
        _ => LegFailure::Protocol,
    }
}

/// The event that broke a chain: TigerBeetle marks every other event `linked_event_failed`.
pub fn first_failure(results: &[TransferResult]) -> Option<(usize, TransferResult)> {
    results
        .iter()
        .position(|r| *r != TransferResult::OK && *r != TransferResult::LINKED_EVENT_FAILED)
        .or_else(|| results.iter().position(|r| *r != TransferResult::OK))
        .map(|i| (i, results[i]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_result_code_has_a_meaning() {
        type R = TransferResult;
        for code in 0..=80 {
            let r = TransferResult(code);
            let class = leg_failure(r);
            let expected = match r {
                R::EXCEEDS_CREDITS | R::EXCEEDS_DEBITS => "insufficient",
                R::DEBIT_ACCOUNT_NOT_FOUND | R::CREDIT_ACCOUNT_NOT_FOUND => "unknown",
                R::ACCOUNTS_MUST_HAVE_THE_SAME_LEDGER
                | R::TRANSFER_MUST_HAVE_THE_SAME_LEDGER_AS_ACCOUNTS => "mismatch",
                _ if r.name().starts_with("OVERFLOWS_") => "overflow",
                _ => "protocol",
            };
            let got = match class {
                LegFailure::Insufficient { .. } => "insufficient",
                LegFailure::UnknownAccount { .. } => "unknown",
                LegFailure::LedgerMismatch => "mismatch",
                LegFailure::Overflow => "overflow",
                LegFailure::Protocol => "protocol",
            };
            assert_eq!(got, expected, "{r:?}");
            assert_eq!(
                r.name() == "UNKNOWN",
                !(0..=68).contains(&code) || code == 18
            );
        }
    }

    #[test]
    fn the_breaking_event_is_found_whatever_its_position() {
        type R = TransferResult;
        let f = R::LINKED_EVENT_FAILED;
        assert_eq!(first_failure(&[R::OK, R::OK]), None);
        assert_eq!(
            first_failure(&[f, R::EXCEEDS_CREDITS, f]),
            Some((1, R::EXCEEDS_CREDITS))
        );
        assert_eq!(first_failure(&[R::EXISTS, f]), Some((0, R::EXISTS)));
        assert_eq!(first_failure(&[f, f]), Some((0, f)));
    }
}
