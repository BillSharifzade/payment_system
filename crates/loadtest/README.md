# payment-loadtest

Drives the money path under load and then proves the ledger is intact. Two modes:

- **`http`** — end to end through a running `payment-server` (or one it spawns with
  `--server-bin`): registration, funding, then transfers; optionally FX, merchant checks
  (create + pay) and device-signed requests (`--sign`: a P-256 key per user, every money
  request signed, so the server's ECDSA verification is in the measurement).
- **`direct`** — in process: the same `Transaction` (`api::payment_entries`) and the same AML
  guard (`api::aml_guard`) as `POST /v1/transfers`, posted with `PostgresLedger::post_on`. The
  difference between the two modes is HTTP, JSON, JWT, the handler's context query and the
  screening pre-checks.

Workloads (`--workload`): `uniform` (random distinct pairs), `hot` (payers pay `--merchants`
Zipf-ranked recipients, `--zipf` exponent), `contention` (everyone is paid by one payer — every
post serialises on one wallet and one AML window), `mixed` (uniform transfers plus balance and
statement reads). `--mix transfer=2,fx=1,check=1,balance=…,statement=…` sets any op mix.

## Running it

```bash
cargo build --release -p api -p loadtest
DB=postgres://payment:payment_dev_pw@localhost:5432/payment_perf

# End to end; the harness spawns the server with the bench knobs (see below).
target/release/payment-loadtest --database-url $DB --server-bin target/release/payment-server \
  --workload hot --concurrency 64 --duration 30s --pg-stats --report hot.json

# Against a server you run yourself (it must be in the dev mode below).
target/release/payment-loadtest --database-url $DB --base-url http://127.0.0.1:8080 --workload mixed

# In process.
target/release/payment-loadtest --database-url $DB --mode direct --workload contention --concurrency 16

# Open loop: 600 transfers/s offered, latency counted from the intended start.
target/release/payment-loadtest ... --rate 600

# Medians and spread over reports, grouped by --label.
target/release/payment-loadtest summarize runs/*.json
```

`matrix.sh` runs interleaved A/B repeats — each run on a freshly recreated database, the
variant order flipped every repeat — and prints the median table:

```bash
ADMIN_URL=postgres://postgres@127.0.0.1:5442/postgres \
DB_URL=postgres://payment:payment_dev_pw@127.0.0.1:5442/payment_perf REPEATS=3 OUT=/tmp/bench \
  crates/loadtest/matrix.sh base=/bin/a/payment-server:/bin/a/payment-loadtest \
                            new=/bin/b/payment-server:/bin/b/payment-loadtest
```

Knobs: `--concurrency`, `--duration`, `--warmup`, `--rate`, `--users`, `--pool-size` (direct
workers, or the spawned server's `DB_MAX_CONNECTIONS`), `--fee-bps` (direct, or the spawned
server's `TRANSFER_FEE_BPS`; 0 = fees off), `--amount LO..HI`, `--fund`, `--retries`,
`--server-env K=V`, `--redis-url`. `--help` lists them all; flags are parsed strictly (an
unknown flag or a bad value refuses to run).

### Dev mode for benchmarks

`--server-bin` starts `payment-server` with `APP_ENV=dev`, `DEPOSIT_DUAL_CONTROL=false`
(one admin funds the users; with dual control on, the harness approves every deposit with a
second admin instead), `DEVICE_BINDING=optional` (signed and unsigned runs share a server; a
signature that is sent is still verified), the per-IP rate limit and the AML caps lifted to
1e15 (the guard still sums its window on every post — that cost is real, the rejections would
not be) and `RUST_LOG=info` (the access log is part of the production cost). Setup promotes the
two admins and sets users to KYC level 2 by SQL, as an operator would. Never run a production
server in this mode.

## What every run checks

After the measured window the harness verifies, against the database, and exits non-zero on
any failure:

- **conservation** — every currency's balances sum to zero (whole database);
- **balances = entries** — every account's materialised balance equals the signed sum of its
  entries (whole database);
- **no lost or altered acknowledged transfer** — every request answered 201 (or replayed as
  posted) exists with exactly the acknowledged legs on the run's wallets;
- **nothing refused yet posted** — no 4xx'd (or rolled-back) id has entries;
- **nothing unexplained** — every transaction touching a run wallet is a funding deposit, an
  acknowledged post, or an unresolved one (5xx/transport after every retry — retries reuse the
  Idempotency-Key, as a client must) that the ledger shows posted; and every wallet's balance
  equals its funding plus the legs the client knows of.

Under `LEDGER_BACKEND=tigerbeetle` (build with `--features tigerbeetle`; the harness passes
`LEDGER_BACKEND` and `TIGERBEETLE_*` on to the server it spawns) `balances` is not written:
conservation sums the journal, and *balances = entries* becomes *cluster = journal* for every
account after a quiesced recovery pass; wallet balances are read from the cluster. `direct`
mode posts through `PostgresLedger` and refuses that setting.

Any failed request also fails the run unless `--allow-errors` is given (invariants are always
enforced); `--p99-ceiling-ms` adds a latency ceiling.

## Output

A table on stderr and, with `--report`, JSON: throughput and latency percentiles per op from
HdrHistogram (p50/p90/p99/p99.9/max, successful requests only), an error breakdown by op, HTTP
status and error code, retries, a per-`--interval` timeline (ops/s, p50, p99 — a stall or a
slowly degrading guard shows up there), **CPU milliseconds per operation** for the harness, the
spawned server and the Postgres processes (from `/proc`; Postgres only when it runs on the same
host), the server settings that decide commit behaviour, and with `--pg-stats` the
pg_stat_statements diff over the measured window (calls, mean, plans, buffers, WAL per
statement; needs the extension) plus WAL and database counters.

On a shared box, compare **medians of interleaved repeats** and lean on CPU/op: throughput moves
with whatever else runs, the work one operation costs much less so. In a closed loop at
saturation latency is mostly queueing (Little's law: concurrency / throughput); an open-loop run
(`--rate`) below capacity shows the service latency.

## Results (perf pass, October 2026)

**Box.** One Firecracker VM: 4 vCPU Intel Xeon @ 2.10 GHz, 15 GB RAM, kernel 6.18, shared with
four other agents building and testing at the same time. Harness, server and Postgres on the
same 4 cores, so every number below is a *relative* measurement: interleaved repeats on fresh
databases, medians with min–max. Throughput moved by ±15% between sessions on the same binary;
compare rows of one table, not across tables. Postgres 16.15 private instance with the
production flags of `deploy/docker-compose.prod.yml` (`shared_buffers=384MB`,
`wal_compression=on`, `synchronous_commit=on`, …) plus `pg_stat_statements`,
`track_io_timing`, `log_lock_waits` and `deadlock_timeout=100ms` (to log shorter waits); data
checksums on, fsync of the WAL ≈ 0.2 ms. No synchronous standby: on a replicated primary every
commit also waits a network round trip.

Common flags: `--users 1000 --concurrency 64 --duration 30s --warmup 5s --pg-stats`
(contention: `--concurrency 16`), transfer fee 30 bps (the harness default: three entries and a
fee shard per transfer), pool 32. `ok/s` is acknowledged operations per second; latencies are
of successful requests; CPU is milliseconds per operation from `/proc`. Every run of every
table passed all post-run checks with zero failed requests.

### Base (80945b0) vs this branch

| Case | ok/s base → this branch | p50 ms | p99 ms | Postgres CPU ms/op | server CPU ms/op |
|---|---|---|---|---|---|
| http-uniform | 1387 (1316–1591) → 1714 (1481–1784) (**+24%**) | 45.5 → 36.5 | 71.4 → 65.0 | 1.818 → 1.603 | 0.452 → 0.392 |
| http-open600 | 600 → 600 (offered) | 5.1 → 4.3 | 16.6 → 12.7 | 2.019 → 1.953 | 0.691 → 0.627 |
| http-hot | 759 (754–801) → 1115 (775–1168) (**+47%**) | 48.1 → 36.5 | 516.6 → 292.9 | 2.425 → 1.773 | 0.674 → 0.525 |
| http-contention | 196 (181–217) → 377 (244–404) (**+92%**) | 74.9 → 41.0 | 130.0 → 65.5 | 4.871 → 2.449 | 0.891 → 0.734 |
| http-mixed | 2026 (1909–2257) → 2171 (1863–2297) (+7%) | 31.2 → 25.6 | 61.8 → 67.2 | 1.229 → 1.303 | 0.337 → 0.288 |
| http-signed | 1315 (1118–1325) → 1421 (1213–1460) (+8%) | 48.1 → 44.3 | 76.9 → 72.6 | 1.882 → 1.722 | 0.767 → 0.702 |
| http-fx-check | 1525 (1433–1603) → 1789 (1767–1895) (**+17%**) | 41.2 → 35.1 | 71.9 → 58.5 | 1.754 → 1.399 | 0.465 → 0.404 |
| direct-uniform | 1824 (1624–1850) → 2257 (2002–2455) (**+24%**) | 34.3 → 27.6 | 54.0 → 46.6 | 1.738 → 1.365 | – |
| direct-hot | 815 (736–916) → 1304 (1023–1332) (**+60%**) | 47.4 → 29.6 | 420.6 → 261.8 | 2.239 → 1.631 | – |
| direct-contention | 200 (180–208) → 390 (377–395) (**+95%**) | 78.1 → 40.2 | 143.7 → 62.8 | 4.913 → 2.269 | – |

Cases (flags on top of the common ones): `--workload uniform`; the same open loop at
`--rate 600` (below both builds' capacity, so its latency is service time, not queueing);
`--workload hot --merchants 10`; `--workload contention --concurrency 16` (one payer);
`--workload mixed`; `--sign`; `--mix transfer=2,fx=1,check=1`; and the three `--mode direct`
ones. 60 runs, all verified, zero failed requests. Medians with min–max in brackets for
throughput; latency and CPU columns are medians (the full spread is in `summarize`'s table).

Read it with the noise in mind. The contention and hot gains are far outside the spread (the
O(1) AML window; guards before wallet locks and 64 shards). Uniform, FX/check and direct gains
are clear, and Postgres CPU per operation is down in every write-heavy case (8–27%; half in the
contention cases). The signed and mixed rows are within the spread: in the mixed run (half
reads) transfers went 1005 → 1086/s and the statement page p50 31.1 → 20.9 ms (one round trip
less), balance reads did not change (16.8 → 18.6 ms, same code), and the mix's p99 and CPU per
op are no better. The open-loop p99 swings by an order of magnitude between repeats of one
build (7–193 ms): stalls of the shared box, not the code.

### What was slow (profile of the base)

- **Re-planning.** Statements with array parameters (`= ANY($1)`) were planned anew on every
  execution (the plan cache estimates 10 elements and never settles on a generic plan): 44 309
  plans for 251 202 statement executions in one 20 s run, about a quarter of Postgres CPU per
  post.
- **No HOT updates.** `idx_balances_updated` made every balance update non-HOT: 0 HOT of
  160 000, two index insertions and a dead tuple per update, frequent vacuums, and every vacuum
  invalidated the cached plans of all backends.
- **Hot wallets held across the guard.** Wallets were locked before the AML guard ran, so a
  merchant everyone pays stayed locked for each payer's guard: lock waits, not CPU, bounded the
  hot workload.
- **Shard collisions.** With 16 shards per system account, the fee-shard update took 2–4 ms on
  average at 32 posts in flight, nearly all of it waiting for another post's shard lock.
- **O(n) AML guard.** The daily limit summed every debit of the day: a payer with thousands of
  posts a day paid thousands of index tuples per post, while holding their user row.
- Reads: the statement page checked ownership in a separate round trip.

### One change at a time

Each row is an interleaved session (3 repeats, medians, min–max), the change against the
build just before it. Single steps under ~10% are within this box's noise; the base-vs-final
table above is the one to quote.

| Change | Evidence |
|---|---|
| Generic plans in `post_on` (`SET LOCAL plan_cache_mode`, sent with `BEGIN`) | plans per statement execution 0.18 → 0; direct-uniform 982 (960–1069) → 1264 (1108–1357) ok/s, Postgres 1.83 → 1.47 ms/op. Over HTTP no gain on its own (949 → 969 ok/s): on the bloated, non-HOT balance indexes the generic plans read 640 buffers per transfer instead of 228 — fixed by the next row |
| 0029: drop `idx_balances_updated` (HOT) | http-uniform 969 (623–1059) → 1137 (1065–1152) ok/s, Postgres 2.09 → 1.71 ms/op, 640 → 234 buffers per transfer |
| System-account cache + immutable accounts trigger | one statement less per post; Postgres 1.71 → 1.60 ms/op over HTTP, throughput within noise |
| Guards before wallet locks | http-hot 364 (358–395) → 477 (371–495) ok/s, p99 1100 → 730 ms; direct-contention p99 581 → 211 ms (throughput 165 → 146, within noise) |
| 64 shards per system account (0029) | mean fee-shard UPDATE (pg_stat_statements, mostly lock wait) 1.4–4.1 ms → 0.1–0.3 ms; direct-uniform 1122 → 1219 ok/s |
| Highest system delta inside the write statement | open-loop 600/s p50 10.5 → 6.4 ms, p99 34 → 35 ms; direct-uniform 1386 → 1417 ok/s |
| 0030: O(1) AML windows, final form (aggregating LATERAL probes, no FKs) | against the full sum: direct-contention (one busy payer, 60 s) 180 (177–183) → 425 (400–432) ok/s, p99 135 → 52 ms, Postgres 5.16 → 2.14 ms/op |
| … stored only from 64 daily debits; lighter users keep the full sum | uniform cost against the full sum: HTTP 1761 (1346–1830) → 1681 (1444–1711) ok/s, open-loop p50 4.1 → 4.4 ms (within noise); direct (database only) 2466 → 2053 ok/s, Postgres 1.31 → 1.48 ms/op — the statement trigger on `entries` and the window probe, paid by every post |
| `commit_durable` as an extra statement before COMMIT (HA fix) | http-uniform 1473 → 1343 ok/s (−9%), direct-uniform 1761 → 1475 (−16%); hence `post_on` lifts `statement_timeout` inside its write statement instead, which costs nothing measurable |
| Statement reads check ownership inside the page query | one round trip less per statement page |

The first cut of the AML windows (plain probes, foreign keys to `users` and `currencies`) cost
15% on the uniform workload: generic plans built on tiny, skewed statistics probed the window
with a join filter instead of an index condition, and updating a row inserted in the same
transaction took a KEY SHARE lock on the one `currencies` row every post shares. Aggregating
LATERAL subqueries (an aggregate cannot be pulled up into the join) and dropping those foreign
keys fixed both.

### Tried and not kept

| Idea | Result |
|---|---|
| Lock credit-only wallets late (after the debited ones) | deadlocks: A pays B while B pays A. `crates/api/tests/lock_order.rs` (crossing pairs, a 3-cycle, a merchant paying out, checks, fees) detects such a design within seconds |
| `commit_delay` 100 µs / 1000 µs | http-uniform 1380 → 1399 / 1396 ok/s, direct-uniform 1991 → 2021 / 2015: noise |
| Batching commits (group commit in the server) | bounded first with `synchronous_commit=off` as a diagnostic (never a setting): only +3.6% (HTTP) / +5.8% (direct) at this concurrency — Postgres already groups WAL flushes; not worth the extra failure modes and proofs |
| mimalloc in the server | server CPU 0.382 → 0.339 ms/op, throughput 1671 → 1704 ok/s (within noise), one signed run with stalls: not worth a new allocator |
| Pool size (`DB_MAX_CONNECTIONS`) | HTTP 8 / 16 / 32 / 64: 1013 / 1235 / 1620 / 1431 ok/s; direct 1142 / 1938 / 2004 / 2020. 32 (the default) stays right for 4 cores |

### Re-check on the final tree, quiet box (0027c64, October 2026)

After the HA, anchoring, fuzzing and TigerBeetle work landed on top of the perf pass, the
three cases that moved most were re-run with nothing else on the box: same VM, harness, server
and Postgres alone on the 4 cores, a private Postgres 16 with the production flags above (no
data checksums, default lock-wait logging). `base` is the 80945b0 server, `final` the 0027c64
server (default build, `LEDGER_BACKEND=postgres`), one harness binary for both. Interleaved,
order flipped per repeat, fresh database per run, median (min–max) of 3; 18 runs, all verified,
zero failed requests:

| Case | ok/s base → final | p50 ms | p99 ms | p99.9 ms | Postgres CPU ms/op | server CPU ms/op |
|---|---|---|---|---|---|---|
| http-uniform | 1547 (1503–1608) → 1871 (1866–1978) (**+21%**) | 40.9 → 33.9 | 65.9 → 50.4 | 170.8 → 196.1 | 1.888 → 1.534 | 0.445 → 0.388 |
| http-hot | 829 (824–840) → 1222 (1219–1241) (**+47%**) | 45.7 → 32.9 | 469.5 → 267.0 | 766.5 → 431.6 | 2.366 → 1.719 | 0.642 → 0.510 |
| http-contention | 204 (204–229) → 454 (428–454) (**+123%**) | 76.6 → 34.6 | 114.5 → 44.5 | 130.1 → 89.3 | 5.215 → 2.105 | 0.814 → 0.636 |

The gains survived the later work, and the throughput ranges no longer overlap in any row. The quiet box tightened the spread (uniform base 1503–1608 against 1316–1591 above), and
single-payer contention gained more than on the shared box (+123% against +92%). One number got
worse: the uniform p99.9 (170.8 → 196.1 ms) at 21% more throughput, so at saturation the rare
slow commit now queues behind more work. It is the next thing to look at, not a win to claim.

### Postgres vs TigerBeetle (`LEDGER_BACKEND`, October 2026)

The same release binaries (`--features api/tigerbeetle,loadtest/tigerbeetle`), HTTP, 1 000
users, 64 clients, 20 s after 5 s warm-up; every run on a fresh database and, for TigerBeetle, a
freshly formatted single-replica 0.17.9 cluster (`--development`, same box); variants
interleaved, order flipped per repeat; median (min–max) of 3, every run verified:

| case | backend | ok/s | p50 ms | p99 ms | Postgres CPU ms/op | server CPU ms/op |
|---|---|---|---|---|---|---|
| uniform | postgres | 1759 (1704–1774) | 36.5 | 54.2 | 1.605 | 0.427 |
| uniform | tigerbeetle | 1452 (1437–1523) | 42.4 | 95.7 | 1.298 | 0.474 |
| uniform | tigerbeetle, 4 sessions | 1214 (1199–1231) | 49.0 | 131.2 | 1.374 | 0.573 |
| hot, 1 merchant | postgres | 564 (564–597) | 79.8 | 392.7 | 2.712 | 0.704 |
| hot, 1 merchant | tigerbeetle | **1500** (1492–1603) | 40.7 | **98.0** | 1.318 | 0.477 |
| hot, 1 merchant | tigerbeetle, 4 sessions | 1291 (1228–1329) | 46.3 | 134.0 | 1.355 | 0.560 |

Spread-out traffic pays two cluster round trips per post (reserve, then post) for less Postgres
work; a hot recipient no longer queues on its balance row (2.7× the throughput, a quarter of
the p99). More client sessions only added CPU on this 4-core box. To reproduce, export
`LEDGER_BACKEND=tigerbeetle TIGERBEETLE_CLUSTER_ID=… TIGERBEETLE_ADDRESSES=…` for the harness
itself (it verifies through the same backend and forwards the settings to the server it spawns)
and reformat the replica before every run: a cluster belongs to one database.

### Reproduce

```bash
cargo build --release -p api -p loadtest   # once per variant, copy the two binaries aside
ADMIN_URL=postgres://postgres@127.0.0.1:5442/postgres \
DB_URL=postgres://payment:payment_dev_pw@127.0.0.1:5442/payment_perf REPEATS=3 OUT=/tmp/bench \
CASES="$(printf '%s\n' 'http-open600|--workload uniform --rate 600')" \
  crates/loadtest/matrix.sh final=/bin/b/payment-server:/bin/b/payment-loadtest \
                            base=/bin/a/payment-server:/bin/a/payment-loadtest
```

Leave `CASES` unset for the full table above minus the open-loop row.
