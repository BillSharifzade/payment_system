//! The slice of the TigerBeetle client API this backend uses. It is a trait so the protocol runs
//! unchanged against a real cluster (the `live` crate wraps the native client) and against
//! [`SimTb`](crate::SimTb), the in-process model the workspace tests use. Field layout, flag
//! bits and result codes are TigerBeetle's own wire values (`tb_client.h`, release 0.16/0.17).

use std::future::Future;

pub mod account_flags {
    pub const LINKED: u16 = 1 << 0;
    pub const DEBITS_MUST_NOT_EXCEED_CREDITS: u16 = 1 << 1;
    pub const CREDITS_MUST_NOT_EXCEED_DEBITS: u16 = 1 << 2;
    pub const HISTORY: u16 = 1 << 3;
    pub const IMPORTED: u16 = 1 << 4;
    pub const CLOSED: u16 = 1 << 5;
}

pub mod transfer_flags {
    pub const LINKED: u16 = 1 << 0;
    pub const PENDING: u16 = 1 << 1;
    pub const POST_PENDING_TRANSFER: u16 = 1 << 2;
    pub const VOID_PENDING_TRANSFER: u16 = 1 << 3;
    pub const BALANCING_DEBIT: u16 = 1 << 4;
    pub const BALANCING_CREDIT: u16 = 1 << 5;
    pub const CLOSING_DEBIT: u16 = 1 << 6;
    pub const CLOSING_CREDIT: u16 = 1 << 7;
    pub const IMPORTED: u16 = 1 << 8;
}

/// `amount` of a post that posts the whole pending amount.
pub const AMOUNT_MAX: u128 = u128::MAX;

/// Events per request. The cluster sets the request size limit when a client registers, and it
/// can be far below the 1 MiB message maximum (a 0.17.9 replica gave the 0.16.78 client 32 KiB:
/// 253 creates or 2031 lookups), so batches stay well under that.
pub const CREATE_BATCH: usize = 200;
pub const LOOKUP_BATCH: usize = 1_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Account {
    pub id: u128,
    pub debits_pending: u128,
    pub debits_posted: u128,
    pub credits_pending: u128,
    pub credits_posted: u128,
    pub user_data_128: u128,
    pub user_data_64: u64,
    pub user_data_32: u32,
    pub ledger: u32,
    pub code: u16,
    pub flags: u16,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transfer {
    pub id: u128,
    pub debit_account_id: u128,
    pub credit_account_id: u128,
    pub amount: u128,
    pub pending_id: u128,
    pub user_data_128: u128,
    pub user_data_64: u64,
    pub user_data_32: u32,
    pub timeout: u32,
    pub ledger: u32,
    pub code: u16,
    pub flags: u16,
    pub timestamp: u64,
}

impl Transfer {
    pub fn has(&self, flag: u16) -> bool {
        self.flags & flag != 0
    }
}

/// `query_transfers`: zero fields do not filter; timestamps are inclusive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryFilter {
    pub user_data_128: u128,
    pub user_data_64: u64,
    pub user_data_32: u32,
    pub ledger: u32,
    pub code: u16,
    pub timestamp_min: u64,
    pub timestamp_max: u64,
    pub limit: u32,
    pub reversed: bool,
}

macro_rules! result_codes {
    ($ty:ident { $($name:ident = $value:literal,)* }) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $ty(pub u32);

        #[allow(dead_code)]
        impl $ty {
            $(pub const $name: $ty = $ty($value);)*

            pub fn name(self) -> &'static str {
                match self.0 {
                    $($value => stringify!($name),)*
                    _ => "UNKNOWN",
                }
            }
        }

        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({})", self.name(), self.0)
            }
        }

        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.name().to_ascii_lowercase())
            }
        }
    };
}

