//! AML limits enforced by the cluster (`TIGERBEETLE_POSTING=direct`).
//!
//! Every user has two limit accounts on the AML ledger, each funded once with one
//! [`ALLOWANCE`] and unable to spend more than that (`debits_must_not_exceed_credits`):
//!
//! - the **day** account: a payment holds `amount / daily limit` of the allowance for 24 h;
//! - the **hour** account: a payment holds `1 / payments per hour` of it for 1 h.
//!
//! A hold is a pending transfer with a timeout, linked into the payment's chain: the cluster
//! refuses the whole chain when either allowance would be exceeded, and releases each hold by
//! itself when its window has passed. That is the rolling 24 h amount and the hourly velocity
//! of `api::payments::aml_check`, decided atomically with the money movement and with no lock
//! in Postgres.
//!
//! The unit is a fraction of the limit, not money, so a limit can change (another KYC tier,
//! new configuration) without touching any account: a hold weighs what its payment was of the
//! limit in force when it was made. For a constant limit `L` the decision is exact:
//! `sum(a) <= L  <=>  sum(floor(a * ALLOWANCE / L)) <= ALLOWANCE` as long as the holds in one
//! window number at most `ALLOWANCE / L` ([`Screening::exact`]; the velocity limit bounds
//! them).
//!
//! Differences from the Postgres guard, both in how a window is valued, never in which side
//! errs: a debit counts at the TJS rate of its own payment (Postgres re-values the window at
//! the current rate), and a hold ends when the cluster expires it (never early).

use std::collections::{HashMap, HashSet};

use storage::StorageError;
use uuid::Uuid;

use crate::client::{
    account_flags, transfer_flags as tf, Account, AccountResult, TbClient, Transfer,
    TransferResult, CREATE_BATCH,
};
use crate::error::{Result, TbError};
use crate::ids::{self, AML_LEDGER};
use crate::tb::TbLedger;

/// What every limit account is funded with, once.
pub const ALLOWANCE: u128 = 1 << 64;

/// A payment as the AML rules see it: who pays, what it is worth in TJS, and the payer's
/// limits (the API decides all three; `pre_screen` has applied the per-transaction limit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screening {
    pub user: Uuid,
    /// The payment in TJS minor units, floored (`api::payments::to_tjs`).
    pub tjs_minor: i64,
    /// The rolling 24 h limit, TJS minor units.
    pub daily_minor: i64,
    pub velocity_per_hour: i64,
}

/// The rule a payment broke, with the Postgres guard's names and messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitRule {
    Daily,
    Velocity,
}

impl LimitRule {
    pub fn rejected(self) -> TbError {
        let (rule, message) = match self {
            LimitRule::Daily => ("daily_limit", "amount exceeds the rolling 24h limit"),
            LimitRule::Velocity => ("velocity", "too many transfers in the last hour"),
        };
        TbError::Storage(StorageError::Rejected {
            rule: rule.to_string(),
            message: message.to_string(),
        })
    }
}

/// What a payment holds of the payer's two allowances.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holds {
    /// Zero for a payment worth less than one TJS minor unit: it places no day hold.
    pub day: u128,
    pub hour: u128,
}

impl Screening {
    /// What the payment weighs of each allowance; a limit it breaks by itself weighs all of it.
    fn weights(&self) -> Holds {
        let day =
            if self.tjs_minor < 0 || self.daily_minor <= 0 || self.tjs_minor > self.daily_minor {
                ALLOWANCE
            } else {
                // tjs_minor < 2^63, so the product is below 2^127.
                ((self.tjs_minor as u128) << 64) / self.daily_minor as u128
            };
        let hour = match self.velocity_per_hour {
            v if v <= 0 => ALLOWANCE,
            v => ALLOWANCE / v as u128,
        };
        Holds { day, hour }
    }

    /// The holds, or the rule the payment breaks whatever the window holds (the daily limit
    /// first, as the Postgres guard decides).
    pub fn holds(&self) -> core::result::Result<Holds, LimitRule> {
        if self.tjs_minor < 0 || self.daily_minor <= 0 || self.tjs_minor > self.daily_minor {
            return Err(LimitRule::Daily);
        }
        if self.velocity_per_hour <= 0 {
            return Err(LimitRule::Velocity);
        }
        Ok(self.weights())
    }

