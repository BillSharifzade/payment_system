use crate::ids::AccountId;
use money::Currency;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NormalSide {
    Debit,
    Credit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountType {
    UserWallet,
    SystemSettlement,
    SystemFeeRevenue,
    SystemFxGainLoss,
    SystemSuspense,
}

impl AccountType {
    pub fn normal_side(&self) -> NormalSide {
        match self {
            AccountType::UserWallet | AccountType::SystemFeeRevenue => NormalSide::Credit,
            AccountType::SystemSettlement
            | AccountType::SystemFxGainLoss
            | AccountType::SystemSuspense => NormalSide::Debit,
        }
    }

    pub fn allows_negative_balance(&self) -> bool {
        !matches!(self, AccountType::UserWallet)
    }

    pub fn as_db_str(&self) -> &'static str {
        match self {
            AccountType::UserWallet => "user_wallet",
            AccountType::SystemSettlement => "system_settlement",
            AccountType::SystemFeeRevenue => "system_fee_revenue",
            AccountType::SystemFxGainLoss => "system_fx_gain_loss",
            AccountType::SystemSuspense => "system_suspense",
        }
    }

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
