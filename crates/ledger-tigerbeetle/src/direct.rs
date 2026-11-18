//! Direct posting (`TIGERBEETLE_POSTING=direct`): the cluster decides.
//!
//! A transfer or a conversion is one linked chain, applied by the cluster atomically:
//!
//! 1. the **key**: a zero-amount transfer between the control accounts whose id derives from
//!    the transaction id (the request's Idempotency-Key) and whose `user_data_64` is the
//!    request's fingerprint. At most one transfer per id ever exists, so a transaction posts
//!    at most once; a retry of the same request finds `exists`, any other request with that
//!    key a difference;
//! 2. the payer's **AML holds** (`limits.rs`), when the posting is screened;
//! 3. the **legs** (the money), under the balance limits of their accounts.
//!
//! The reply is the outcome: there is no reservation, no second step and nothing to recover.
//! The key cannot fail transiently, and holds and legs carry fresh ids, so a posting refused
//! for funds or a limit leaves nothing behind and its key can be used again. Postgres learns
//! of the posting from the cluster afterwards (`mirror.rs`).
//!
//! The same id space binds the keys of the other paths, so a key belongs to one request
//! whatever endpoint it is sent to: the guarded protocol (`hybrid.rs`: checks, deposits)
//! creates its key right before its COMMIT, and a void is a key of its own kind.

use ledger::Transaction;
use uuid::Uuid;

use crate::client::{TbClient, Transfer, TransferResult};
use crate::error::{first_failure, Result, TbError};
use crate::ids::{self, CONTROL_LEDGER};
use crate::limits::{hold_transfers, HoldIds, HoldSlot, Screening};
use crate::tb::{link, TbLedger};

/// What a direct posting came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyed {
    /// Applied now.
    Posted,
    /// This very request (same key, same fingerprint) had already posted.
    Replayed,
    /// The key belongs to another request.
    Conflict,
    /// The key was voided: it can never post.
    Voided,
}

/// Whom a key belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOwner {
    /// A direct posting, applied at `timestamp` (the cluster's).
    Direct {
        fingerprint: u64,
        timestamp: u64,
    },
    /// A request of the guarded protocol; whether it committed is Postgres's to say.
    Hybrid {
        fingerprint: u64,
    },
    Void {
        by: Uuid,
        timestamp: u64,
    },
}

fn is_exists_with_difference(r: TransferResult) -> bool {
    r.name().starts_with("EXISTS_WITH_DIFFERENT")
}

fn key_transfer(transaction: u128, code: u16, fingerprint: u64, tag: u32) -> Transfer {
    Transfer {
        id: ids::key(transaction),
        debit_account_id: ids::control_account(0),
        credit_account_id: ids::control_account(1),
        ledger: CONTROL_LEDGER,
        code,
        user_data_128: transaction,
        user_data_64: fingerprint,
        user_data_32: tag,
        ..Transfer::default()
    }
}

/// How a keyed chain ended, before the caller's own failure cases.
pub(crate) enum KeyResult {
    Created,
    /// The same key already exists (same kind, same fingerprint).
    Exists,
    /// Another request's, or a void.
    Taken,
    /// Event `i` of the chain (never the key) refused it.
    Failed(usize, TransferResult),
}

impl<C: TbClient> TbLedger<C> {
    /// Submits a chain that starts with a key.
    pub(crate) async fn create_keyed(&self, chain: Vec<Transfer>) -> Result<KeyResult> {
        Ok(match first_failure(&self.create(link(chain)).await?) {
            None => KeyResult::Created,
            Some((0, TransferResult::EXISTS)) => KeyResult::Exists,
            Some((0, r)) if is_exists_with_difference(r) => KeyResult::Taken,
            Some((0, r)) => return Err(TbError::Protocol(format!("key: {r}"))),
            Some((i, r)) => KeyResult::Failed(i, r),
        })
    }

