use crate::ids::AccountId;
use money::Currency;
use serde::{Deserialize, Serialize};

/// Which side of the ledger an account is "normal" on — i.e. which direction
/// increases its balance.
///
/// This is standard double-entry accounting:
/// - **Credit-normal** accounts (liabilities, revenue): a *credit* increases the
///   balance. A user wallet is credit-normal — we *owe* the user their balance,
///   so crediting them (a deposit) increases what we owe.
/// - **Debit-normal** accounts (assets): a *debit* increases the balance. A bank
///   settlement/nostro account is debit-normal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NormalSide {
    Debit,
    Credit,
}

/// The kind of account. The type fixes the accounting semantics (normal side and
/// whether the balance may go negative); it is not free-form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountType {
    /// An end user's wallet. Liability (we owe the user). Credit-normal, and may
    /// **not** go negative — a user cannot spend money they do not have.
    UserWallet,
    /// Mirrors funds held at / owed to a partner bank (nostro). Asset.
    /// Debit-normal. May go negative.
    SystemSettlement,
    /// Accrues fees we charge. Revenue. Credit-normal. May go negative.
    SystemFeeRevenue,
    /// Absorbs rounding / gains / losses from FX. May go negative.
    SystemFxGainLoss,
    /// Temporary holding for in-flight or unclassified funds. May go negative.
    SystemSuspense,
}

impl AccountType {
    /// The side on which this account type's balance increases.
    pub fn normal_side(&self) -> NormalSide {
        match self {
            AccountType::UserWallet | AccountType::SystemFeeRevenue => NormalSide::Credit,
            AccountType::SystemSettlement
            | AccountType::SystemFxGainLoss
            | AccountType::SystemSuspense => NormalSide::Debit,
        }
    }

    /// Whether this account is permitted to hold a negative balance.
    ///
    /// Only user wallets are forbidden from going negative (no overdraft in v1).
    /// System accounts represent the platform's own position and routinely sit
    /// negative by design.
    pub fn allows_negative_balance(&self) -> bool {
        !matches!(self, AccountType::UserWallet)
    }

    /// The stable string used to persist this type (must match the DB CHECK
    /// constraint in the migrations).
    pub fn as_db_str(&self) -> &'static str {
        match self {
            AccountType::UserWallet => "user_wallet",
            AccountType::SystemSettlement => "system_settlement",
            AccountType::SystemFeeRevenue => "system_fee_revenue",
            AccountType::SystemFxGainLoss => "system_fx_gain_loss",
            AccountType::SystemSuspense => "system_suspense",
        }
    }

    /// Parse a persisted account type. Returns `None` for an unrecognised value.
    pub fn from_db_str(s: &str) -> Option<Self> {
        Some(match s {
            "user_wallet" => AccountType::UserWallet,
            "system_settlement" => AccountType::SystemSettlement,
            "system_fee_revenue" => AccountType::SystemFeeRevenue,
            "system_fx_gain_loss" => AccountType::SystemFxGainLoss,
            "system_suspense" => AccountType::SystemSuspense,
            _ => return None,
        })
    }
}

/// An account in the ledger. Each account holds exactly one currency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub account_type: AccountType,
    pub currency: Currency,
}

impl Account {
    pub fn new(id: AccountId, account_type: AccountType, currency: Currency) -> Self {
        Self {
            id,
            account_type,
            currency,
        }
    }

    pub fn normal_side(&self) -> NormalSide {
        self.account_type.normal_side()
    }

    pub fn allows_negative_balance(&self) -> bool {
        self.account_type.allows_negative_balance()
    }
}
