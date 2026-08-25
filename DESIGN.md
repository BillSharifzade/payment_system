# Payment System — Backend Design & Architecture

> Status: **DRAFT FOR REVIEW.** Nothing here is implemented yet. This document is for us to argue over and refine before a single line of code is written.
>
> Author pass: review & hardening of the initial architecture sketch.
> Target: a domestic (and eventually Central-Asia regional) payment system backend in Rust — fast, secure, correct, and operationally resilient.

---

## 0. How to read this document

The previous draft was a good skeleton. This document keeps the good parts (Rust, Postgres, append-only ledger, cryptographic tamper-evidence instead of blockchain, native app) and **fixes or deepens the parts that would have bitten you in production**. Wherever I changed a decision, there is a **"⚠️ Change from initial draft"** note explaining *why*, so you can push back.

The single most important idea in this whole document:

> **In a payment system, "fast" is worthless if it is ever wrong. Correctness is the feature. Speed is the second feature. We design for correctness first and then make the correct thing fast — never the other way around.**

---

## 1. Goals, non-goals, and an honest reality check

### 1.1 Goals
- **Correctness:** money is never created, destroyed, double-spent, or lost — even under crashes, retries, and concurrency.
- **Speed:** p99 latency for a transfer under ~50 ms server-side; throughput that scales horizontally to tens of thousands of transfers/sec.
- **Security:** defense in depth — cryptographic auth, tamper-evident ledger, encrypted secrets, full audit trail.
- **Resilience ("unkillable"):** survives single-node failure with zero data loss; degrades gracefully; recovers automatically.
- **Auditability:** every state change is traceable, attributable, and cryptographically verifiable after the fact.