result_codes!(AccountResult {
    OK = 0,
    LINKED_EVENT_FAILED = 1,
    LINKED_EVENT_CHAIN_OPEN = 2,
    TIMESTAMP_MUST_BE_ZERO = 3,
    RESERVED_FIELD = 4,
    RESERVED_FLAG = 5,
    ID_MUST_NOT_BE_ZERO = 6,
    ID_MUST_NOT_BE_INT_MAX = 7,
    FLAGS_ARE_MUTUALLY_EXCLUSIVE = 8,
    DEBITS_PENDING_MUST_BE_ZERO = 9,
    DEBITS_POSTED_MUST_BE_ZERO = 10,
    CREDITS_PENDING_MUST_BE_ZERO = 11,
    CREDITS_POSTED_MUST_BE_ZERO = 12,
    LEDGER_MUST_NOT_BE_ZERO = 13,
    CODE_MUST_NOT_BE_ZERO = 14,
    EXISTS_WITH_DIFFERENT_FLAGS = 15,
    EXISTS_WITH_DIFFERENT_USER_DATA_128 = 16,
    EXISTS_WITH_DIFFERENT_USER_DATA_64 = 17,
    EXISTS_WITH_DIFFERENT_USER_DATA_32 = 18,
    EXISTS_WITH_DIFFERENT_LEDGER = 19,
    EXISTS_WITH_DIFFERENT_CODE = 20,
    EXISTS = 21,
    IMPORTED_EVENT_EXPECTED = 22,
    IMPORTED_EVENT_NOT_EXPECTED = 23,
    IMPORTED_EVENT_TIMESTAMP_OUT_OF_RANGE = 24,
    IMPORTED_EVENT_TIMESTAMP_MUST_NOT_ADVANCE = 25,
    IMPORTED_EVENT_TIMESTAMP_MUST_NOT_REGRESS = 26,
});

result_codes!(TransferResult {
    OK = 0,
    LINKED_EVENT_FAILED = 1,
    LINKED_EVENT_CHAIN_OPEN = 2,
    TIMESTAMP_MUST_BE_ZERO = 3,
    RESERVED_FLAG = 4,
    ID_MUST_NOT_BE_ZERO = 5,
    ID_MUST_NOT_BE_INT_MAX = 6,
    FLAGS_ARE_MUTUALLY_EXCLUSIVE = 7,
    DEBIT_ACCOUNT_ID_MUST_NOT_BE_ZERO = 8,
    DEBIT_ACCOUNT_ID_MUST_NOT_BE_INT_MAX = 9,
    CREDIT_ACCOUNT_ID_MUST_NOT_BE_ZERO = 10,
    CREDIT_ACCOUNT_ID_MUST_NOT_BE_INT_MAX = 11,
    ACCOUNTS_MUST_BE_DIFFERENT = 12,
    PENDING_ID_MUST_BE_ZERO = 13,
    PENDING_ID_MUST_NOT_BE_ZERO = 14,
    PENDING_ID_MUST_NOT_BE_INT_MAX = 15,
    PENDING_ID_MUST_BE_DIFFERENT = 16,
    TIMEOUT_RESERVED_FOR_PENDING_TRANSFER = 17,
    LEDGER_MUST_NOT_BE_ZERO = 19,
    CODE_MUST_NOT_BE_ZERO = 20,
    DEBIT_ACCOUNT_NOT_FOUND = 21,
    CREDIT_ACCOUNT_NOT_FOUND = 22,
    ACCOUNTS_MUST_HAVE_THE_SAME_LEDGER = 23,
    TRANSFER_MUST_HAVE_THE_SAME_LEDGER_AS_ACCOUNTS = 24,
    PENDING_TRANSFER_NOT_FOUND = 25,
    PENDING_TRANSFER_NOT_PENDING = 26,
    PENDING_TRANSFER_HAS_DIFFERENT_DEBIT_ACCOUNT_ID = 27,
    PENDING_TRANSFER_HAS_DIFFERENT_CREDIT_ACCOUNT_ID = 28,
    PENDING_TRANSFER_HAS_DIFFERENT_LEDGER = 29,
    PENDING_TRANSFER_HAS_DIFFERENT_CODE = 30,
    EXCEEDS_PENDING_TRANSFER_AMOUNT = 31,
    PENDING_TRANSFER_HAS_DIFFERENT_AMOUNT = 32,
    PENDING_TRANSFER_ALREADY_POSTED = 33,
    PENDING_TRANSFER_ALREADY_VOIDED = 34,
    PENDING_TRANSFER_EXPIRED = 35,
    EXISTS_WITH_DIFFERENT_FLAGS = 36,
    EXISTS_WITH_DIFFERENT_DEBIT_ACCOUNT_ID = 37,
    EXISTS_WITH_DIFFERENT_CREDIT_ACCOUNT_ID = 38,
    EXISTS_WITH_DIFFERENT_AMOUNT = 39,
    EXISTS_WITH_DIFFERENT_PENDING_ID = 40,
    EXISTS_WITH_DIFFERENT_USER_DATA_128 = 41,
    EXISTS_WITH_DIFFERENT_USER_DATA_64 = 42,
    EXISTS_WITH_DIFFERENT_USER_DATA_32 = 43,
    EXISTS_WITH_DIFFERENT_TIMEOUT = 44,
    EXISTS_WITH_DIFFERENT_CODE = 45,
    EXISTS = 46,
    OVERFLOWS_DEBITS_PENDING = 47,
    OVERFLOWS_CREDITS_PENDING = 48,
    OVERFLOWS_DEBITS_POSTED = 49,
    OVERFLOWS_CREDITS_POSTED = 50,
    OVERFLOWS_DEBITS = 51,
    OVERFLOWS_CREDITS = 52,
    OVERFLOWS_TIMEOUT = 53,
    EXCEEDS_CREDITS = 54,
    EXCEEDS_DEBITS = 55,
    IMPORTED_EVENT_EXPECTED = 56,
    IMPORTED_EVENT_NOT_EXPECTED = 57,
    IMPORTED_EVENT_TIMESTAMP_OUT_OF_RANGE = 58,
    IMPORTED_EVENT_TIMESTAMP_MUST_NOT_ADVANCE = 59,
    IMPORTED_EVENT_TIMESTAMP_MUST_NOT_REGRESS = 60,
    IMPORTED_EVENT_TIMESTAMP_MUST_POSTDATE_DEBIT_ACCOUNT = 61,
    IMPORTED_EVENT_TIMESTAMP_MUST_POSTDATE_CREDIT_ACCOUNT = 62,
    IMPORTED_EVENT_TIMEOUT_MUST_BE_ZERO = 63,
    CLOSING_TRANSFER_MUST_BE_PENDING = 64,
    DEBIT_ACCOUNT_ALREADY_CLOSED = 65,
    CREDIT_ACCOUNT_ALREADY_CLOSED = 66,
    EXISTS_WITH_DIFFERENT_LEDGER = 67,
    ID_ALREADY_FAILED = 68,
});