    /// Whether the cluster decides these limits exactly as the sums would: the holds of one
    /// 24 h window (at most 25 hours' worth of the velocity limit) lose less than one minor
    /// unit to flooring in total, and one payment more than the velocity limit never fits.
    pub fn exact(daily_minor: i64, velocity_per_hour: i64) -> bool {
        if daily_minor <= 0 || velocity_per_hour <= 0 {
            return true; // every payment is refused before the cluster is asked
        }
        let (daily, velocity) = (daily_minor as u128, velocity_per_hour as u128);
        daily
            .checked_mul(25 * velocity)
            .is_some_and(|n| n <= ALLOWANCE)
            && velocity * (velocity + 1) < ALLOWANCE
    }
}

/// Ids for the two holds of one payment.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HoldIds {
    pub day: u128,
    pub hour: u128,
}

/// Which hold a transfer of a chain is, by its position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HoldSlot {
    Day,
    Hour,
}

impl HoldSlot {
    pub fn rule(self) -> LimitRule {
        match self {
            HoldSlot::Day => LimitRule::Daily,
            HoldSlot::Hour => LimitRule::Velocity,
        }
    }
}

impl HoldSlot {
    fn account(self, user: u128) -> u128 {
        match self {
            HoldSlot::Day => ids::aml_day_account(user),
            HoldSlot::Hour => ids::aml_hour_account(user),
        }
    }

    fn code(self) -> u16 {
        match self {
            HoldSlot::Day => ids::TRANSFER_AML_DAY,
            HoldSlot::Hour => ids::TRANSFER_AML_HOUR,
        }
    }

    /// One hold: `amount` of the user's allowance, released after `timeout` seconds.
    fn hold(
        self,
        s: &Screening,
        id: u128,
        amount: u128,
        timeout: u32,
        txn: u128,
        tag: u32,
    ) -> Transfer {
        Transfer {
            id,
            debit_account_id: self.account(s.user.as_u128()),
            credit_account_id: ids::aml_account(1),
            amount,
            user_data_128: txn,
            user_data_64: s.tjs_minor.max(0) as u64,
            user_data_32: tag,
            timeout,
            ledger: AML_LEDGER,
            code: self.code(),
            flags: tf::PENDING,
            ..Transfer::default()
        }
    }
}

/// The hold transfers of one payment, in the order the rules are decided (day, then hour),
/// with which slot each is. Not linked: the caller chains them with what they guard.
pub(crate) fn hold_transfers(
    s: &Screening,
    holds: Holds,
    transaction: u128,
    hold_ids: HoldIds,
    tag: u32,
    (day_secs, hour_secs): (u32, u32),
) -> Vec<(HoldSlot, Transfer)> {
    let mut out = Vec::with_capacity(2);
    if holds.day > 0 {
        let t = HoldSlot::Day.hold(s, hold_ids.day, holds.day, day_secs, transaction, tag);
        out.push((HoldSlot::Day, t));
    }
    let t = HoldSlot::Hour.hold(s, hold_ids.hour, holds.hour, hour_secs, transaction, tag);
    out.push((HoldSlot::Hour, t));
    out
}

/// A hold to re-create at the switch to direct posting, for a debit the journal already
/// holds: what is left of its window, valued and weighed like a new payment's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldSeed {
    /// The journal entry (its id makes the seed idempotent).
    pub entry: Uuid,
    pub transaction: Uuid,
    pub screening: Screening,
    /// Seconds left of the debit's 24 h window (0: already out of it).
    pub day_left_secs: u32,
    /// Seconds left of its 1 h window (0: already out of it).
    pub hour_left_secs: u32,
}

impl<C: TbClient> TbLedger<C> {
    /// The AML ledger's source and sink. Idempotent; run at startup under direct posting.
    pub async fn ensure_aml_accounts(&self) -> Result<()> {
        let accounts = [(0, ids::CODE_AML_SOURCE), (1, ids::CODE_AML_SINK)]
            .map(|(side, code)| Account {
                id: ids::aml_account(side),
                ledger: AML_LEDGER,
                code,
                ..Account::default()
            })
            .to_vec();
        self.create_aml_accounts(accounts).await
    }

    async fn create_aml_accounts(&self, accounts: Vec<Account>) -> Result<()> {
        let results = self
            .call(self.client().create_accounts(accounts.clone()))
            .await?;
        for (a, r) in accounts.iter().zip(&results) {
            if *r != AccountResult::OK && *r != AccountResult::EXISTS {
                return Err(TbError::Protocol(format!("AML account {:032x}: {r}", a.id)));
            }
        }
        Ok(())
    }