### 1.2 Non-goals (at least for v1)
- Being a blockchain / cryptocurrency. (See §6.)
- Card issuing / acquiring with raw PAN handling. (That triggers full PCI-DSS scope — see §1.3. We avoid touching card numbers if we can.)
- Multi-region active-active across continents. (We design so it's *possible* later, but we don't build it on day one — it's a huge complexity multiplier.)

### 1.3 ⚠️ The reality check the first draft skipped: compliance & licensing
This is not optional and it shapes the architecture, so it goes first.

A system that holds and moves other people's money in Tajikistan is a **regulated activity**. Before this is a product (not a prototype), you will need to engage with:

- **National Bank of Tajikistan (NBT)** — licensing as a payment institution / e-money issuer. They define capital requirements, reporting, and what you're legally allowed to do.
- **KYC (Know Your Customer)** — identity verification on signup. This is a backend subsystem, not an afterthought.
- **AML / CFT (Anti-Money-Laundering / Counter-Financing of Terrorism)** — transaction monitoring, suspicious-activity reporting, sanctions screening. This is *also* a backend subsystem.
- **Data residency / data protection** — where user data physically lives may be legally constrained.
- **PCI-DSS** — *only if* we ever touch raw card data. **Strong recommendation: don't.** Integrate cards via a tokenizing processor so card numbers never enter our servers. This removes the single most expensive compliance burden in payments.

**Why this is in an engineering doc:** KYC/AML/audit/reporting are not bolt-ons. They are first-class data and first-class code paths. Designing them in from the start is cheap; retrofitting them is a rewrite. The architecture below has explicit homes for them.

> **Decision to confirm with you:** Are we building (a) a wallet / e-money system (users hold balances with us), (b) a payment *rails*/switch (we route between banks), or (c) a merchant payment gateway? The core ledger is similar for all three, but the integrations and licensing differ a lot. **This doc assumes (a) a wallet/e-money model** as the primary case, since that's what "banking app for Tajikistan" implies. Tell me if that's wrong.

---

## 2. Architecture principles

1. **The ledger is the source of truth.** Balances are *derived from* the ledger, never edited directly.
2. **Double-entry accounting.** Every movement of money is balanced debits and credits that sum to zero. This is non-negotiable and is the biggest correctness change from the first draft (§4).
3. **Money is integers.** No floating point, ever. (§3)
4. **Idempotency everywhere on the write path.** Every state-changing request can be safely retried. (§5.3)
5. **Separate correctness from auditability.** ACID transactions guarantee correctness *synchronously*; cryptographic tamper-evidence is built *asynchronously* so it never bottlenecks a payment. (§6 — this is a key fix.)
6. **Boring, proven technology in the hot path.** Postgres, not a fashionable database. We earn our reliability.
7. **Everything is observable.** If we can't measure it, we can't claim it's fast or up. (§13)
8. **Deterministic, testable core.** The money-moving logic is pure and simulation-tested. (§14)

---

## 3. Money representation (get this wrong and nothing else matters)

⚠️ **Change from initial draft:** the first draft listed `rust_decimal` for amounts. We will use **integer minor units** as the canonical representation instead.

- Store every amount as a **signed 64-bit (or 128-bit) integer count of the currency's smallest unit.**
  - TJS (Somoni) has 100 diram → store `1 TJS` as `100`.
  - Each currency carries a known `exponent` (TJS = 2, JPY = 0, etc.).
- **Never `f32`/`f64` for money.** Floating point cannot represent `0.10` exactly; it silently loses cents. This is the #1 cause of "the books don't balance" bugs.
- `i64` of minor units holds ~92 quadrillion TJS-diram — plenty. Use `i128` in the ledger core if you want enormous headroom and overflow paranoia.
- All arithmetic is **checked** (`checked_add`, `checked_sub`) — an overflow must be an error, never a silent wrap.
- `rust_decimal` is acceptable only at the *edges* (e.g., FX rate math, interest) where fractional precision is needed, and results are rounded to integer minor units with an explicit, documented rounding policy before they ever hit the ledger.

A `Money` type wraps `{ amount: i128 (minor units), currency: CurrencyCode }` and refuses to add two different currencies at compile/runtime. This single type prevents a whole category of disasters.

---

## 4. The ledger: double-entry, not single-entry (the heart of the system)

⚠️ **Biggest change from the initial draft.** The first draft modeled a transaction as `{from_account, to_account, amount}` — a single row describing a movement. That works for a toy, but real financial systems use **double-entry bookkeeping**, and here's why it's worth it:

### 4.1 What double-entry means
Every transaction is a set of **entries** (postings) against accounts, where **debits equal credits** — the entries sum to exactly zero. Money is never created or destroyed; it only moves between accounts.

A simple transfer of 50.00 TJS from Alice to Bob:

```
Transaction T1 (transfer)
  Entry: DEBIT  Alice's wallet   -5000   (minor units)
  Entry: CREDIT Bob's wallet     +5000
  Sum = 0  ✓  (enforced as a hard invariant)
```

Bringing money *into* the system (a deposit from a partner bank) credits a user and debits a **system "external/nostro" account**:

```
Transaction T2 (deposit)
  Entry: DEBIT  System:BankSettlement  -10000
  Entry: CREDIT Alice's wallet         +10000
  Sum = 0 ✓
```

### 4.2 Why this matters (the payoff)
- **Self-checking:** the entire database can be audited with one query: *the sum of every entry across all accounts must be zero, always.* If it isn't, you have a bug and you know it immediately. A single-entry model cannot do this.
- **Fees, splits, FX, reversals** all become natural: a transfer with a fee is just three entries (debit payer, credit payee, credit fee-revenue account). FX is a transaction touching two currency accounts plus an FX gain/loss account.
- **Reversals are append-only:** you never delete or mutate an entry. A reversal is a *new* transaction with opposite entries. History is immutable.
- **It's how every real bank, and TigerBeetle, and Stripe's ledger work.** This is the well-trodden path.

### 4.3 Account types
- **User wallets** (liability accounts — money we owe users)
- **System accounts:** bank-settlement/nostro, fee revenue, FX gain/loss, suspense/clearing, write-off. (assets/revenue/equity)
- Every account has a `currency`; an account holds exactly one currency. Multi-currency users have multiple accounts.

### 4.4 Balances: derived but materialized
- **Truth:** balance of an account = sum of its entries. Always reconstructable from history.
- **Speed:** we keep a **materialized balance** per account, updated *inside the same database transaction* that writes the entries. So a balance read is a single indexed row, but it can always be re-derived and reconciled against the entries (a background job does this continuously — see §13).
- We track two balances: **posted** (settled) and **available** (posted minus holds/pending authorizations). This distinction is what lets you "reserve" funds for a pending payment without double-spending.

---

## 5. Concurrency & correctness (the hard part the first draft didn't address)

This is where payment systems actually fail. Two requests hit Alice's account at the same time; both read balance = 100; both approve a 100 payment; now she's spent 200. We must make that impossible.

### 5.1 The invariant
> For any non-credit account, **available balance must never go negative** (unless an explicit overdraft line exists). This must hold under arbitrary concurrency.

### 5.2 How we enforce it — chosen approach
**Pessimistic row locking inside a serializable-enough transaction**, ordered to avoid deadlocks:

1. Begin a Postgres transaction at `REPEATABLE READ` (or `SERIALIZABLE` for the most sensitive flows).
2. `SELECT ... FOR UPDATE` the affected account rows **in a deterministic order** (e.g., sorted by account ID) to prevent deadlocks between two transfers touching the same pair.
3. Check the available balance against the debit.
4. Insert the entries, update materialized balances, commit.

Locking only the specific account rows means unrelated accounts proceed fully in parallel — high concurrency, but the *same* account is serialized (which is exactly what correctness requires).

> **Alternative considered (TigerBeetle-style):** a dedicated single-threaded, in-memory accounting state machine that processes transfers from a queue at ~1M/s with no locks because there's only one writer. Phenomenal throughput and correctness, but it's a separate system to operate. **Plan: start with Postgres row-locking (simpler, one system). Abstract the ledger behind a `LedgerEngine` trait so we can swap in TigerBeetle later if throughput demands it — without rewriting business logic.** See §15.

### 5.3 Idempotency (retries must never double-charge)
- Every state-changing endpoint requires a client-supplied **`Idempotency-Key`** (UUID).
- We store `(idempotency_key → request_hash, response, status)` in a dedicated table.
- On a repeat: same key + same request body → return the *stored* response, do nothing. Same key + *different* body → reject (client bug / possible attack).
- The idempotency record is written in the **same transaction** as the ledger entries, so "did it happen?" and "what was the result?" are never inconsistent.

### 5.4 Exactly-once across services: the transactional outbox
⚠️ **Addition.** The first draft had the backend writing to Postgres *and* publishing to NATS. That's a **dual write** — if the DB commit succeeds but the NATS publish fails (or vice versa), you get lost or phantom events. We avoid this with the **outbox pattern**:

- Events to publish are written into an `outbox` table **in the same DB transaction** as the business change.
- A separate relay process reads the outbox and publishes to NATS, marking rows as sent (at-least-once; consumers are idempotent).
- Result: the event is published **if and only if** the business change committed. No lost events, no phantom events.

---

## 6. "Blockchain?" — the honest answer, refined

The first draft's instinct was right: **you want the cryptographic guarantees of a blockchain, not a blockchain.** I agree and I'll sharpen it, including a way to honor your blockchain interest without paying its costs.

### 6.1 What you actually want
Tamper-**evidence**: if anyone (including a malicious DBA or an attacker who breaches the DB) alters history, it is **detectable and provable**. You do *not* need decentralized consensus — you are the trusted operator. Consensus is what makes blockchains slow; you'd be paying for a property you don't need.

### 6.2 ⚠️ The hidden bottleneck in the first draft's design
The first draft chained every transaction to the hash of the previous one (`prev_hash = SHA256(previous entry)`). That sounds great but it **serializes all writes globally** — you can only append one transaction at a time across the *entire system*, because each needs the previous one's hash. That caps you at single-writer throughput and directly contradicts the "extremely fast" goal.

### 6.3 The fix: asynchronous, batched checkpointing (Merkle anchoring)
Decouple correctness from cryptographic proof:

1. **Correctness path (synchronous, fast):** the double-entry ledger commits in Postgres with ACID guarantees. No hashing on the critical path. This is what the user waits for.
2. **Tamper-evidence path (asynchronous, batched):** a background "sealer" process groups committed transactions into **checkpoints** (e.g., every 200 ms or every N transactions), builds a **Merkle tree** over them, and writes a signed checkpoint record: `{ checkpoint_id, merkle_root, prev_checkpoint_hash, signature (Ed25519), time }`. The *checkpoints* form the hash chain — far fewer of them, so the chain is never the bottleneck.
3. Any individual transaction can be proven to belong to a checkpoint via a short **Merkle proof**, and checkpoints chain to each other. Altering any historical transaction breaks a Merkle root, which breaks the signed chain — detectable.

### 6.4 Optional public anchoring — your "blockchain, the useful 5%"
If you want externally-verifiable, "nobody can claim we rewrote history" guarantees: **periodically publish the latest checkpoint Merkle root to a public ledger** (e.g., a public blockchain, or even a notarized public log) — say, hourly. This costs almost nothing (one tiny write per hour), runs entirely off the hot path, and gives you blockchain-grade external auditability without blockchain's latency or throughput penalty. This is the genuinely smart use of a public chain for a payment system, and it's *optional and additive*.

**Net:** full tamper-evidence, no throughput penalty, optional public verifiability. This is strictly better than the first draft's design.

---

## 7. System architecture (components)

```
                         ┌────────────────────────────┐
   Flutter app  ───┐     │      Edge / API Gateway     │
   Web dashboard ──┼────►│  TLS termination, WAF,      │
   Partner banks ──┘     │  rate limit, mTLS for       │
        (mTLS)           │  partners                   │
                         └──────────────┬──────────────┘
                                        │
                         ┌──────────────▼──────────────┐
                         │     Rust API service(s)      │   (stateless, horizontally scaled)
                         │  axum + tokio + tower        │
                         │  auth · validation · routing │
                         └──────────────┬──────────────┘
                                        │
              ┌─────────────────────────┼───────────────────────────┐
              │                         │                           │
     ┌────────▼────────┐    ┌───────────▼──────────┐     ┌──────────▼─────────┐
     │  Ledger engine   │   │   Supporting services │     │   Async workers     │
     │ (double-entry,   │   │  auth, KYC, AML,      │     │  (outbox relay,     │
     │  row-locked txns)│   │  notifications, FX    │     │  sealer/Merkle,     │
     └────────┬────────┘    └───────────┬──────────┘     │  reconciliation,    │
              │                         │                 │  AML screening)     │
              │                         │                 └──────────┬─────────┘
     ┌────────▼─────────────────────────▼────────────────────────────▼─────────┐
     │  PostgreSQL (primary + sync standby + read replicas)                     │
     │  ledger · accounts · users · idempotency · outbox · audit · checkpoints  │
     └──────────────────────────────────────────────────────────────────────────┘
              │                         │
     ┌────────▼────────┐      ┌─────────▼─────────┐
     │  Redis          │      │  NATS (events,    │
     │ (sessions,      │      │  async jobs)      │
     │  rate limits,   │      └───────────────────┘
     │  hot reads)     │
     └─────────────────┘
```

Key property: the **API service is stateless** (all state in Postgres/Redis), so we scale it horizontally and any instance can die without data loss.

---

## 8. Technology stack

| Concern | Choice | Notes / changes |
|---|---|---|
| Language | **Rust** | Agreed — no GC pauses, memory safety, great concurrency. |
| HTTP framework | **axum** | ⚠️ Pick **one** and standardize (first draft listed axum *or* actix). axum: tower ecosystem, cleaner middleware story, tokio-native. |
| Async runtime | **tokio** | Standard. |
| Middleware | **tower / tower-http** | Rate limit, timeouts, tracing, CORS, circuit breaking. |
| Database | **PostgreSQL** | ACID, mature, replication, partitioning. Source of truth. |
| DB access | **sqlx** | Compile-time-checked SQL → kills SQL injection + schema drift. ⚠️ Prefer over an ORM for a ledger; we want to *see* our SQL. |
| Connection pooling | **PgBouncer** (transaction mode) in front, `sqlx` pool in app | Postgres connections are expensive; pooling is mandatory at scale. |
| Cache / sessions / rate-limit | **Redis** (`deadpool-redis`) | Sub-ms; never the source of truth for money. |
| Messaging / async jobs | **NATS (JetStream)** | Lighter than RabbitMQ, persistent streams, good Rust client. |
| Auth — sessions | **JWT (short-lived access) + opaque refresh tokens** | ⚠️ Refresh tokens are opaque + server-side revocable (pure JWT can't be revoked; that's unacceptable for banking). |
| Password hashing | **Argon2id** | Agreed. |
| Signing | **Ed25519** (`ed25519-dalek`) | For checkpoint sealing, partner/webhook signatures. |
| Key management | **HSM / KMS** (cloud KMS, or SoftHSM/Vault to start) | ⚠️ Upgrade from "age/Vault": signing keys must live in an HSM/KMS and never touch app memory in the clear. PCI/financial-grade. |
| Hashing | **SHA-256 / BLAKE3** | BLAKE3 for speed on the Merkle path. |
| Observability | **OpenTelemetry + Prometheus + Grafana + Loki/Tempo** | §13. |
| Migrations | **sqlx migrate** or **refinery** | Versioned, reviewed, forward-only. |
| Testing | `cargo test` + **proptest** + deterministic simulation harness | §14. |

---

## 9. Data model (sketch — not final)

```sql
-- Currencies and their minor-unit exponent
currencies(code PK, exponent SMALLINT, name)

-- Users / identity (KYC lives adjacent)
users(id PK, phone, email, status, created_at, ...)
kyc_records(id PK, user_id FK, level, doc_refs, verified_at, status, ...)

-- Accounts: each holds exactly one currency
accounts(
  id PK, owner_user_id FK NULL,   -- NULL for system accounts
  type ENUM(user_wallet, system_settlement, system_fee, system_fx, suspense, ...),
  currency FK -> currencies,
  status ENUM(active, frozen, closed),
  created_at
)

-- Materialized balances (derived, reconciled continuously)
balances(
  account_id PK FK,
  posted_minor   NUMERIC(38,0) NOT NULL,   -- or BIGINT; integer minor units
  available_minor NUMERIC(38,0) NOT NULL,
  version BIGINT NOT NULL,                  -- optimistic-lock guard
  updated_at
)

-- Transactions: the logical money event
transactions(
  id PK, type, status ENUM(pending, posted, failed, reversed),
  idempotency_key UNIQUE, created_at, metadata JSONB
)

-- Entries: the double-entry postings (append-only, immutable)
entries(
  id PK, transaction_id FK,
  account_id FK,
  direction ENUM(debit, credit),
  amount_minor NUMERIC(38,0) NOT NULL CHECK (amount_minor > 0),
  currency FK,
  created_at
  -- INVARIANT: per transaction, sum(credits) - sum(debits) == 0
)

-- Idempotency
idempotency_keys(key PK, request_hash, response_body, status_code, created_at, expires_at)

-- Transactional outbox
outbox(id PK, aggregate_id, event_type, payload JSONB, created_at, sent_at NULL)

-- Cryptographic checkpoints (the hash chain of Merkle roots)
checkpoints(
  id PK, seq BIGINT UNIQUE,
  merkle_root BYTEA, prev_checkpoint_hash BYTEA,
  signature BYTEA, from_txn_id, to_txn_id, created_at
)

-- Audit log (who did what, append-only)
audit_log(id PK, actor, action, target, before JSONB, after JSONB, ip, at)
```

Notes:
- `entries` and `transactions` are **append-only**. No `UPDATE`/`DELETE`. Corrections are new reversing transactions.
- Partition `entries` and `audit_log` by time (monthly) for manageable size and fast pruning/archival.
- `CHECK` constraints and a deferred constraint trigger enforce the "entries sum to zero" invariant at the DB level as a backstop, even if app code has a bug.

---

## 10. API design

- **External clients (app, web):** REST/JSON over HTTPS. Versioned (`/v1/...`). Simple, debuggable, universally supported.
- **Internal service-to-service & partner banks:** **gRPC** (typed, fast, streaming) with **mTLS**.
- **Webhooks to partners:** signed (Ed25519) payloads, with retries and idempotency.
- Core endpoints (v1): auth (register/login/refresh/logout), accounts (list/balance), transfers (create/get/list — all idempotent), deposits/withdrawals (via partner integration), admin (freeze account, view ledger, reconciliation reports).
- **Every write endpoint:** requires `Idempotency-Key`, validates input strictly (reject unknown fields), and returns a stable, documented error taxonomy.

---

## 11. Security model (defense in depth)

- **Transport:** TLS 1.3 everywhere; **certificate pinning** in the mobile app; **mTLS** for partners.
- **AuthN:** Argon2id passwords; short-lived JWT access tokens (~15 min) + server-side-revocable opaque refresh tokens; biometric unlock on device (private key in the device secure enclave).
- **AuthZ:** role- and scope-based; every sensitive action authorized server-side, never trusting the client.
- **Secrets & keys:** in KMS/HSM; app never holds signing keys in cleartext; secrets injected at runtime, never in the repo. Key rotation policy from day one.
- **Encryption at rest:** DB-level + column-level encryption for PII.
- **Input safety:** `sqlx` compile-checked queries (no string-built SQL); strict deserialization; size/rate limits.
- **Rate limiting & abuse:** per-IP and per-user, in Redis; progressive backoff; lockout on credential stuffing.
- **Fraud/AML hooks:** every transfer passes through a screening step (velocity checks, sanctions list, anomaly scoring) — can run inline for blocking rules and async for scoring.
- **Audit:** append-only `audit_log` for every privileged/admin action and every money movement; tamper-evident via the checkpoint chain.
- **Least privilege:** DB roles scoped per service; the API role *cannot* `DELETE` from `entries`.
- **Secure SDLC:** `cargo audit` / `cargo deny` in CI for dependency CVEs; secret scanning; code review required for ledger-touching code.

---

## 12. Reliability — what "unkillable" actually requires

"Unkillable" is an operational property, not a code property. Concretely:

- **No single point of failure:**
  - Postgres: **primary + synchronous standby** (zero-data-loss failover) + async read replicas. Automated failover (Patroni or managed equivalent).
  - API: stateless, ≥3 instances behind the gateway across availability zones.
  - Redis, NATS: clustered/replicated.
- **Zero data loss on crash:** synchronous replication means a committed transaction survives the loss of the primary.
- **Idempotency + outbox** mean retries and partial failures never corrupt state. (§5)
- **Graceful degradation:** if a non-critical dependency (e.g., notifications) is down, queue and continue; never fail a payment because a push notification service is down. **Circuit breakers** (tower) isolate failing dependencies.
- **Graceful shutdown:** drain in-flight requests on deploy; zero-downtime rolling/blue-green deploys.
- **Backups & DR:** continuous WAL archiving + **point-in-time recovery**; backups encrypted, off-site, and **restore-tested on a schedule** (an untested backup is not a backup); documented RPO/RTO targets.
- **Chaos & game-days:** periodically kill nodes in staging and verify the system self-heals. You don't *have* an unkillable system until you've tried to kill it.
- **Health checks:** liveness/readiness endpoints; automatic removal of unhealthy instances.

> ⏸️ **§12.2–§12.9 are DEFERRED until the backend exists.** The full HA datacenter topology below is the *destination*, not the starting point. For now we build and test everything in **Docker Compose on one machine** (§12.10, "Dev"). Do not stand up Talos/Patroni/Vault/etc. until the ledger and API are real and tested. Parked here intentionally so it's ready when we need it.

### 12.1 Deployment context (decided)
- **Data residency:** ✅ all data stays **in-country, in a company-owned datacenter** in Tajikistan. This is good for both law and latency (servers next to users).
- **Hardware:** ✅ **abundant** — a 100+ machine datacenter. Redundancy is not budget-constrained.
- **Operators:** ⚠️ **exactly one** (you). This is the binding constraint and it drives every choice below.

> **Governing principle: machines are cheap, your attention is not.** Spend hardware freely on redundancy; spend ~zero on manual operations. Everything declarative, immutable, self-healing. One person cannot hand-tend 100 servers — but one person *can* maintain a Git repo that the infrastructure continuously converges to.

### 12.2 The five automation pillars (how one person runs a bank)
1. **GitOps — Git is the only source of truth for infra.** The desired state of the whole cluster lives in a Git repo; **Argo CD** (or Flux) continuously makes reality match it. You deploy by `git push`. You roll back with `git revert`. No `kubectl apply` by hand, ever.
2. **Immutable infrastructure.** Nodes are not pets you SSH into and tweak — they're cattle rebuilt from an image. No config drift, no "what did I change on node 37 last March?"
3. **Self-healing.** Kubernetes reschedules dead pods; Patroni fails the database over automatically; nodes that die are replaced from the same image. The system recovers *without you*.
4. **Observability-driven ops.** You do not go looking for problems — the system pages *you* (§13), with dashboards that tell you what and where. Alert on symptoms (latency, error rate, replication lag, reconciliation drift), not noise.
5. **Reproducibility.** Any machine — or the entire cluster — can be wiped and rebuilt from Git + images in minutes. This *is* your disaster recovery story, not a separate one.

### 12.3 ⚠️ Changed recommendation: Talos Linux for the Kubernetes tier
Earlier (when budget/scale was unknown) I suggested "Debian everywhere, Talos later." **With 100 machines and a solo operator, that flips.** At this scale the dominant risk is *config drift and manual toil across many nodes*, and the mitigation for that is immutable + declarative — which is exactly Talos Linux:

- **Talos** is an immutable, minimal, API-managed OS that runs *only* Kubernetes. No SSH, no shell, no package manager — nothing to drift, rot, or be attacked. You manage all nodes through one declarative config.
- The usual objection ("I can't SSH in to debug") is *mitigated for you* by strong observability (§13) and by the fact that immutable nodes simply break far less. The thing you'd SSH in to fix mostly stops happening.
- Trade-off: real learning curve. **Fallback if you want a gentler start: k3s on Debian Stable**, then migrate nodes to Talos once comfortable. Same Kubernetes API either way, so this choice is reversible and does not affect application code.

**Stateful data tiers stay on Debian Stable on dedicated bare metal — NOT in Kubernetes.** Postgres and (Phase 2) TigerBeetle hold the money; they get boring, debuggable, hand-controllable hosts with proper storage. *Stateless in the orchestrator, stateful on bare metal* is the rule.

### 12.4 Physical topology (initial footprint ≈ 20–25 of your 100 machines, room to grow)
Network is segmented into three security zones via VLANs/firewall: **edge → app → data**. Traffic only flows inward through defined choke points.

```
┌─ EDGE ZONE ────────────────────────────────────────────────┐
│  2× load balancers (HAProxy + keepalived, shared VIP)        │  ← HA ingress, TLS 1.3 termination,
│       active/standby, automatic failover                     │     mTLS for partner banks, WAF, rate-limit
└───────────────────────────────┬─────────────────────────────┘
                                 │ (only ingress → app)
┌─ APP ZONE: Kubernetes (Talos) ─▼─────────────────────────────┐
│  3× control-plane nodes (HA, embedded etcd quorum)           │
│  4–6× worker nodes:                                          │
│     • Rust API pods            (≥3 replicas, auto-healed)    │
│     • async workers            (sealer, outbox relay, recon, │
│                                  AML screening)              │
│     • NATS JetStream cluster   (3 replicas)                  │
│     • Redis                    (3 replicas, Sentinel)        │
│  Argo CD (GitOps controller) reconciles all of the above     │
└───────────────────────────────┬─────────────────────────────┘
                                 │ (only app → data)
┌─ DATA ZONE: dedicated bare metal (Debian), NOT in k8s ──────▼┐
│  Postgres HA via Patroni:                                    │
│     • 1 primary                                              │
│     • 1 synchronous standby   (zero-data-loss failover)      │
│     • 1+ async read replica   (reads / analytics)            │
│  3× etcd (Patroni's failover coordinator, isolated)          │
│  PgBouncer (transaction-mode pooling) in front of Postgres   │
│  pgBackRest → WAL archiving + full backups                   │
│  ── Phase 2 ──                                               │
│  6× TigerBeetle replicas (VSR consensus, hot ledger path)    │
└──────────────────────────────────────────────────────────────┘

┌─ SUPPORTING (own nodes, isolated from prod blast radius) ────┐
│  Observability:  Prometheus + Grafana + Loki + Tempo          │
│  Secrets:        HashiCorp Vault cluster (3 nodes, raft)      │
│  CI/CD + supply: Gitea (git) · CI runners · Harbor (registry  │
│                  with image vuln scanning) · Argo CD          │
│  Backups:        backup store + OFF-SITE encrypted copy       │
└──────────────────────────────────────────────────────────────┘
```

Why these specifics:
- **2 load balancers, not 1** — a single LB is a single point of failure in front of everything. keepalived floats a virtual IP between them; one dies, the other takes the VIP in ~seconds.
- **3 control-plane / 3 etcd / 3 NATS / 3 Redis / 3 Vault** — odd numbers because quorum-based systems need a majority to make decisions; 3 survives losing 1.
- **Synchronous Postgres standby** — a transaction isn't acknowledged until it's on two machines, so losing the primary loses **zero** committed money. This is the core of "unkillable" for the data that matters most.
- **Observability & Vault on separate nodes** — your monitoring must survive the outage it's reporting on, and your secret store must not share a failure domain with the things it unlocks.
- **Off-site backup** — a datacenter-level disaster (fire, flood, power) must not be able to destroy the only copy of the ledger. At least one encrypted copy lives in a physically separate location.

### 12.5 Provisioning & infrastructure-as-code (turning 100 bare machines into the above)
- **Bare-metal provisioning:** if the DC has a virtualization layer (Proxmox/VMware/OpenStack), drive it with **Terraform/OpenTofu**. If it's raw bare metal, use **PXE/netboot** to auto-install: Talos images for k8s nodes, Debian preseed for data nodes. Goal: *zero hand-installed machines.*
- **Data-tier host config:** **Ansible** playbooks (Postgres, Patroni, PgBouncer, pgBackRest, kernel tuning) — versioned in Git, idempotent, repeatable.
- **Kubernetes workloads:** all manifests/Helm charts in Git; **Argo CD** applies them. Nothing manual.
- **Result:** the entire stack is reconstructable from three Git repos (infra, ansible, k8s-manifests) + the container registry. That reconstructability is your DR plan.

### 12.6 Container & build strategy
- Rust → **single static binary** → **distroless or `scratch`** image (typically <20 MB; no shell, no package manager, minimal attack surface, instant cold-start).
- **Multi-stage Docker build:** builder stage compiles with cached dependencies; final stage copies only the binary. Reproducible builds; images tagged by Git commit SHA (immutable, traceable).
- Images pushed to **Harbor** (self-hosted registry, in-country) with **automatic CVE scanning** — a vulnerable image is blocked from deploy.

### 12.7 CI/CD pipeline (solo-operator friendly, safety-gated)
```
git push ─► CI: fmt · clippy -D warnings · cargo test · proptest · cargo audit · cargo deny
        ─► build multi-stage image ─► scan (Harbor) ─► push, tagged by commit SHA
        ─► update image tag in GitOps repo ─► Argo CD deploys to STAGING automatically
        ─► run integration + simulation + load tests against staging
        ─► PRODUCTION deploy requires your manual approval (a Git tag / Argo sync)
        ─► rolling deploy, health-gated; auto-rollback on failed health checks
```
Production never deploys without passing every gate **and** your explicit approval — because it's a bank, and you're the only set of eyes. Money-touching code (the `ledger` crate) gets the strictest checks.

### 12.8 Secrets & keys
- **HashiCorp Vault** is the in-country stand-in for a cloud KMS (no hyperscaler KMS is available locally). It holds DB credentials, service tokens, and issues short-lived dynamic secrets.
- **Signing keys** (Ed25519 checkpoint sealing, partner webhooks): held in Vault's transit engine now; **migrate to a physical HSM** appliance in the DC for production — the private key then *never* exists in application memory in cleartext. The DC owning real hardware makes a true HSM realistic, which is a security advantage over cloud.
- No secret is ever in Git or in an image. Apps fetch secrets from Vault at startup via short-lived tokens.

### 12.9 What this buys you (and the honest cost)
**Buys:** survive any single machine failure with zero downtime and zero data loss; rebuild any node or the whole cluster from Git; deploy and roll back safely as one person; full audit/observability; a security posture appropriate for money.

**Honest cost:** this is a *real* amount of technology to stand up (Talos, k8s, Patroni, Argo CD, Vault, Harbor, the observability stack). For a solo operator that's weeks of platform work **before** the payment app is in production. Mitigation — we build it in the phased order in §17 and **§12.10**, so you're never blocked waiting for the whole platform; the ledger core gets built and tested in parallel on a laptop/dev box.

### 12.10 Phased DevOps rollout (so the platform doesn't block the product)
- **Dev (now):** everything in **Docker Compose** on one machine — Postgres, Redis, NATS, the Rust service. Fast inner loop. You build and test the entire backend here while the DC platform is being stood up.
- **Staging (early):** a *minimal* k8s (even single-node k3s) + one Postgres, to exercise the real deploy path and run integration/load tests.
- **Production HA (before real users/money):** the full §12.4 topology — Talos k8s, Patroni Postgres HA, Vault, observability, off-site backups, the works.
- **Scale (when load demands):** add TigerBeetle (6 replicas), more workers/read-replicas, partitioning; chaos game-days become routine.

---

## 13. Observability & "every stat and data possible"

Your "every stat possible" goal maps cleanly to three pillars + analytics:

- **Metrics (Prometheus):** request rate/latency/error per endpoint; transfers/sec; balance-reconciliation status; queue depths; DB pool saturation; per-currency volume. Dashboards in Grafana with SLO alerting.
- **Tracing (OpenTelemetry/Tempo):** distributed trace of every request across services — find *where* the 50 ms went.
- **Structured logs (Loki):** JSON logs, correlation IDs, no PII in logs.
- **Business analytics:** a read-replica / separate analytics store feeds reporting (volumes, active users, fraud rates, settlement reports for the regulator). Kept off the transactional primary so analytics never slows payments.
- **Continuous reconciliation:** a background job constantly re-derives balances from `entries` and asserts they equal the materialized `balances`, and that all entries sum to zero. Any drift pages an engineer immediately. **This is your early-warning system for correctness bugs.**

---

## 14. Testing & verification strategy (how we earn trust)

Payments demand more than "I ran it and it worked":

- **Unit tests:** pure domain logic (the `Money` type, entry-balancing, fee math).
- **Property-based tests (`proptest`):** generate thousands of random transaction sequences and assert invariants always hold — "no balance goes negative," "entries always sum to zero," "total money is conserved."
- **Deterministic simulation testing (the gold standard, à la TigerBeetle/FoundationDB):** run the ledger engine against a simulated world with injected faults — random crashes, network partitions, message reordering, concurrent conflicting transfers — replayable from a seed. If invariants survive millions of simulated fault scenarios, you have real confidence. This is how the best financial systems are tested.
- **Integration tests:** real Postgres (via testcontainers), full request→ledger→balance path, including retries and idempotency.
- **Load tests:** establish the real p99 latency and max throughput; catch regressions in CI.
- **Fuzzing:** on parsers and the API boundary.

---

## 15. Build vs. buy: the ledger engine, and TigerBeetle

You said "extremely fast." The fastest correct path is worth naming explicitly:

- **TigerBeetle** is a purpose-built, open-source financial accounting database (double-entry, debits/credits as first-class) doing ~1M+ transfers/sec with built-in replication and deterministic testing. It does *exactly* the hot-path accounting we need, and nothing else.
- **Trade-off:** it's a second datastore to operate, and it only does accounting — all the other data (users, KYC, metadata) still lives in Postgres.

**Decision (updated after review):** We **commit to TigerBeetle as the destination** for the hot accounting path — it *is* faster (1M+ TPS), its VSR-replicated cluster directly serves the "unkillable" goal, and it is deterministic-simulation-tested to a degree almost nothing else in this space is. It is philosophically the right home for a system whose whole reason to exist is speed + robustness.

But we still build behind a `LedgerEngine` trait, and we still start on Postgres, for one blunt reason: **the launch bottleneck is never the ledger's TPS — it's auth, KYC, network round-trips, and ops maturity.** TigerBeetle only does accounting; it does *not* store users, KYC, metadata, idempotency records, or the outbox — all of that lives in Postgres regardless. Running TigerBeetle's consensus cluster *and* Postgres *and* Redis *and* NATS on day one, before a single real user, is a lot of moving parts to operate.

So:
1. **Phase 1: `PostgresLedger`** — double-entry, row-locked, in the Postgres we already need. One fewer datastore to operate while we prove correctness and ship the product. Comfortably handles thousands of TPS — far beyond early Tajik volume.
2. **Phase 2: `TigerBeetleLedger`** — swap the hot accounting path to a TigerBeetle cluster once real load (or load tests) justify the extra operational surface. Postgres stays for everything non-accounting.

Because all business logic talks to the **trait**, this swap is a *configuration/wiring change, not a rewrite*. We get "ship correct soon" **and** "extremely fast at scale" without betting the launch on operating two consensus systems before we have users. The trait is designed from day one with TigerBeetle's model in mind (integer amounts, debit/credit transfers, pending/posted two-phase transfers) so the Postgres implementation doesn't paint us into a corner.

---

## 16. Proposed project structure (refined)

Mostly keeping the first draft's clean layering, with additions for the new subsystems:

```
payment-backend/
├── Cargo.toml                # workspace
├── crates/
│   ├── money/                # Money type, currency, checked arithmetic (pure, no deps on db/http)
│   ├── ledger/               # LedgerEngine trait + PostgresLedger; double-entry core (pure-ish, simulation-tested)
│   ├── domain/               # accounts, transactions, business rules (pure)
│   ├── api/                  # axum handlers, routing, middleware, DTOs
│   ├── services/             # orchestration: payments, auth, kyc, aml, fx, notifications
│   ├── repository/           # sqlx queries
│   ├── workers/              # outbox relay, sealer (Merkle/checkpoints), reconciliation, screening
│   ├── crypto/               # signing, hashing, Merkle, checkpoint chain
│   ├── config/               # typed config loader
│   └── observability/        # tracing/metrics setup
├── migrations/               # sqlx migrations
├── tests/                    # integration + simulation harness
└── deploy/                   # docker, k8s/nomad, CI
```

⚠️ **Change from initial draft:** a **Cargo workspace of crates** instead of one crate with modules. Why: the `money` and `ledger` cores can then have *zero* dependency on HTTP/DB, which (a) keeps them pure and fast to compile/test, (b) makes simulation testing clean, and (c) enforces the layering at the compiler level — `domain` literally *cannot* import `api`.

---

## 17. Phased roadmap (so we ship, not just plan forever)

> **Build status (live):**
> - ✅ Workspace skeleton created (`Cargo.toml`).
> - ✅ `money` crate — integer minor units, currency-safe, checked arithmetic. 9 tests + 2 property tests, clippy-clean.
> - ✅ `ledger` crate — double-entry core: `Account`/`Entry`/`Transaction`, `LedgerEngine` trait, `InMemoryLedger` reference engine. Enforces balanced-transaction, no-overdraft, atomic+idempotent posting, and money conservation. 6 tests + 1 simulation property test, clippy-clean.
> - ✅ `storage` crate — `PostgresLedger` (async `LedgerStore` trait) on real PostgreSQL: row-locked, deadlock-ordered, atomic, idempotent. Reuses the pure `ledger` validation. 2 integration tests **incl. a 200-way concurrent double-spend test** — pass against Postgres.
> - ✅ `auth` crate — Argon2id password hashing, JWT access tokens, opaque+hashed revocable refresh tokens. 6 unit tests, clippy-clean.
> - ✅ `api` crate — axum HTTP server (`payment-server`). Auth (`register`/`login`/`refresh`/`logout`), `POST /v1/wallets`, `GET /v1/accounts/{id}/balance`, **KYC** (`GET /v1/kyc`, `POST /v1/kyc/submissions`, admin `approve`/`reject`), admin **blocklist** (`POST`/`DELETE /v1/admin/blocks`), `POST /v1/deposits` (**admin-only**), `POST /v1/transfers` (**KYC-gated + AML-screened**), `GET /health`. Bearer-token auth + ownership enforcement, **admin role** (`AdminUser`, DB-checked), **KYC** (submission/review workflow, transfers require level ≥ 1), **AML screening** (blocklist + per-tx/rolling-24h/hourly-velocity limits tiered by KYC level, all decisions logged to `screening_events`), **rate limiting** (fixed-window per `X-Forwarded-For` — in-memory *or* Redis-backed, 429), per-endpoint **idempotency**, typed error→HTTP mapping, JSON logs. 10 HTTP integration tests.
> - ✅ `crypto` crate — SHA-256 Merkle tree with inclusion proofs + Ed25519 signing. 9 unit tests.
> - ✅ `workers` crate — checkpoint **sealer** (batched Merkle + signed chain), independent **chain verifier**, **reconciliation** (conservation + balance integrity), and the **outbox relay** (pluggable `EventPublisher` — `LoggingPublisher` *and* `NatsPublisher`; at-least-once, `FOR UPDATE SKIP LOCKED`). Binary `payment-workers` loops relay+seal+reconcile, publishing to NATS when `NATS_URL` is set. Integration tests prove seal→verify→**tamper-detected**→restore, post→outbox→relay-once-and-marked-sent, and **NATS publish→subscriber receives**.
> - ✅ **Transactional outbox** (§5.4) — `PostgresLedger::post` writes a `transaction.posted` event in the *same* DB transaction as the entries (no dual write). Published by the relay; transport behind a trait (logging now, NATS later).
> - ✅ Dev environment — `docker-compose.yml` (Postgres 16 + NATS + Redis) + `migrations/` (sqlx) + `README.md`. ✅ CI workflow (`.github/workflows/ci.yml`): fmt, clippy `-D warnings`, `cargo deny`, DB-less + Postgres integration tests.
> - ✅ **Transfer fees** (§4.2) — configurable basis-points fee (`FeeConfig`, env `TRANSFER_FEE_BPS`, default 0) credited to a seeded fee-revenue account via the canonical three-entry double-entry transfer. Integration-tested.
> - ✅ **FX / multi-currency** (§4.2) — `POST /v1/fx` converts between a caller's own wallets via a single transaction with one balanced leg per currency; admin-set integer-ratio rates (`fx_rates`, `POST /v1/admin/fx-rates`); USD added; per-currency FX-position accounts; floored conversion never creates money. Integration-tested (TJS→USD, both legs balance).
> - ✅ **Security audit** — `cargo audit` / `cargo deny`: 4 fixable advisories (rustls-webpki via async-nats) **fixed** by upgrading async-nats 0.38→0.49; 1 unfixable transitive (`rsa`, Marvin) formally accepted in `deny.toml` with rationale (reachable only via the unused MySQL macro path; Postgres uses SCRAM). **Zero outstanding vulnerabilities.**
> - ✅ **Performance** (release, single dev box) — `/health`: **~372k req/s**, p99 **0.89ms**; authenticated DB-backed balance read (JWT verify + ownership + balance query): **~23.7k req/s**, avg 2.1ms; both 100% success. Ledger correctness holds under 200-way concurrency (storage test). Rate-limit budget tunable via `RATE_LIMIT_MAX`/`RATE_LIMIT_WINDOW_SECS`.
> - **Backend feature set is functionally complete:** double-entry ledger, auth, KYC, AML, fees, FX, idempotency, tamper-evidence, outbox→NATS, reconciliation, rate limiting. **48 tests** (30 unit/property/simulation + 18 DB/infra integration), all green; clippy + fmt clean; release build clean.
> - ⏭️ Next: production deployment (§12 — Talos/k8s, Patroni Postgres HA, Vault, observability), NATS JetStream durability + a consumer, public-anchoring option (§6.4).

**Phase 0 — Foundations (this is where coding starts, after we agree on this doc):**
- ✅ Workspace skeleton. ⬜ config, Postgres connection, migrations, observability baseline, CI with `cargo audit`/`deny`.

**Phase 1 — Correct ledger core:**
- ✅ `money` crate + ✅ `ledger` crate pure core (double-entry, idempotency, in-memory engine).
- ⬜ `PostgresLedger` (row-locking, real persistence). ✅ Property tests + first simulation test. **Goal: provably correct transfers.**

**Phase 2 — API & auth:**
- Register/login/refresh, accounts, transfers (idempotent), balances. mTLS scaffolding.

**Phase 3 — Tamper-evidence & events:**
- Outbox relay, sealer (Merkle checkpoints + Ed25519 chain), audit log.

**Phase 4 — Money in/out:**
- Partner-bank deposit/withdrawal integration, FX accounts, fees.

**Phase 5 — Compliance & ops hardening:**
- KYC/AML subsystems, reconciliation job, DR drills, load tests, chaos tests.

**Phase 6 — Scale (only if needed):**
- Evaluate `TigerBeetleLedger`, read replicas, partitioning, multi-region.

Each phase ends with: tests green, invariants holding under simulation, dashboards live.

---

## 18. Open questions for our review session

Status legend: ✅ decided · ⬜ still open.

1. ✅ **Product type:** **Wallet / e-money** — users hold balances with us. (§1.3)
2. ⬜ **Cards:** are we ever touching card data, or routing through a tokenizing processor? (Strongly prefer the latter.) (§1.3)
3. ✅ **Currencies:** **TJS only at launch, but built multi-currency-ready** — the `currencies` table + `Money{amount, currency}` type mean adding a currency is data, not a schema change. No FX engine in v1; the account/entry model already supports it when we add it. (§3/§4)
4. ✅ **Deployment target:** **in-country, company-owned datacenter (100+ machines), solo operator.** Full architecture in §12.1–§12.10: Talos+Kubernetes for stateless tier (GitOps via Argo CD), Debian bare-metal for Postgres HA (Patroni), Vault for secrets, distroless Rust images. (§12)
5. ⬜ **Public anchoring:** public-chain checkpoint anchoring in v1, or later add-on? (§6.4)
6. ⬜ **Throughput target:** realistic peak TPS for year 1? (§15)
7. ✅ **Team:** **solo.** This is the binding constraint — drove the "automate everything, immutable, self-healing" platform design in §12.2.
8. ⬜ **Existing rails:** does NBT provide a national switch / instant-payment system we must interoperate with? Hard external constraint.

---

## 19. Summary of what I changed from the initial draft (so you can challenge me)

| # | Initial draft | This document | Why |
|---|---|---|---|
| 1 | `rust_decimal` for money | **Integer minor units**, `Money` type, checked arithmetic | Floats/decimals invite rounding bugs; integers are the financial standard. |
| 2 | Single-entry `{from,to,amount}` | **Double-entry** debits/credits summing to zero | Self-auditing, handles fees/FX/reversals, industry standard. |
| 3 | (not addressed) | **Concurrency model**: row-locking, ordered, serializable-enough | Prevents double-spend — the core failure mode. |
| 4 | Global per-transaction hash chain | **Async batched Merkle checkpoints** + signed chain | The global chain serializes all writes — a throughput killer. |
| 5 | DB write + NATS publish (dual write) | **Transactional outbox** | Eliminates lost/phantom events. |
| 6 | JWT auth | JWT access + **revocable opaque refresh tokens** | Pure JWT can't be revoked — unacceptable for banking. |
| 7 | age/Vault for keys | **HSM/KMS** for signing keys | Financial-grade key custody. |
| 8 | axum *or* actix | **axum**, standardized | One framework, less divergence. |
| 9 | Single crate, modules | **Cargo workspace**, pure `money`/`ledger` cores | Compiler-enforced layering + clean simulation testing. |
| 10 | (not addressed) | **Compliance/KYC/AML/licensing** as first-class | Legally required; cheap to design in, expensive to retrofit. |
| 11 | (not addressed) | **Deterministic simulation testing** + continuous reconciliation | How correctness is actually proven and monitored. |
| 12 | "blockchain as option" | **Optional public anchoring** of checkpoint roots | Keeps the genuinely useful 5% of blockchain, drops the costly 95%. |

---

*End of draft. Let's review §18 together, settle the open questions, and only then start Phase 0.*