impl TransferResult {
    /// TigerBeetle remembers ids that failed with these codes: a retry with the same id fails
    /// with `ID_ALREADY_FAILED` even after the cause is gone (`CreateTransferStatus.transient`).
    pub fn transient(self) -> bool {
        matches!(
            self,
            Self::DEBIT_ACCOUNT_NOT_FOUND
                | Self::CREDIT_ACCOUNT_NOT_FOUND
                | Self::PENDING_TRANSFER_NOT_FOUND
                | Self::EXCEEDS_CREDITS
                | Self::EXCEEDS_DEBITS
                | Self::DEBIT_ACCOUNT_ALREADY_CLOSED
                | Self::CREDIT_ACCOUNT_ALREADY_CLOSED
        )
    }
}

/// The request did not complete: the cluster is unreachable, the client was evicted, or the
/// call timed out. Whether a create took effect is unknown.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct ClientError(pub String);

/// One result per submitted event, in order (`OK` for the created ones).
pub trait TbClient: Send + Sync + 'static {
    fn create_accounts(
        &self,
        accounts: Vec<Account>,
    ) -> impl Future<Output = Result<Vec<AccountResult>, ClientError>> + Send;

    fn create_transfers(
        &self,
        transfers: Vec<Transfer>,
    ) -> impl Future<Output = Result<Vec<TransferResult>, ClientError>> + Send;

    /// Ids that do not exist are left out.
    fn lookup_accounts(
        &self,
        ids: Vec<u128>,
    ) -> impl Future<Output = Result<Vec<Account>, ClientError>> + Send;

    /// Ids that do not exist are left out.
    fn lookup_transfers(
        &self,
        ids: Vec<u128>,
    ) -> impl Future<Output = Result<Vec<Transfer>, ClientError>> + Send;

    fn query_transfers(
        &self,
        filter: QueryFilter,
    ) -> impl Future<Output = Result<Vec<Transfer>, ClientError>> + Send;

    /// The cluster id, which scopes recovery state kept in Postgres.
    fn cluster_id(&self) -> u128;
}