    /// Each user's two limit accounts, created and funded. Idempotent (the funding transfer's
    /// id derives from the account), and remembered per process.
    pub async fn ensure_limit_accounts(&self, users: &[Uuid]) -> Result<()> {
        let missing: Vec<u128> = {
            let ready = self.limits_ready.read().expect("limits poisoned");
            let mut seen = HashSet::new();
            users
                .iter()
                .map(|u| u.as_u128())
                .filter(|u| !ready.contains(u) && seen.insert(*u))
                .collect()
        };
        if missing.is_empty() {
            return Ok(());
        }
        self.ensure_aml_accounts().await?;
        // Two accounts and two transfers per user.
        for chunk in missing.chunks(CREATE_BATCH / 2) {
            let accounts: Vec<Account> = chunk
                .iter()
                .flat_map(|u| {
                    [
                        (ids::aml_day_account(*u), ids::CODE_AML_DAY),
                        (ids::aml_hour_account(*u), ids::CODE_AML_HOUR),
                    ]
                    .map(|(id, code)| Account {
                        id,
                        user_data_128: *u,
                        ledger: AML_LEDGER,
                        code,
                        flags: account_flags::DEBITS_MUST_NOT_EXCEED_CREDITS,
                        ..Account::default()
                    })
                })
                .collect();
            let funding: Vec<Transfer> = accounts
                .iter()
                .map(|a| Transfer {
                    id: ids::aml_fund_transfer(a.id),
                    debit_account_id: ids::aml_account(0),
                    credit_account_id: a.id,
                    amount: ALLOWANCE,
                    user_data_128: a.user_data_128,
                    user_data_32: ids::TAG_AML,
                    ledger: AML_LEDGER,
                    code: ids::TRANSFER_AML_FUND,
                    ..Transfer::default()
                })
                .collect();
            self.create_aml_accounts(accounts).await?;
            for (t, r) in funding.iter().zip(self.create(funding.clone()).await?) {
                if r != TransferResult::OK && r != TransferResult::EXISTS {
                    return Err(TbError::Protocol(format!(
                        "funding limit account {:032x}: {r}",
                        t.credit_account_id
                    )));
                }
            }
            self.limits_ready
                .write()
                .expect("limits poisoned")
                .extend(chunk.iter().copied());
        }
        Ok(())
    }

    /// Drops the memory of a user's limit accounts (the cluster said one is missing).
    pub(crate) fn forget_limit_accounts(&self, user: Uuid) {
        self.limits_ready
            .write()
            .expect("limits poisoned")
            .remove(&user.as_u128());
    }

    /// Re-creates the holds of debits already in the journal (the switch to direct posting,
    /// with money movement stopped), oldest first per user. Idempotent per entry; returns how
    /// many holds the seeds amount to. A window the present limits cannot hold (the limits
    /// were lowered, or a rate moved) fills the allowance and no more: that user pays again
    /// once enough of it has aged out, as under the Postgres guard.
    pub async fn seed_holds(&self, seeds: &[HoldSeed]) -> Result<usize> {
        let users: Vec<Uuid> = seeds.iter().map(|s| s.screening.user).collect();
        self.ensure_limit_accounts(&users).await?;
        let mut used: HashMap<u128, [u128; 2]> = HashMap::new();
        let mut transfers = Vec::new();
        for seed in seeds {
            let weights = seed.screening.weights();
            let entry = seed.entry.as_u128();
            let used = used.entry(seed.screening.user.as_u128()).or_default();
            for (i, slot, weight, left) in [
                (0, HoldSlot::Day, weights.day, seed.day_left_secs),
                (1, HoldSlot::Hour, weights.hour, seed.hour_left_secs),
            ] {
                let amount = weight.min(ALLOWANCE - used[i]);
                if left == 0 || amount == 0 {
                    continue;
                }
                used[i] += amount;
                transfers.push((
                    seed.entry,
                    slot,
                    slot.hold(
                        &seed.screening,
                        ids::aml_seed_transfer(entry, slot == HoldSlot::Hour),
                        amount,
                        left,
                        seed.transaction.as_u128(),
                        ids::TAG_AML,
                    ),
                ));
            }
        }
        type R = TransferResult;
        for chunk in transfers.chunks(CREATE_BATCH) {
            let batch: Vec<Transfer> = chunk.iter().map(|(_, _, t)| *t).collect();
            for ((entry, slot, _), r) in chunk.iter().zip(self.create(batch).await?) {
                // A re-run after a rate or limit changed finds its earlier seed: that one stands.
                if !matches!(
                    r,
                    R::OK
                        | R::EXISTS
                        | R::EXISTS_WITH_DIFFERENT_AMOUNT
                        | R::EXISTS_WITH_DIFFERENT_TIMEOUT
                ) {
                    return Err(StorageError::DataIntegrity(format!(
                        "seeding the {slot:?} hold of journal entry {entry}: {r}"
                    ))
                    .into());
                }
            }
        }
        Ok(transfers.len())
    }

