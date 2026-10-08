//! `SimTb`: an in-process model of one TigerBeetle cluster, for tests without a server.
//!
//! It follows `src/state_machine.zig` (release 0.17.9) for everything this backend touches:
//! the order of validation checks and so which result code wins, linked chains (all or
//! nothing, `linked_event_failed` for the rest), two-phase transfers and their timeouts,
//! balance limits, `exists*` idempotency and the ids TigerBeetle remembers after a transient
//! failure (`id_already_failed`). Imported events and account closing are not modelled.
//! `tests/live.rs` runs the same operation sequences against a real cluster and the model and
//! requires identical results, so a divergence fails CI instead of hiding behind the model.
//!
//! Faults: a request can be dropped before the cluster sees it, or applied with its reply lost.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::client::{
    account_flags as af, transfer_flags as tf, Account, AccountResult, ClientError, QueryFilter,
    TbClient, Transfer, TransferResult,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    CreateAccounts,
    CreateTransfers,
    LookupAccounts,
    LookupTransfers,
    QueryTransfers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The request never reaches the cluster.
    Drop(Op),
    /// The cluster applies the request; the reply is lost.
    LoseReply(Op),
    /// Lets one request through untouched, to aim the fault queued behind it.
    Pass(Op),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Pending,
    Posted,
    Voided,
    Expired,
}

enum Undo {
    Account(Account),
    NewAccount(u128),
    Transfer(u128, u64),
    Status(u128, Option<Status>),
    Expiry((u64, u128), bool),
}

#[derive(Default)]
struct State {
    accounts: HashMap<u128, Account>,
    transfers: HashMap<u128, Transfer>,
    by_time: BTreeMap<u64, u128>,
    failed: HashSet<u128>,
    status: HashMap<u128, Status>,
    expiries: BTreeSet<(u64, u128)>,
    clock: u64,
    skew_ns: u64,
    faults: VecDeque<Fault>,
}

pub struct SimTb {
    cluster_id: u128,
    state: Mutex<State>,
}

impl Default for SimTb {
    fn default() -> Self {
        Self::new()
    }
}

const NS: u64 = 1_000_000_000;
const ACCOUNT_FLAGS_KNOWN: u16 = (1 << 6) - 1;
const TRANSFER_FLAGS_KNOWN: u16 = (1 << 9) - 1;

fn wall_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos() as u64
}

impl SimTb {
    pub fn new() -> Self {
        Self {
            cluster_id: uuid::Uuid::new_v4().as_u128(),
            state: Mutex::new(State::default()),
        }
    }

    pub fn inject(&self, fault: Fault) {
        self.lock().faults.push_back(fault);
    }

    /// Moves the cluster clock forward (pending timeouts are measured on it).
    pub fn advance(&self, secs: u64) {
        self.lock().skew_ns += secs * NS;
    }

    /// Reservations neither posted, voided nor expired.
    pub fn open_reservations(&self) -> usize {
        let mut s = self.lock();
        let now = s.now();
        s.expire(now);
        s.status.values().filter(|v| **v == Status::Pending).count()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("sim state poisoned")
    }

    /// Runs `f` atomically, like one request to the cluster, honouring injected faults and
    /// the request size limit the live cluster negotiated (32 KiB).
    async fn request<T>(
        &self,
        op: Op,
        events: usize,
        f: impl FnOnce(&mut State) -> T,
    ) -> Result<T, ClientError> {
        let limit = match op {
            Op::CreateAccounts | Op::CreateTransfers => 253,
            Op::LookupAccounts | Op::LookupTransfers => 2_031,
            Op::QueryTransfers => 1,
        };
        if events > limit {
            return Err(ClientError("Too much data provided on this batch".into()));
        }
        // Let concurrent callers interleave between requests, as they would on a network.
        tokio::task::yield_now().await;
        let mut s = self.lock();
        let fault = match s.faults.front() {
            Some(Fault::Drop(o) | Fault::LoseReply(o) | Fault::Pass(o)) if *o == op => {
                s.faults.pop_front()
            }
            _ => None,
        };
        if let Some(Fault::Drop(_)) = fault {
            return Err(ClientError(format!("injected: {op:?} dropped")));
        }
        let now = s.now();
        s.expire(now);
        let out = f(&mut s);
        match fault {
            Some(Fault::LoseReply(_)) => Err(ClientError(format!("injected: {op:?} reply lost"))),
            _ => Ok(out),
        }
    }
}

