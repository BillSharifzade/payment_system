//! How ledger objects map onto TigerBeetle's: account ids, ledgers, codes and the transfer ids
//! of the posting protocol.
//!
//! - An account keeps its UUID as its TigerBeetle id (`as_u128`), so either side maps back.
//! - One ledger per currency, `[c0, c1, c2, exponent]` big-endian: self-describing, so the
//!   account registry can be rebuilt from TigerBeetle alone. Ledger 1 is the control ledger.
//! - Transfer ids put a 48-bit time prefix on top (TigerBeetle's LSM prefers ordered ids) and
//!   keep the low 8 bits for the leg index.

use ledger::AccountType;
use money::Currency;
use sha2::{Digest, Sha256};

use crate::client::account_flags;

pub const LEG_BITS: u32 = 8;
pub const LEG_MASK: u128 = (1 << LEG_BITS) - 1;
/// A transaction's chain must fit one request (with the direct path's marker), see
/// `CREATE_BATCH`; real transactions net to one to three legs.
pub const MAX_LEGS: usize = 64;

/// Holds the two control accounts commit markers move zero between.
pub const CONTROL_LEDGER: u32 = 1;
/// Holds the AML limit accounts (`limits.rs`): its unit is a fraction of a limit, not money.
pub const AML_LEDGER: u32 = 2;

/// Account codes. Every ledger account's `code` is its [`AccountType`].
pub const CODE_CONTROL: u16 = 100;
pub const CODE_OPENING: u16 = 101;
pub const CODE_AML_SOURCE: u16 = 110;
pub const CODE_AML_SINK: u16 = 111;
pub const CODE_AML_DAY: u16 = 112;
pub const CODE_AML_HOUR: u16 = 113;

/// Transfer codes.
pub const TRANSFER_LEG: u16 = 1;
pub const TRANSFER_MARKER: u16 = 2;
pub const TRANSFER_OPENING: u16 = 3;
/// Who an idempotency key belongs to under `TIGERBEETLE_POSTING=direct` (`direct.rs`): the
/// transaction a direct chain posted, a request of the guarded protocol, or nobody (voided).
pub const TRANSFER_KEY_DIRECT: u16 = 10;
pub const TRANSFER_KEY_HYBRID: u16 = 11;
pub const TRANSFER_KEY_VOID: u16 = 12;
/// The second transfer of a void: its `user_data_128` is who voided the key.
pub const TRANSFER_VOID_WITNESS: u16 = 13;
/// AML: a limit account's one-time allowance, and the expiring holds payments place on it.
pub const TRANSFER_AML_FUND: u16 = 20;
pub const TRANSFER_AML_DAY: u16 = 21;
pub const TRANSFER_AML_HOUR: u16 = 22;

/// `user_data_32` of every transfer the backend creates: the protocol step it belongs to.
/// Recovery scans for `TAG_RESERVE`; a post or void carries its own tag, not the pending's.
pub const TAG_RESERVE: u32 = 1;
pub const TAG_POST: u32 = 2;
pub const TAG_VOID: u32 = 3;
pub const TAG_FORCED: u32 = 4;
pub const TAG_DIRECT: u32 = 5;
pub const TAG_MARKER: u32 = 6;
pub const TAG_OPENING: u32 = 7;
/// Every transfer of a chain the journal mirror turns into Postgres rows (`mirror.rs`): a
/// direct posting (key, holds, legs) or a void (key, witness). The mirror scans this tag.
pub const TAG_JOURNALED: u32 = 8;
/// The key and holds of a guarded (hybrid) posting under direct mode: Postgres journals it.
pub const TAG_KEY_HYBRID: u32 = 9;
pub const TAG_AML: u32 = 10;

pub fn ledger_of(currency: Currency) -> u32 {
    let c = currency.code().as_bytes();
    u32::from_be_bytes([c[0], c[1], c[2], currency.exponent()])
}

pub fn currency_of(ledger: u32) -> Option<Currency> {
    let b = ledger.to_be_bytes();
    if !b[..3].iter().all(u8::is_ascii_uppercase) {
        return None;
    }
    Currency::new(std::str::from_utf8(&b[..3]).ok()?, b[3]).ok()
}

pub fn code_of(account_type: AccountType) -> u16 {
    match account_type {
        AccountType::UserWallet => 1,
        AccountType::SystemSettlement => 2,
        AccountType::SystemFeeRevenue => 3,
        AccountType::SystemFxGainLoss => 4,
        AccountType::SystemSuspense => 5,
    }
}

pub fn account_type_of(code: u16) -> Option<AccountType> {
    Some(match code {
        1 => AccountType::UserWallet,
        2 => AccountType::SystemSettlement,
        3 => AccountType::SystemFeeRevenue,
        4 => AccountType::SystemFxGainLoss,
        5 => AccountType::SystemSuspense,
        _ => return None,
    })
}

/// An account that may not go below zero on its normal side gets TigerBeetle's matching
/// balance limit, so the no-overdraft rule is enforced atomically by the cluster.
pub fn flags_of(account_type: AccountType) -> u16 {
    use ledger::NormalSide;
    match (
        account_type.allows_negative_balance(),
        account_type.normal_side(),
    ) {
        (true, _) => 0,
        (false, NormalSide::Credit) => account_flags::DEBITS_MUST_NOT_EXCEED_CREDITS,
        (false, NormalSide::Debit) => account_flags::CREDITS_MUST_NOT_EXCEED_DEBITS,
    }
}

/// A deterministic id with `x`'s time prefix. SHA-256, because transaction ids are client
/// chosen (Idempotency-Key): a collision would let one client's key block another's posting.
pub fn derive(domain: &str, x: u128) -> u128 {
    let digest = Sha256::new()
        .chain_update(domain.as_bytes())
        .chain_update(x.to_be_bytes())
        .finalize();
    let mut h = [0u8; 16];
    h.copy_from_slice(&digest[..16]);
    ((x & (u128::MAX << 80)) | (u128::from_be_bytes(h) >> 48)) & !LEG_MASK
}