    /// What is left of a user's two allowances, as fractions of [`ALLOWANCE`] still free
    /// (`None` for a user without limit accounts yet).
    pub async fn allowances(&self, user: Uuid) -> Result<Option<(u128, u128)>> {
        let u = user.as_u128();
        let (day, hour) = (ids::aml_day_account(u), ids::aml_hour_account(u));
        let found = self
            .call(self.client().lookup_accounts(vec![day, hour]))
            .await?;
        let free = |id| {
            found.iter().find(|a| a.id == id).map(|a: &Account| {
                a.credits_posted
                    .saturating_sub(a.debits_posted)
                    .saturating_sub(a.debits_pending)
            })
        };
        Ok(free(day).zip(free(hour)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screening(tjs_minor: i64, daily_minor: i64, velocity_per_hour: i64) -> Screening {
        Screening {
            user: Uuid::now_v7(),
            tjs_minor,
            daily_minor,
            velocity_per_hour,
        }
    }

    /// Whether the cluster would accept these payments in one window.
    fn fits(amounts: &[i64], daily: i64) -> bool {
        amounts
            .iter()
            .map(|a| screening(*a, daily, 1_000).holds().unwrap().day)
            .sum::<u128>()
            <= ALLOWANCE
    }

    #[test]
    fn a_window_fits_exactly_when_its_sum_is_within_the_limit() {
        for daily in [1, 7, 999, 1_000_000, 123_456_789, 10_i64.pow(13)] {
            // Thirds of the limit never divide the allowance evenly.
            let third = daily / 3;
            if third > 0 {
                assert!(fits(&[third, third, daily - 2 * third], daily), "{daily}");
                if 3 * third < daily {
                    assert!(
                        !fits(&[third, third, daily - 2 * third, 1], daily),
                        "{daily}"
                    );
                }
            }
            assert!(fits(&[daily], daily));
            assert!(!fits(&[daily, 1], daily));
            // Many small payments: flooring must not let one minor unit more through.
            let n = 1_000.min(daily);
            let mut parts = vec![daily / n; n as usize];
            parts[0] += daily % n;
            assert!(fits(&parts, daily), "{daily} in {n} parts");
            parts.push(1);
            assert!(!fits(&parts, daily), "{daily} in {n} parts and one more");
        }
    }

    #[test]
    fn the_velocity_allowance_holds_exactly_the_limit() {
        for v in [1_i64, 2, 3, 7, 30, 1_000, 65_536] {
            let hour = screening(1, 10, v).holds().unwrap().hour;
            assert!(hour * v as u128 <= ALLOWANCE, "{v}");
            assert!(hour * (v as u128 + 1) > ALLOWANCE, "{v}");
        }
    }

    #[test]
    fn limits_refused_without_the_cluster() {
        assert_eq!(screening(11, 10, 5).holds(), Err(LimitRule::Daily));
        assert_eq!(screening(1, 0, 5).holds(), Err(LimitRule::Daily));
        assert_eq!(screening(1, 10, 0).holds(), Err(LimitRule::Velocity));
        // Both broken: the daily limit is reported, as the Postgres guard does.
        assert_eq!(screening(11, 10, 0).holds(), Err(LimitRule::Daily));
        assert_eq!(screening(0, 10, 5).holds().unwrap().day, 0);
    }

    #[test]
    fn exactness_bounds() {
        assert!(Screening::exact(1_000_000_000, 1_000));
        assert!(Screening::exact(i64::MAX, 0));
        assert!(!Screening::exact(i64::MAX, 1));
        assert!(!Screening::exact(1, i64::MAX));
        assert!(Screening::exact(0, 5));
    }
}