impl State {
    fn now(&self) -> u64 {
        wall_ns() + self.skew_ns
    }

    fn expire(&mut self, now: u64) {
        while let Some(&(at, id)) = self.expiries.first() {
            if at > now {
                break;
            }
            self.expiries.remove(&(at, id));
            self.status.insert(id, Status::Expired);
            let p = self.transfers[&id];
            self.accounts
                .get_mut(&p.debit_account_id)
                .expect("pending debit account")
                .debits_pending -= p.amount;
            self.accounts
                .get_mut(&p.credit_account_id)
                .expect("pending credit account")
                .credits_pending -= p.amount;
        }
    }

    /// TigerBeetle's `execute_create`: timestamps, linked chains and transient failures.
    fn execute<E>(
        &mut self,
        events: &[E],
        flags: impl Fn(&E) -> (bool, u128),
        mut apply: impl FnMut(&mut Self, u64, &E, &mut Vec<Undo>) -> u32,
        transient: impl Fn(u32) -> bool,
    ) -> Vec<u32> {
        let base = self.clock.max(self.now());
        self.clock = base + events.len() as u64;
        let mut results = vec![0u32; events.len()];
        let mut chain: Option<usize> = None;
        let mut broken = false;
        let mut undo = Vec::new();
        for (i, event) in events.iter().enumerate() {
            let (linked, id) = flags(event);
            let ts = base + i as u64 + 1;
            let result = 'r: {
                if linked {
                    if chain.is_none() {
                        chain = Some(i);
                        undo.clear();
                    }
                    if i == events.len() - 1 {
                        break 'r 2; // linked_event_chain_open
                    }
                }
                if broken {
                    break 'r 1; // linked_event_failed
                }
                apply(self, ts, event, &mut undo)
            };
            if result != 0 {
                if let Some(start) = chain {
                    if !broken {
                        broken = true;
                        self.rollback(&mut undo);
                        results[start..i].fill(1);
                    }
                }
                if transient(result) {
                    self.failed.insert(id);
                }
            }
            results[i] = result;
            if chain.is_some() && (!linked || result == 2) {
                chain = None;
                broken = false;
            }
            if chain.is_none() {
                undo.clear();
            }
        }
        results
    }

    fn rollback(&mut self, undo: &mut Vec<Undo>) {
        while let Some(u) = undo.pop() {
            match u {
                Undo::Account(a) => {
                    self.accounts.insert(a.id, a);
                }
                Undo::NewAccount(id) => {
                    self.accounts.remove(&id);
                }
                Undo::Transfer(id, ts) => {
                    self.transfers.remove(&id);
                    self.by_time.remove(&ts);
                }
                Undo::Status(id, Some(s)) => {
                    self.status.insert(id, s);
                }
                Undo::Status(id, None) => {
                    self.status.remove(&id);
                }
                Undo::Expiry(key, true) => {
                    self.expiries.remove(&key);
                }
                Undo::Expiry(key, false) => {
                    self.expiries.insert(key);
                }
            }
        }
    }

    fn create_account(&mut self, ts: u64, a: &Account, undo: &mut Vec<Undo>) -> u32 {
        type R = AccountResult;
        if a.timestamp != 0 {
            return R::TIMESTAMP_MUST_BE_ZERO.0;
        }
        if a.flags & !ACCOUNT_FLAGS_KNOWN != 0 {
            return R::RESERVED_FLAG.0;
        }
        assert_eq!(a.flags & af::IMPORTED, 0, "SimTb does not model imports");
        if a.id == 0 {
            return R::ID_MUST_NOT_BE_ZERO.0;
        }
        if a.id == u128::MAX {
            return R::ID_MUST_NOT_BE_INT_MAX.0;
        }
        if let Some(e) = self.accounts.get(&a.id) {
            return if a.flags != e.flags {
                R::EXISTS_WITH_DIFFERENT_FLAGS
            } else if a.user_data_128 != e.user_data_128 {
                R::EXISTS_WITH_DIFFERENT_USER_DATA_128
            } else if a.user_data_64 != e.user_data_64 {
                R::EXISTS_WITH_DIFFERENT_USER_DATA_64
            } else if a.user_data_32 != e.user_data_32 {
                R::EXISTS_WITH_DIFFERENT_USER_DATA_32
            } else if a.ledger != e.ledger {
                R::EXISTS_WITH_DIFFERENT_LEDGER
            } else if a.code != e.code {
                R::EXISTS_WITH_DIFFERENT_CODE
            } else {
                R::EXISTS
            }
            .0;
        }
        let limits = af::DEBITS_MUST_NOT_EXCEED_CREDITS | af::CREDITS_MUST_NOT_EXCEED_DEBITS;
        if a.flags & limits == limits {
            return R::FLAGS_ARE_MUTUALLY_EXCLUSIVE.0;
        }
        for (v, r) in [
            (a.debits_pending, R::DEBITS_PENDING_MUST_BE_ZERO),
            (a.debits_posted, R::DEBITS_POSTED_MUST_BE_ZERO),
            (a.credits_pending, R::CREDITS_PENDING_MUST_BE_ZERO),
            (a.credits_posted, R::CREDITS_POSTED_MUST_BE_ZERO),
        ] {
            if v != 0 {
                return r.0;
            }
        }
        if a.ledger == 0 {
            return R::LEDGER_MUST_NOT_BE_ZERO.0;
        }
        if a.code == 0 {
            return R::CODE_MUST_NOT_BE_ZERO.0;
        }
        self.accounts.insert(
            a.id,
            Account {
                timestamp: ts,
                ..*a
            },
        );
        undo.push(Undo::NewAccount(a.id));
        R::OK.0
    }

    fn update(&mut self, id: u128, undo: &mut Vec<Undo>, f: impl FnOnce(&mut Account)) {
        let a = self.accounts.get_mut(&id).expect("account exists");
        undo.push(Undo::Account(*a));
        f(a);
    }

    fn insert(&mut self, t: Transfer, undo: &mut Vec<Undo>) {
        self.transfers.insert(t.id, t);
        self.by_time.insert(t.timestamp, t.id);
        undo.push(Undo::Transfer(t.id, t.timestamp));
    }

    fn create_transfer(&mut self, ts: u64, t: &Transfer, undo: &mut Vec<Undo>) -> u32 {
        type R = TransferResult;
        if t.timestamp != 0 {
            return R::TIMESTAMP_MUST_BE_ZERO.0;
        }
        if t.flags & !TRANSFER_FLAGS_KNOWN != 0 {
            return R::RESERVED_FLAG.0;
        }
        assert!(!t.has(tf::IMPORTED), "SimTb does not model imports");
        if t.id == 0 {
            return R::ID_MUST_NOT_BE_ZERO.0;
        }
        if t.id == u128::MAX {
            return R::ID_MUST_NOT_BE_INT_MAX.0;
        }
        if let Some(e) = self.transfers.get(&t.id) {
            return self.transfer_exists(t, e).0;
        }
        if self.failed.contains(&t.id) {
            return R::ID_ALREADY_FAILED.0;
        }
        if t.has(tf::POST_PENDING_TRANSFER) || t.has(tf::VOID_PENDING_TRANSFER) {
            return self.post_or_void(ts, t, undo).0;
        }
        let checks = [
            (
                t.debit_account_id == 0,
                R::DEBIT_ACCOUNT_ID_MUST_NOT_BE_ZERO,
            ),
            (
                t.debit_account_id == u128::MAX,
                R::DEBIT_ACCOUNT_ID_MUST_NOT_BE_INT_MAX,
            ),
            (
                t.credit_account_id == 0,
                R::CREDIT_ACCOUNT_ID_MUST_NOT_BE_ZERO,
            ),
            (
                t.credit_account_id == u128::MAX,
                R::CREDIT_ACCOUNT_ID_MUST_NOT_BE_INT_MAX,
            ),
            (
                t.credit_account_id == t.debit_account_id,
                R::ACCOUNTS_MUST_BE_DIFFERENT,
            ),
            (t.pending_id != 0, R::PENDING_ID_MUST_BE_ZERO),
            (
                !t.has(tf::PENDING) && t.timeout != 0,
                R::TIMEOUT_RESERVED_FOR_PENDING_TRANSFER,
            ),
            (
                !t.has(tf::PENDING) && t.flags & (tf::CLOSING_DEBIT | tf::CLOSING_CREDIT) != 0,
                R::CLOSING_TRANSFER_MUST_BE_PENDING,
            ),
            (t.ledger == 0, R::LEDGER_MUST_NOT_BE_ZERO),
            (t.code == 0, R::CODE_MUST_NOT_BE_ZERO),
        ];
        if let Some((_, r)) = checks.iter().find(|(bad, _)| *bad) {
            return r.0;
        }
        let Some(dr) = self.accounts.get(&t.debit_account_id).copied() else {
            return R::DEBIT_ACCOUNT_NOT_FOUND.0;
        };
        let Some(cr) = self.accounts.get(&t.credit_account_id).copied() else {
            return R::CREDIT_ACCOUNT_NOT_FOUND.0;
        };
        if dr.ledger != cr.ledger {
            return R::ACCOUNTS_MUST_HAVE_THE_SAME_LEDGER.0;
        }
        if t.ledger != dr.ledger {
            return R::TRANSFER_MUST_HAVE_THE_SAME_LEDGER_AS_ACCOUNTS.0;
        }
        if dr.flags & af::CLOSED != 0 {
            return R::DEBIT_ACCOUNT_ALREADY_CLOSED.0;
        }
        if cr.flags & af::CLOSED != 0 {
            return R::CREDIT_ACCOUNT_ALREADY_CLOSED.0;
        }
        let mut amount = t.amount;
        if t.has(tf::BALANCING_DEBIT) {
            let used = dr.debits_posted.saturating_add(dr.debits_pending);
            amount = amount.min(dr.credits_posted.saturating_sub(used));
        }
        if t.has(tf::BALANCING_CREDIT) {
            let used = cr.credits_posted.saturating_add(cr.credits_pending);
            amount = amount.min(cr.debits_posted.saturating_sub(used));
        }
        let over = |a: u128, b: u128| a.checked_add(b).is_none();
        if t.has(tf::PENDING) {
            if over(amount, dr.debits_pending) {
                return R::OVERFLOWS_DEBITS_PENDING.0;
            }
            if over(amount, cr.credits_pending) {
                return R::OVERFLOWS_CREDITS_PENDING.0;
            }
        }
        if over(amount, dr.debits_posted) {
            return R::OVERFLOWS_DEBITS_POSTED.0;
        }
        if over(amount, cr.credits_posted) {
            return R::OVERFLOWS_CREDITS_POSTED.0;
        }
        if over(amount, dr.debits_pending + dr.debits_posted) {
            return R::OVERFLOWS_DEBITS.0;
        }
        if over(amount, cr.credits_pending + cr.credits_posted) {
            return R::OVERFLOWS_CREDITS.0;
        }
        if ts
            .checked_add(t.timeout as u64 * NS)
            .is_none_or(|e| e >= 1 << 63)
        {
            return R::OVERFLOWS_TIMEOUT.0;
        }
        if dr.flags & af::DEBITS_MUST_NOT_EXCEED_CREDITS != 0
            && dr.debits_pending + dr.debits_posted + amount > dr.credits_posted
        {
            return R::EXCEEDS_CREDITS.0;
        }
        if cr.flags & af::CREDITS_MUST_NOT_EXCEED_DEBITS != 0
            && cr.credits_pending + cr.credits_posted + amount > cr.debits_posted
        {
            return R::EXCEEDS_DEBITS.0;
        }
        self.insert(
            Transfer {
                amount,
                timestamp: ts,
                ..*t
            },
            undo,
        );
        let pending = t.has(tf::PENDING);
        self.update(t.debit_account_id, undo, |a| {
            if pending {
                a.debits_pending += amount;
            } else {
                a.debits_posted += amount;
            }
            if t.has(tf::CLOSING_DEBIT) {
                a.flags |= af::CLOSED;
            }
        });
        self.update(t.credit_account_id, undo, |a| {
            if pending {
                a.credits_pending += amount;
            } else {
                a.credits_posted += amount;
            }
            if t.has(tf::CLOSING_CREDIT) {
                a.flags |= af::CLOSED;
            }
        });
        if pending {
            undo.push(Undo::Status(t.id, None));
            self.status.insert(t.id, Status::Pending);
            if t.timeout > 0 {
                let key = (ts + t.timeout as u64 * NS, t.id);
                self.expiries.insert(key);
                undo.push(Undo::Expiry(key, true));
            }
        }
        R::OK.0
    }

    fn transfer_exists(&self, t: &Transfer, e: &Transfer) -> TransferResult {
        type R = TransferResult;
        if t.flags != e.flags {
            return R::EXISTS_WITH_DIFFERENT_FLAGS;
        }
        if t.pending_id != e.pending_id {
            return R::EXISTS_WITH_DIFFERENT_PENDING_ID;
        }
        if t.timeout != e.timeout {
            return R::EXISTS_WITH_DIFFERENT_TIMEOUT;
        }
        if t.has(tf::POST_PENDING_TRANSFER) || t.has(tf::VOID_PENDING_TRANSFER) {
            let p = &self.transfers[&t.pending_id];
            let inherited = |mine: u128, theirs: u128, pending: u128| {
                if mine == 0 {
                    theirs != pending
                } else {
                    mine != theirs
                }
            };
            if t.debit_account_id != 0 && t.debit_account_id != e.debit_account_id {
                return R::EXISTS_WITH_DIFFERENT_DEBIT_ACCOUNT_ID;
            }
            if t.credit_account_id != 0 && t.credit_account_id != e.credit_account_id {
                return R::EXISTS_WITH_DIFFERENT_CREDIT_ACCOUNT_ID;
            }
            let amount_differs = if t.has(tf::VOID_PENDING_TRANSFER) {
                if t.amount == 0 {
                    e.amount != p.amount
                } else {
                    t.amount != e.amount
                }
            } else if t.amount == u128::MAX {
                e.amount != p.amount
            } else {
                t.amount != e.amount
            };
            if amount_differs {
                return R::EXISTS_WITH_DIFFERENT_AMOUNT;
            }
            if inherited(t.user_data_128, e.user_data_128, p.user_data_128) {
                return R::EXISTS_WITH_DIFFERENT_USER_DATA_128;
            }
            if inherited(
                t.user_data_64 as u128,
                e.user_data_64 as u128,
                p.user_data_64 as u128,
            ) {
                return R::EXISTS_WITH_DIFFERENT_USER_DATA_64;
            }
            if inherited(
                t.user_data_32 as u128,
                e.user_data_32 as u128,
                p.user_data_32 as u128,
            ) {
                return R::EXISTS_WITH_DIFFERENT_USER_DATA_32;
            }
            if t.ledger != 0 && t.ledger != e.ledger {
                return R::EXISTS_WITH_DIFFERENT_LEDGER;
            }
            if t.code != 0 && t.code != e.code {
                return R::EXISTS_WITH_DIFFERENT_CODE;
            }
            return R::EXISTS;
        }
        let amount_differs = if t.flags & (tf::BALANCING_DEBIT | tf::BALANCING_CREDIT) != 0 {
            t.amount < e.amount
        } else {
            t.amount != e.amount
        };
        [
            (
                t.debit_account_id != e.debit_account_id,
                R::EXISTS_WITH_DIFFERENT_DEBIT_ACCOUNT_ID,
            ),
            (
                t.credit_account_id != e.credit_account_id,
                R::EXISTS_WITH_DIFFERENT_CREDIT_ACCOUNT_ID,
            ),
            (amount_differs, R::EXISTS_WITH_DIFFERENT_AMOUNT),
            (
                t.user_data_128 != e.user_data_128,
                R::EXISTS_WITH_DIFFERENT_USER_DATA_128,
            ),
            (
                t.user_data_64 != e.user_data_64,
                R::EXISTS_WITH_DIFFERENT_USER_DATA_64,
            ),
            (
                t.user_data_32 != e.user_data_32,
                R::EXISTS_WITH_DIFFERENT_USER_DATA_32,
            ),
            (t.ledger != e.ledger, R::EXISTS_WITH_DIFFERENT_LEDGER),
            (t.code != e.code, R::EXISTS_WITH_DIFFERENT_CODE),
        ]
        .into_iter()
        .find_map(|(bad, r)| bad.then_some(r))
        .unwrap_or(R::EXISTS)
    }

    fn post_or_void(&mut self, ts: u64, t: &Transfer, undo: &mut Vec<Undo>) -> TransferResult {
        type R = TransferResult;
        let post = t.has(tf::POST_PENDING_TRANSFER);
        let void = t.has(tf::VOID_PENDING_TRANSFER);
        let exclusive = tf::PENDING
            | tf::BALANCING_DEBIT
            | tf::BALANCING_CREDIT
            | tf::CLOSING_DEBIT
            | tf::CLOSING_CREDIT;
        if (post && void) || t.flags & exclusive != 0 {
            return R::FLAGS_ARE_MUTUALLY_EXCLUSIVE;
        }
        if t.pending_id == 0 {
            return R::PENDING_ID_MUST_NOT_BE_ZERO;
        }
        if t.pending_id == u128::MAX {
            return R::PENDING_ID_MUST_NOT_BE_INT_MAX;
        }
        if t.pending_id == t.id {
            return R::PENDING_ID_MUST_BE_DIFFERENT;
        }
        if t.timeout != 0 {
            return R::TIMEOUT_RESERVED_FOR_PENDING_TRANSFER;
        }
        let Some(p) = self.transfers.get(&t.pending_id).copied() else {
            return R::PENDING_TRANSFER_NOT_FOUND;
        };
        if !p.has(tf::PENDING) {
            return R::PENDING_TRANSFER_NOT_PENDING;
        }
        if t.debit_account_id > 0 && t.debit_account_id != p.debit_account_id {
            return R::PENDING_TRANSFER_HAS_DIFFERENT_DEBIT_ACCOUNT_ID;
        }
        if t.credit_account_id > 0 && t.credit_account_id != p.credit_account_id {
            return R::PENDING_TRANSFER_HAS_DIFFERENT_CREDIT_ACCOUNT_ID;
        }
        if t.ledger > 0 && t.ledger != p.ledger {
            return R::PENDING_TRANSFER_HAS_DIFFERENT_LEDGER;
        }
        if t.code > 0 && t.code != p.code {
            return R::PENDING_TRANSFER_HAS_DIFFERENT_CODE;
        }
        let amount = match (void, t.amount) {
            (true, 0) => p.amount,
            (false, u128::MAX) => p.amount,
            (_, a) => a,
        };
        if amount > p.amount {
            return R::EXCEEDS_PENDING_TRANSFER_AMOUNT;
        }
        if void && amount < p.amount {
            return R::PENDING_TRANSFER_HAS_DIFFERENT_AMOUNT;
        }
        match self.status[&p.id] {
            Status::Pending => {}
            Status::Posted => return R::PENDING_TRANSFER_ALREADY_POSTED,
            Status::Voided => return R::PENDING_TRANSFER_ALREADY_VOIDED,
            Status::Expired => return R::PENDING_TRANSFER_EXPIRED,
        }
        let expires_at = (p.timeout > 0).then(|| p.timestamp + p.timeout as u64 * NS);
        if expires_at.is_some_and(|at| at <= ts) {
            return R::PENDING_TRANSFER_EXPIRED;
        }
        let pick = |mine: u128, pending: u128| if mine > 0 { mine } else { pending };
        self.insert(
            Transfer {
                id: t.id,
                debit_account_id: p.debit_account_id,
                credit_account_id: p.credit_account_id,
                amount,
                pending_id: t.pending_id,
                user_data_128: pick(t.user_data_128, p.user_data_128),
                user_data_64: pick(t.user_data_64 as u128, p.user_data_64 as u128) as u64,
                user_data_32: pick(t.user_data_32 as u128, p.user_data_32 as u128) as u32,
                timeout: 0,
                ledger: p.ledger,
                code: p.code,
                flags: t.flags,
                timestamp: ts,
            },
            undo,
        );
        if let Some(at) = expires_at {
            self.expiries.remove(&(at, p.id));
            undo.push(Undo::Expiry((at, p.id), false));
        }
        undo.push(Undo::Status(p.id, Some(Status::Pending)));
        self.status
            .insert(p.id, if post { Status::Posted } else { Status::Voided });
        self.update(p.debit_account_id, undo, |a| {
            a.debits_pending -= p.amount;
            if post {
                a.debits_posted += amount;
            }
        });
        self.update(p.credit_account_id, undo, |a| {
            a.credits_pending -= p.amount;
            if post {
                a.credits_posted += amount;
            }
        });
        R::OK
    }
}