    /// Whom the key of `transaction` belongs to, if anyone.
    pub async fn key_owner(&self, transaction: Uuid) -> Result<Option<KeyOwner>> {
        let key = ids::key(transaction.as_u128());
        let found = self
            .call(self.client().lookup_transfers(vec![key, key | 1]))
            .await?;
        let Some(k) = found.iter().find(|t| t.id == key) else {
            return Ok(None);
        };
        Ok(Some(match k.code {
            ids::TRANSFER_KEY_DIRECT => KeyOwner::Direct {
                fingerprint: k.user_data_64,
                timestamp: k.timestamp,
            },
            ids::TRANSFER_KEY_HYBRID => KeyOwner::Hybrid {
                fingerprint: k.user_data_64,
            },
            ids::TRANSFER_KEY_VOID => {
                let witness = found.iter().find(|t| t.id == key | 1).ok_or_else(|| {
                    TbError::Protocol(format!("void of {transaction} has no witness"))
                })?;
                KeyOwner::Void {
                    by: Uuid::from_u128(witness.user_data_128),
                    timestamp: k.timestamp,
                }
            }
            code => {
                return Err(TbError::Protocol(format!(
                    "key of {transaction} has code {code}"
                )))
            }
        }))
    }

    async fn taken(&self, transaction: Uuid) -> Result<Keyed> {
        match self.key_owner(transaction).await? {
            Some(KeyOwner::Void { .. }) => Ok(Keyed::Voided),
            Some(_) => Ok(Keyed::Conflict),
            None => Err(TbError::Protocol(format!(
                "the key of {transaction} differs but does not exist"
            ))),
        }
    }

    /// Posts `txn` as one chain: key, AML holds (when `screening` is given), legs. Errors are
    /// the ledger's (`InsufficientFunds`, …), a limit (`Rejected`, with the Postgres guard's
    /// rule names) or the cluster not answering; everything about the key is in [`Keyed`].
    pub async fn post_keyed(
        &self,
        txn: &Transaction,
        fingerprint: &str,
        screening: Option<&Screening>,
    ) -> Result<Keyed> {
        txn.validate()?;
        let plan = self.plan(txn).await?;
        let holds = screening
            .map(|s| s.holds().map(|h| (s, h)))
            .transpose()
            .map_err(|rule| rule.rejected())?;
        let t = txn.id.as_uuid().as_u128();
        let cfg = self.config();
        // Twice at most: a payer without limit accounts gets them, then the chain is retried.
        for first_try in [true, false] {
            // Fresh ids every try: the cluster remembers ids that failed for funds or a limit.
            let attempt = ids::new_attempt();
            let mut chain = vec![key_transfer(
                t,
                ids::TRANSFER_KEY_DIRECT,
                ids::fingerprint64(fingerprint),
                ids::TAG_JOURNALED,
            )];
            let slots: Vec<HoldSlot> = match holds {
                None => Vec::new(),
                Some((s, h)) => {
                    let hold_ids = HoldIds {
                        day: ids::leg(attempt, 0),
                        hour: ids::leg(attempt, 1),
                    };
                    let windows = (cfg.aml_day_secs, cfg.aml_hour_secs);
                    let (slots, transfers): (Vec<_>, Vec<_>) =
                        hold_transfers(s, h, t, hold_ids, ids::TAG_JOURNALED, windows)
                            .into_iter()
                            .unzip();
                    chain.extend(transfers);
                    slots
                }
            };
            let first_leg = chain.len();
            chain.extend(plan.legs.iter().enumerate().map(|(i, l)| Transfer {
                id: ids::leg(attempt, 2 + i),
                debit_account_id: l.debit,
                credit_account_id: l.credit,
                amount: l.amount,
                ledger: l.ledger,
                code: ids::TRANSFER_LEG,
                user_data_128: t,
                user_data_64: plan.legs.len() as u64,
                user_data_32: ids::TAG_JOURNALED,
                ..Transfer::default()
            }));
            return match self.create_keyed(chain).await? {
                KeyResult::Created => Ok(Keyed::Posted),
                KeyResult::Exists => Ok(Keyed::Replayed),
                KeyResult::Taken => self.taken(txn.id.as_uuid()).await,
                KeyResult::Failed(i, r) if i >= first_leg => {
                    Err(self.leg_error(&plan, i - first_leg, r).await)
                }
                KeyResult::Failed(i, r) => match r {
                    TransferResult::EXCEEDS_CREDITS => Err(slots[i - 1].rule().rejected()),
                    TransferResult::DEBIT_ACCOUNT_NOT_FOUND
                    | TransferResult::CREDIT_ACCOUNT_NOT_FOUND
                        if first_try =>
                    {
                        let (s, _) = holds.expect("a hold failed, so the posting is screened");
                        self.forget_limit_accounts(s.user);
                        self.ensure_limit_accounts(&[s.user]).await?;
                        continue;
                    }
                    r => Err(TbError::Protocol(format!("AML hold {i}: {r}"))),
                },
            };
        }
        unreachable!("the second try returns")
    }

