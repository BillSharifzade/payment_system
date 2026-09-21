# Frontend design & architecture

Companion to `DESIGN.md` (the backend source of truth). This document plans the
two client surfaces, the client-side architecture rules a money app must obey,
and the small set of backend endpoints the frontends need but the API does not
yet expose.

---

## 1. What we are building (and in what order)

| # | Surface | For | Stack (recommended) | Why this order |
|---|---------|-----|---------------------|----------------|
| 1 | **Admin/ops console** | you + future compliance staff | React + Vite + TypeScript SPA, served by Caddy | Unblocks operations (KYC review, blocklist, FX rates, funding) with zero app-store friction; smallest possible scope; exercises the API contract before the customer app locks it in |
| 2 | **Customer wallet app** | end users in Tajikistan | **Native Kotlin + Jetpack Compose, Android-only** (iOS deferred; Swift or KMP when traction demands it) | The actual product |
| 3 | QR payments, push notifications, iOS release | growth | (extends #2) | Needs #2 shipped + real merchant feedback |

### Why native Kotlin (Jetpack Compose), Android-only — and why NOT Swift yet

Decision 2026-07-03: smoothness, robustness and speed are the stated product
priorities, and the market makes the trade-off clean.

- **Market reality:** Tajikistan is overwhelmingly Android, heavy on low-end
  devices. Native Kotlin is the best runtime and tooling for exactly that, and
  the platform security surfaces a money app leans on (Keystore/StrongBox,
  BiometricPrompt, `FLAG_SECURE`, Play Integrity) are first-class instead of
  wrapped.
- **iOS is deferred, not chosen-and-parked:** building Kotlin AND Swift
  simultaneously as one person means every feature, fix, and security audit
  twice — that is where robustness actually dies. When iOS demand is real,
  either write the Swift app then, or move shared business logic (Money,
  PaymentSubmitter, API client) into **Kotlin Multiplatform** and keep both UIs
  native. Nothing in this plan blocks either path.
- **Flutter/React Native rejected** given the priorities: both are fine tools,
  but they trade a layer of indirection for cross-platform reach we do not
  currently need, and the reach they buy (iOS) is exactly what we are
  deferring.
- **PWA rejected as the primary surface:** a money app lives and dies on trust
  and OS integration (secure storage, biometrics, camera for QR/KYC docs,
  push).

### Why React+Vite for the admin console

Boring and fast to build: the console is forms + tables + a review queue.
TanStack Query for server state, TanStack Table for lists, react-hook-form +
zod for forms. No SSR — it is an internal tool behind a login; ship it as
static files from the existing Caddy.

---

## 2. The client-side rules that make a wallet app correct

These four patterns are the frontend counterpart of the ledger's invariants.
They are not optional styling choices; codify them in the app's core layer.

### 2.1 Money is integers, end to end

The API speaks minor units (`amount_minor`, diram). Clients keep money as
`Long` all the way to the formatting boundary (64-bit — fine).
One `Money` type in the app: `{ minorUnits: int, currency: String }`, with a
single formatter (`65,00 сомонӣ` / `65.00 TJS` per locale). **No `double`
ever touches an amount.** Parsing user input goes text → minor units directly.

### 2.2 The send-money state machine (idempotency on the client)

The backend replays by `Idempotency-Key`; the client must hold up its half:

```
draft ──(user taps Send)──> submitting(key=UUID, persisted) ──> posted | rejected
                                   │  network error / timeout / app killed
                                   └──> retry SAME key (bounded backoff)
```

- Generate the UUID key **when the user confirms**, persist it in local storage
  *before* the first request, and reuse it for every retry of that payment.
- A retry answered `201/200` = the stored outcome — safe to show "sent" even if
  the first response was lost mid-flight. `409 idempotency_conflict` = client
  bug; surface loudly in dev.
- Never auto-retry with a *fresh* key. That is how double-sends happen.
- On app restart, any persisted `submitting` payment is resolved by retrying its
  key — the answer is authoritative because the backend replays before
  screening.

### 2.3 Token lifecycle

- Access token (15 min) in memory; refresh token in **Keystore-backed secure
  storage** (EncryptedSharedPreferences) — never in plain prefs / localStorage.
  Admin console: refresh token in memory + re-login is acceptable (ops tool).
- One shared HTTP interceptor: on 401 → single-flight refresh → replay the
  original request. Serialize refreshes; parallel refreshes will trip rotation.
- If refresh answers 401 (rotation family revoked — possibly theft): wipe local
  session, force password login, tell the user why ("you were signed out for
  security").
- App-lock layer (PIN + biometric) gates the UI independently of tokens.

### 2.4 Errors are the API's machine codes, not its messages

The API guarantees stable `error.code` values (`insufficient_funds`,
`limit_exceeded`, `kyc_required`, `account_blocked`, `rate_limited`, …). The
client maps **codes** to localized, human explanations and actions (e.g.
`kyc_required` → route to the KYC flow). Server `message` strings are for logs,
never for screens — they are English and unstable.

---

## 3. Customer app: screens and flows (MVP)

```
Onboarding      Home                    Money                     Profile
──────────      ────                    ─────                     ───────
splash/lock     balance card            send  (recipient → amount cascade)
register        recent transactions     receive (wallet QR + id)
login           KYC banner if level 0   history (paged statement)
                                        convert (FX quote → confirm)
                                        KYC flow (level 1 → 2)
                                        settings (language, sessions, PIN)
```

- **Language:** Tajik and Russian at launch, English later. All strings through
  ARB files from day one; no hardcoded text.
- **Send flow:** recipient by **phone number** (see §5 — needs a lookup
  endpoint), amount keypad in somoni with live fee line (`fee_minor` mirror of
  the backend bps calc, display-only — the ledger stays authoritative), confirm
  screen shows exactly what the recipient receives, then the §2.2 machine.
- **History:** cursor-paged statement per wallet (see §5), grouped by day,
  entries rendered from the user's perspective (in/out, counterparty, fee).
  Cache last page locally so the app opens instantly offline with a stale
  banner; **never** cache across accounts.
- **KYC flow:** camera capture → upload (see §5) → pending state chip →
  approved/rejected via poll (push later). Level gates mirrored client-side
  for UX only; the backend remains the enforcer.
- **Security hardening:** `FLAG_SECURE` on balance/send screens (no
  screenshots/recents preview), certificate pinning against the API host with
  a remote-config escape hatch, R8 obfuscation, no amounts/PII in client logs.

### App architecture (Kotlin + Jetpack Compose)

```
app/
  core/      Money, ApiClient (OkHttp + interceptors, error-code mapping),
             SecureSession (Keystore), PaymentSubmitter (the §2.2 machine),
             formatters, l10n
  feature/   auth/  home/  send/  receive/  history/  kyc/  fx/  settings/
             (each: repository + ViewModel + Compose screens)
  design/    tokens (color/type/spacing), shared composables
```

- **UI:** Jetpack Compose, single-activity, Navigation-Compose.
- **DI:** Hilt. **Async:** coroutines + Flow (structured concurrency; no RxJava).
- **Network:** Retrofit/OkHttp with certificate pinning; client generated from
  the OpenAPI spec (§5 item 8) so the contract can't drift.
- **Persistence:** Room (SQLite) for txn cache + pending payments;
  EncryptedSharedPreferences/Keystore for the refresh token; nothing sensitive
  in plain prefs. Keep Money/PaymentSubmitter/repositories in pure Kotlin
  modules (no Android imports) — unit-testable on the JVM and KMP-ready.
- **Testing:** JVM unit tests for Money/PaymentSubmitter (the correctness
  core), Compose UI tests per feature, one instrumented suite driving a debug
  build against the local Docker stack (the same one `deploy/` ships).

---

## 4. Admin console: capabilities (MVP)

1. **KYC review queue** — list pending submissions (needs §5 endpoint), view
   details + document, approve/reject with reason. This is the daily-driver
   screen.
2. **User lookup** — by phone: KYC level, status, wallets, balances, recent
   screening events; block/unblock with reason (writes the audit trail).
3. **FX rates** — current table + set rate (num/den with a live preview of the
   implied decimal rate, so nobody fat-fingers an inverted fraction).
4. **Funding (deposits)** — pick user wallet, amount, idempotency handled the
   §2.2 way; this is the partner-bank settlement desk until that integration
   is automated.
5. **Health strip** — links to Grafana (`ssh -L`), latest checkpoint seq +
   last reconciliation/verification timestamps via a small `/v1/admin/status`
   (see §5).

Admin logins are ordinary users flipped via SQL (unchanged by design); the
console is IP-allowlisted at Caddy in addition to auth.

---

## 5. Backend work the frontends need (the gap list)

The current API is money-complete but *app-incomplete*. In build order:

| # | Endpoint | Why | Status |
|---|----------|-----|--------|
| 1 | `GET /v1/wallets` | app home screen (own wallets + balances) | ✅ **shipped** (Phase A) |
| 2 | `GET /v1/accounts/{id}/transactions?cursor=&limit=` | statement/history | ✅ **shipped** (keyset cursor) |
| 3 | `GET /v1/admin/kyc/submissions?status=` | the review queue | ✅ **shipped** |
| 4 | `GET /v1/users/resolve?phone=` or `?wallet=` | send-by-phone + QR | ✅ **shipped 2026-07-20** (see §7.2) |
| 5 | `POST /v1/kyc/documents` (multipart) | document upload | ✅ **shipped** (uuid.ext `document_ref`, admin GET) |
| 6 | `GET /v1/fx/rates` | convert screen quote | ✅ **shipped** (any auth) |
| 7 | `GET /v1/admin/status` | console health strip | ✅ **shipped** (+ `/v1/admin/metrics`) |
| 8 | OpenAPI spec via `utoipa` | generated TS + Dart clients | ❌ not built — customer app hand-writes DTOs for MVP (see §7.4) |
| 9 | Push: `POST /v1/devices` + a notification worker | "you received money" | ❌ not built — Phase C; NATS consumer → FCM |
| 10 | SMS OTP on registration | real phone ownership proof | ❌ not built — needs a TJ provider (OsonSMS); Phase C |

Phase A shipped items 1–3, 5–7. For the customer MVP the only backend gap is
**item 4 (resolve-by-phone)** — everything else the app needs already exists.

---

## 7. Customer app MVP — concrete build plan (2026-07-20)

Sharpens §2–§3 into something buildable, grounded in the API contract as it
**actually exists today** (verified against `crates/api/src/lib.rs`, not memory).

### 7.1 The contract the app codes against (verified)

Money is `amount_minor` (i64, diram) everywhere. Auth is `Bearer <access>`.

| Call | Request | Response |
|------|---------|----------|
| `POST /v1/auth/register` | `{phone, password}` | `TokenResponse` |
| `POST /v1/auth/login` | `{phone, password}` | `TokenResponse` |
| `POST /v1/auth/refresh` | `{refresh_token}` | `TokenResponse` (rotates) |
| `POST /v1/auth/logout` | `{refresh_token}` | 204 |
| `GET /v1/wallets` | — | `[{id, currency, balance_minor, display}]` |
| `GET /v1/accounts/{id}/transactions?cursor=&limit=` | — | `{entries:[{entry_id, transaction_id, direction, amount_minor, currency, created_at_ms, kind, counterparty_phone?, counterparty_name?}], next_cursor}` — `kind` ∈ transfer/deposit/fx/fee/withdrawal/other from the viewer's POV; counterparty only on `transfer` (2026-08-25) |
| `GET /v1/config` | — | `{transfer_fee_bps}` — server-owned pricing for display-only fee previews (2026-08-25) |
| `POST /v1/transfers` | `{from_account, to_account, amount_minor, currency}` + `Idempotency-Key` | `{transaction_id, status}` |
| `POST /v1/fx` | `{from_account, to_account, amount_minor}` + `Idempotency-Key` | `FxResponse` |
| `GET /v1/fx/rates` | — | admin-set rate table |
| `GET /v1/kyc` | — | `{kyc_level, latest_submission?}` |
| `POST /v1/kyc/documents` | multipart `file` | `{document_ref}` |
| `POST /v1/kyc/submissions` | `{requested_level, document_ref?}` | submission |

`TokenResponse = {user_id, access_token, refresh_token, token_type, expires_in}`.
**Access token TTL = 900s (15 min); refresh TTL = 30 days.** Both from config,
so the client must treat `expires_in` as authoritative, not hardcode 15m.

**Error codes** the client maps (from `error.rs`, exhaustive): `account_blocked`,
`amount_too_large`, `bad_request`, `conflict`, `currency_mismatch`,
`duplicate_transaction`, `forbidden`, `idempotency_conflict`,
`insufficient_funds`, `internal_error`, `invalid_amount`, `invalid_transaction`,
`kyc_required`, `limit_exceeded`, `not_found`, `rate_limited`, `unauthorized`,
`unknown_account`, `unknown_currency`. Envelope: `{error:{code, message}}`.

### 7.2 The one open decision — how does a sender pick a recipient?

`POST /v1/transfers` requires `to_account` = the **recipient's wallet UUID**.
Nothing maps a phone number to a wallet, so §3's "send by phone number" cannot
be built as-is. Two ways forward:

- **Option A — build gap item 4 first (`GET /v1/users/resolve?phone=`).** Then
  the app's send flow is what the market expects: type/pick a phone, we resolve
  it to the recipient's TJS wallet + a masked name shown on the confirm screen.
  Cost: ~half a day of backend work + its own guards (KYC-1+ caller only,
  rate-limited, every lookup logged — it's a user-enumeration surface).
- **Option B — QR / wallet-id only for v1, defer phone.** Recipient shows their
  receive-QR (wallet id encoded); sender scans. No backend change. But typing a
  raw UUID is a non-starter, so without QR built this means "no practical send"
  in the very first slice.

**Decided: A — and built 2026-07-20.** `GET /v1/users/resolve` now bridges a
human phone number (or a scanned wallet-QR) to the wallet id `POST /v1/transfers`
needs. One endpoint, two modes:

- `?phone=<digits>` → the "check number" button.
- `?wallet=<uuid>` → the QR-scan confirm (a user's receive-QR encodes their
  wallet id).

Response `{wallet_id, currency, name, name_verified}`. **Names are only ever the
verified name from an *approved* KYC submission** — the `users` table stores no
name, so `name` is `null` (`name_verified:false`) for a registered-but-unverified
recipient (account exists, still sendable, just no name to confirm). Unknown
number → `404 not_found`. Guards: caller must be authenticated **and KYC level 1**
(same bar as sending, so it's not an anonymous enumeration oracle) + the global
rate limiter. **Privacy note / future work:** this still lets any verified user
probe whether a phone is registered and read a verified name; before public
launch, add a dedicated per-user lookup rate-limit + an audit log of lookups, and
decide whether to mask the returned name (e.g. "Firuz R.") — a one-line change in
`resolve_recipient`. Kept full-name for now per product call (confirm-before-send
clarity).

### 7.3 Build order — thin vertical slices (each ends installed & driven on the emulator)

1. ✅ **Skeleton + core + auth** (2026-07-20). Gradle project (flavors
   dev/staging/prod), the pure-Kotlin `core` (Money, error-code mapping),
   `ApiClient`, `SecureSession`, register/login/refresh, single-flight 401
   refresh. *Exit met: real login against local backend AND the LAN server,
   token in Keystore, survives app kill.*
2. ✅ **Home** (2026-08-25). Wallet cards, KYC-level-0 banner, action row
   (Send/Receive/History/Convert), recent-activity preview. Registration now
   auto-creates the TJS wallet server-side (client self-heals an empty list).
3. ✅ **History** (2026-08-25). Paged `…/transactions` (keyset, infinite
   scroll), grouped by day (Today/Yesterday/date), in/out from the user's POV
   with counterparty name/phone from the enriched statement. No Room cache yet
   — deliberate (lean); add if offline history becomes a real ask.
4. ✅ **Send** (2026-08-25). resolve → amount keypad in somoni with live
   display-only fee line (`GET /v1/config` bps, floored exactly like the
   backend) → confirm → `PaymentSubmitter` §2.2 machine. *Exit met and
   exceeded: payment survived server-down submit, app force-stop AND
   reinstall, then same-key retry posted exactly once (ledger-verified).*
5. ✅ **KYC** (2026-08-25). Status states (form/under-review/verified/
   rejected), photo-picker upload (multipart, 5 MB client check), submit,
   10s poll while under review (live-verified: approval flipped the screen
   with no user action). Camera capture (vs. picker) still open.
6. ✅ **FX convert** (2026-08-25). Rate from `/v1/fx/rates`, integer-exact
   floored quote ("you get exactly"), swap direction, one-tap "open a USD
   wallet" when the second wallet is missing.
7. **Settings + hardening pass — PARTIAL.** Done: `FLAG_SECURE` (prod flavor
   only, so emulator QA screenshots keep working), R8 release build green,
   Receive screen (phone + tap-to-copy). Open: PIN/biometric app-lock,
   Tajik+Russian localization, cert pinning (needs the real HTTPS domain),
   log scrubbing audit, receive-QR.

### 7.4 Stack & module specifics

- **Kotlin + Jetpack Compose, single-activity, Navigation-Compose. compileSdk 36,
  minSdk 26** (Android 8: covers the vast majority of the low-end TJ install base
  while keeping Keystore-backed `EncryptedSharedPreferences` and `BiometricPrompt`
  first-class; StrongBox used opportunistically where 28+).
- **What is actually in the tree (2026-09-21), and why it differs from the
  original plan.** The plan said Hilt + Retrofit + Room; the app ships with
  **none of them**, deliberately:
  - **DI: a hand-wired `AppContainer`** (`mobile/app/.../AppContainer.kt`),
    constructed once in `PaymentApp.onCreate`, exposing lazy singletons
    (`SecureSession`, `PendingPaymentPrefsStore`, `ApiClient`, the two
    repositories) that screens receive through `viewModel { ... }` factories in
    `AppRoot`. Eight classes do not justify an annotation processor: no kapt/KSP
    step, no generated graph to debug, and the whole dependency chain is
    readable in one file. Revisit only if the graph stops fitting on a screen.
  - **Network: raw OkHttp + kotlinx-serialization** (`ApiClient.kt`), no
    Retrofit. The API surface is ~15 endpoints; each is an explicit, auditable
    function. This keeps the cross-cutting money rules in one place — the
    `Authenticator` does the **single-flight refresh** with a **tri-state
    outcome** (rotated / dead / unreachable: only a server 401/403 to the
    refresh token signs the user out, anything else is *offline*), no-auth
    requests are tagged so they never enter the refresh path, the
    `Idempotency-Key` header is set by the one `transfer`/`fx` call site, a
    2xx with an unreadable body is reported as *accepted-answer-lost* (never a
    refusal), and redirects are disabled so a bearer or POST body is never
    replayed elsewhere. It also makes the client a plain JVM class: the refresh
    and idempotency paths are tested with MockWebServer, no device.
  - **Persistence: `EncryptedSharedPreferences` only** (two Keystore-backed
    files, one for the session, one for the single pending payment), no Room.
    The app persists exactly two things — the refresh token and the in-flight
    payment with its key — and both must be written **synchronously and
    encrypted** before a request goes out. A relational cache of the statement
    was never needed: history is paged from the server. Opening either store is
    guarded (a corrupt keyset is wiped and recreated rather than crash-looping;
    the user is told once if a payment record was lost).
  - **Async:** coroutines + `StateFlow` per ViewModel; wallets and `/v1/config`
    are cached in `WalletRepository` and shared by Home/Send/FX, invalidated by
    any money move.
- **DTOs hand-written for MVP** (no utoipa/OpenAPI yet — gap item 8). The DTO
  set is small and pinned by §7.1; revisit codegen if/when a second client (iOS)
  appears. Error envelope is `{"error":{"code","message","request_id"}}`; only
  `code` is mapped to copy, `request_id` is quoted as "Ref: …" on 5xx.
- **Module layout:** two Gradle modules. **`:core` is pure Kotlin/JVM (no
  Android imports)** — `Money`, the DTOs, `ErrorCode`, `ApiOutcome`, fees, and
  the `PaymentSubmitter` idempotency state machine, all unit-tested on the JVM
  in milliseconds and KMP-portable. **`:app`** is Compose UI, `ApiClient`, the
  secure stores, repositories and ViewModels (with their own JVM tests for the
  HTTP layer). The `feature/*` split from §3 is deferred until there is enough
  UI to warrant it.
- **Build flavors:** `dev` → `http://10.0.2.2:8099` (local payment-server as seen
  from the emulator), cleartext allowed for that host only via a flavor
  network-security-config; `staging` → the LAN deployment on `192.168.1.156:8099`
  (same cleartext carve-out, for a real phone on the LAN); `prod` → HTTPS base URL
  (placeholder until a server exists), no cleartext, a ready-to-fill `<pin-set>`
  in its network-security-config, `FLAG_SECURE`, and a release build that
  **refuses to package without an upload key** (`keystore.properties` or
  `KEYSTORE_*` env vars — dev/staging stay debug-signed).

### 7.5 The build/test loop on this dev box

- Backend: `payment-server` on `127.0.0.1:8099` (RATE_LIMIT_MAX high) + Docker
  Postgres/NATS/Redis, exactly the console recipe in the runbook.
- App: `./gradlew :app:assembleDevDebug` → `adb -s emulator-5554 install -r` →
  drive on **Pixel_API36**. I can do this entirely headless.
- **Correctness core gets JVM unit tests from slice 1**: Money arithmetic/format
  and the PaymentSubmitter state machine (idempotency-key persistence + retry)
  are where a money bug hides — these run without a device. Compose UI tests and
  one instrumented end-to-end (debug build against the local stack) come later.

### 7.6 What this MVP deliberately excludes

Push notifications (Phase C, gap 9), SMS-OTP registration (Phase C, gap 10, needs
a TJ provider), iOS, and QR *pay* (receive-QR is in; scanning-to-pay rides with
the QR work). Registration therefore trusts the phone number for now — acceptable
for a closed early-merchant track, closed before a public launch.

---

## 6. Delivery plan

- **Phase A — admin console + gap items 1–3, 5, 7.**
  Console deployed as `admin.<domain>` via the existing Caddy container.
  Exit: a KYC submission reviewed end-to-end in a browser against prod.
- **Phase B — customer app MVP (Kotlin/Compose) + gap items 4, 6, 8.**
  Auth, wallet, send/receive, history, KYC, FX; Tajik + Russian; closed track
  on Google Play with sideload APKs for early merchants.
  Exit: a real somoni moved phone-to-phone by non-developers.
- **Phase C — QR receive/pay, push (item 9), OTP (item 10); iOS (Swift or KMP)
  only when user demand is demonstrated.**

Phases end-to-end respect the standing rule: build against real needs, don't
gold-plate ops ahead of usage.