impl TbClient for SimTb {
    async fn create_accounts(
        &self,
        accounts: Vec<Account>,
    ) -> Result<Vec<AccountResult>, ClientError> {
        self.request(Op::CreateAccounts, accounts.len(), |s| {
            s.execute(
                &accounts,
                |a| (a.flags & af::LINKED != 0, a.id),
                |s, ts, a, undo| s.create_account(ts, a, undo),
                |_| false,
            )
            .into_iter()
            .map(AccountResult)
            .collect()
        })
        .await
    }

    async fn create_transfers(
        &self,
        transfers: Vec<Transfer>,
    ) -> Result<Vec<TransferResult>, ClientError> {
        self.request(Op::CreateTransfers, transfers.len(), |s| {
            s.execute(
                &transfers,
                |t| (t.has(tf::LINKED), t.id),
                |s, ts, t, undo| s.create_transfer(ts, t, undo),
                |r| TransferResult(r).transient(),
            )
            .into_iter()
            .map(TransferResult)
            .collect()
        })
        .await
    }

    async fn lookup_accounts(&self, ids: Vec<u128>) -> Result<Vec<Account>, ClientError> {
        self.request(Op::LookupAccounts, ids.len(), |s| {
            ids.iter()
                .filter_map(|id| s.accounts.get(id))
                .copied()
                .collect()
        })
        .await
    }