/// A fresh reservation: TigerBeetle remembers ids that failed transiently (`exceeds_credits`
/// among them), so every attempt needs ids of its own for a later retry to be able to succeed.
pub fn new_attempt() -> u128 {
    uuid::Uuid::now_v7().as_u128() & !LEG_MASK
}

/// Ids of the commit record of a transaction: the posts of its legs (or the zero-amount marker
/// of a direct post). At most one transfer per id ever exists, so at most one attempt posts.
pub fn post_base(transaction: u128) -> u128 {
    derive("payment/tb/post", transaction)
}

pub fn void_base(attempt: u128) -> u128 {
    derive("payment/tb/void", attempt)
}

pub fn control_account(side: u8) -> u128 {
    derive("payment/tb/control", side as u128) | 1
}

pub fn opening_account(ledger: u32) -> u128 {
    derive("payment/tb/opening", ledger as u128) | 1
}

pub fn opening_transfer(account: u128) -> u128 {
    derive("payment/tb/opening-balance", account)
}

/// The transfer that binds an idempotency key (a transaction id) under direct posting: at
/// most one ever exists per key, whoever creates it. `key(k) | 1` and `| 2` are its fixed
/// companions (a void's witness; a guarded posting's holds).
pub fn key(transaction: u128) -> u128 {
    derive("payment/tb/key", transaction)
}

pub fn aml_account(side: u8) -> u128 {
    derive("payment/tb/aml", side as u128) | 1
}

/// A user's rolling 24 h allowance.
pub fn aml_day_account(user: u128) -> u128 {
    derive("payment/tb/aml-day", user) | 1
}

/// A user's rolling 1 h allowance of payments.
pub fn aml_hour_account(user: u128) -> u128 {
    derive("payment/tb/aml-hour", user) | 1
}

pub fn aml_fund_transfer(account: u128) -> u128 {
    derive("payment/tb/aml-fund", account)
}

/// A hold re-created at the switch to direct posting for a debit the journal already holds.
pub fn aml_seed_transfer(entry: u128, hour: bool) -> u128 {
    derive("payment/tb/aml-seed", entry) | hour as u128
}

/// The journal entry the mirror writes for one side of one account of a direct posting.
pub fn mirror_entry(transaction: u128, account: u128, credit: bool) -> u128 {
    let digest = Sha256::new()
        .chain_update(b"payment/tb/mirror-entry")
        .chain_update(transaction.to_be_bytes())
        .chain_update(account.to_be_bytes())
        .chain_update([credit as u8])
        .finalize();
    let mut h = [0u8; 16];
    h.copy_from_slice(&digest[..16]);
    (transaction & (u128::MAX << 80)) | (u128::from_be_bytes(h) >> 48)
}

/// The outbox event the mirror writes for a direct posting.
pub fn mirror_event(transaction: u128) -> u128 {
    derive("payment/tb/mirror-event", transaction)
}

/// The `allowed` screening event the mirror writes for a screened direct posting.
pub fn mirror_screening(transaction: u128) -> u128 {
    derive("payment/tb/mirror-screening", transaction)
}

/// 64 bits of a request fingerprint, for the key's `user_data_64`: a retry with the same
/// request finds the same key (`exists`), any other request a different one.
pub fn fingerprint64(fingerprint: &str) -> u64 {
    let digest = Sha256::digest(fingerprint.as_bytes());
    let mut h = [0u8; 8];
    h.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(h)
}

pub fn leg(base: u128, index: usize) -> u128 {
    debug_assert!(index < MAX_LEGS && base & LEG_MASK == 0);
    base | index as u128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledgers_round_trip_and_never_collide_with_the_control_ledger() {
        for (code, exp) in [("TJS", 2), ("USD", 2), ("JPY", 0), ("KWD", 3)] {
            let c = Currency::new(code, exp).unwrap();
            let l = ledger_of(c);
            assert_ne!(l, CONTROL_LEDGER);
            assert_eq!(currency_of(l), Some(c));
        }
        assert_ne!(
            ledger_of(Currency::new("TJS", 2).unwrap()),
            ledger_of(Currency::new("TJS", 3).unwrap())
        );
        assert_eq!(currency_of(CONTROL_LEDGER), None);
    }

    #[test]
    fn account_codes_round_trip() {
        for t in [
            AccountType::UserWallet,
            AccountType::SystemSettlement,
            AccountType::SystemFeeRevenue,
            AccountType::SystemFxGainLoss,
            AccountType::SystemSuspense,
        ] {
            assert_eq!(account_type_of(code_of(t)), Some(t));
        }
        assert_eq!(
            flags_of(AccountType::UserWallet),
            account_flags::DEBITS_MUST_NOT_EXCEED_CREDITS
        );
        assert_eq!(flags_of(AccountType::SystemSettlement), 0);
    }

    #[test]
    fn derived_ids_keep_the_time_prefix_and_leave_room_for_legs() {
        let t = uuid::Uuid::now_v7().as_u128();
        let p = post_base(t);
        assert_eq!(p >> 80, t >> 80);
        assert_eq!(p & LEG_MASK, 0);
        assert_ne!(p, void_base(t));
        assert_eq!(post_base(t), p);
        assert_ne!(post_base(t + 1), p);
        let a = new_attempt();
        assert_eq!(a & LEG_MASK, 0);
        assert_eq!(leg(a, 3), a + 3);
        assert_ne!(control_account(0), control_account(1));
        assert_ne!(control_account(0), 0);
    }
}