    /// The key of a guarded posting under direct mode, with its AML holds: created by
    /// `HybridLedger` inside its Postgres transaction, right before the COMMIT. `Ok(true)`:
    /// the key is this request's (now, or from an earlier try whose COMMIT did not happen —
    /// its holds were placed with it). `Ok(false)`: it belongs to another request or a void.
    pub(crate) async fn claim_key(
        &self,
        transaction: u128,
        fingerprint: u64,
        screening: Option<&Screening>,
    ) -> Result<bool> {
        let holds = screening
            .map(|s| s.holds().map(|h| (s, h)))
            .transpose()
            .map_err(|rule| rule.rejected())?;
        let cfg = self.config();
        for first_try in [true, false] {
            let attempt = ids::new_attempt();
            let mut chain = vec![key_transfer(
                transaction,
                ids::TRANSFER_KEY_HYBRID,
                fingerprint,
                ids::TAG_KEY_HYBRID,
            )];
            let slots: Vec<HoldSlot> = match holds {
                None => Vec::new(),
                Some((s, h)) => {
                    let hold_ids = HoldIds {
                        day: ids::leg(attempt, 0),
                        hour: ids::leg(attempt, 1),
                    };
                    let windows = (cfg.aml_day_secs, cfg.aml_hour_secs);
                    let (slots, transfers): (Vec<_>, Vec<_>) =
                        hold_transfers(s, h, transaction, hold_ids, ids::TAG_KEY_HYBRID, windows)
                            .into_iter()
                            .unzip();
                    chain.extend(transfers);
                    slots
                }
            };
            return match self.create_keyed(chain).await? {
                KeyResult::Created | KeyResult::Exists => Ok(true),
                KeyResult::Taken => Ok(false),
                KeyResult::Failed(i, TransferResult::EXCEEDS_CREDITS) => {
                    Err(slots[i - 1].rule().rejected())
                }
                KeyResult::Failed(
                    _,
                    TransferResult::DEBIT_ACCOUNT_NOT_FOUND
                    | TransferResult::CREDIT_ACCOUNT_NOT_FOUND,
                ) if first_try => {
                    let (s, _) = holds.expect("a hold failed, so the posting is screened");
                    self.forget_limit_accounts(s.user);
                    self.ensure_limit_accounts(&[s.user]).await?;
                    continue;
                }
                KeyResult::Failed(i, r) => Err(TbError::Protocol(format!("AML hold {i}: {r}"))),
            };
        }
        unreachable!("the second try returns")
    }

    /// Voids a key: it can never post. `Ok(true)` when `by` voided it (now or before);
    /// `Ok(false)` when the key belongs to a posting, to a guarded request, or to someone
    /// else's void ([`key_owner`](Self::key_owner) says which).
    pub async fn void_key(&self, transaction: Uuid, by: Uuid) -> Result<bool> {
        let t = transaction.as_u128();
        let key = key_transfer(
            t,
            ids::TRANSFER_KEY_VOID,
            ids::fingerprint64(&by.to_string()),
            ids::TAG_JOURNALED,
        );
        let witness = Transfer {
            id: key.id | 1,
            code: ids::TRANSFER_VOID_WITNESS,
            user_data_128: by.as_u128(),
            user_data_64: 0,
            ..key
        };
        match self.create_keyed(vec![key, witness]).await? {
            KeyResult::Created | KeyResult::Exists => Ok(true),
            KeyResult::Taken => Ok(false),
            KeyResult::Failed(i, r) => Err(TbError::Protocol(format!("void witness {i}: {r}"))),
        }
    }

    /// The legs of a direct posting, in chain order (empty for a transaction that is not a
    /// direct posting, or moved nothing).
    pub async fn keyed_legs(&self, transaction: Uuid) -> Result<Vec<Transfer>> {
        let found = self
            .call(self.client().query_transfers(crate::client::QueryFilter {
                user_data_128: transaction.as_u128(),
                user_data_32: ids::TAG_JOURNALED,
                code: ids::TRANSFER_LEG,
                limit: ids::MAX_LEGS as u32,
                ..Default::default()
            }))
            .await?;
        Ok(found)
    }
}