    async fn lookup_transfers(&self, ids: Vec<u128>) -> Result<Vec<Transfer>, ClientError> {
        self.request(Op::LookupTransfers, ids.len(), |s| {
            ids.iter()
                .filter_map(|id| s.transfers.get(id))
                .copied()
                .collect()
        })
        .await
    }

    async fn query_transfers(&self, f: QueryFilter) -> Result<Vec<Transfer>, ClientError> {
        self.request(Op::QueryTransfers, 1, |s| {
            let max = if f.timestamp_max == 0 {
                u64::MAX
            } else {
                f.timestamp_max
            };
            let range = s
                .by_time
                .range(f.timestamp_min..=max)
                .map(|(_, id)| &s.transfers[id]);
            let keep = |t: &&Transfer| {
                (f.user_data_128 == 0 || t.user_data_128 == f.user_data_128)
                    && (f.user_data_64 == 0 || t.user_data_64 == f.user_data_64)
                    && (f.user_data_32 == 0 || t.user_data_32 == f.user_data_32)
                    && (f.ledger == 0 || t.ledger == f.ledger)
                    && (f.code == 0 || t.code == f.code)
            };
            let limit = f.limit as usize;
            if f.reversed {
                range.rev().filter(keep).take(limit).copied().collect()
            } else {
                range.filter(keep).take(limit).copied().collect()
            }
        })
        .await
    }

    fn cluster_id(&self) -> u128 {
        self.cluster_id
    }
}
