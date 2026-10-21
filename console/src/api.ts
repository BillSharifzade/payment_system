// API client for the ops console. Same-origin (Caddy proxies /v1 to the
// payment-server), Bearer auth with single-flight refresh, and the API's
// stable machine error codes surfaced to callers.

import { lastActivityAt, resetActivity } from "./activity";
import { setAuthed, setSessionNotice } from "./auth";
import { errorCopy } from "./errors";

export class ApiError extends Error {
  constructor(
    public status: number,
    public code: string,
    message: string,
    /** Server correlation id (error.request_id, else the X-Request-Id header). */
    public requestId: string | null = null,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

/** fetch() itself failed (offline, DNS, connection reset, CORS…): no HTTP
 *  answer at all, so for a write the outcome is unknown. */
export class NetworkError extends Error {
  constructor(cause?: unknown) {
    super("network error", { cause });
    this.name = "NetworkError";
  }
}

/** A background request (a poll) needed a token refresh while nobody has
 *  touched the console since the current token was minted. Polls must not keep
 *  an idle session alive, so the request is dropped instead; pollers keep
 *  their last data and try again on the next tick. */
export class IdlePausedError extends Error {
  constructor() {
    super("paused while idle");
    this.name = "IdlePausedError";
  }
}

/** Operator-facing text for anything an API call throws: copy keyed by the
 *  machine code (never the server's English message, FRONTEND.md §2.4), with
 *  the request id on server-side failures so the operator can quote it.
 *  `overrides` lets a page give a code a sharper, context-specific meaning. */
export function describeError(e: unknown, overrides: Record<string, string> = {}): string {
  if (e instanceof ApiError) {
    const base =
      overrides[e.code] ??
      errorCopy(e.code) ??
      `Unexpected error (${e.code || "no code"}, HTTP ${e.status}).`;
    // bad_request is the API's catch-all for validation; its code alone says
    // nothing useful, so the server's detail rides along as a secondary note.
    const detail = e.code === "bad_request" && e.message ? ` Details: ${e.message}` : "";
    const ref = e.requestId && (e.status >= 500 || e.status === 0) ? ` (Ref: ${e.requestId})` : "";
    return `${base}${detail}${ref}`;
  }
  if (e instanceof NetworkError) return errorCopy("network")!;
  if (e instanceof IdlePausedError) return "Paused while you were away.";
  return e instanceof Error ? e.message : String(e);
}

/** The rejection fetch produces when its AbortSignal fires — never worth showing. */
export function isAbort(e: unknown): boolean {
  return typeof e === "object" && e !== null && (e as { name?: unknown }).name === "AbortError";
}

// ----- Session (memory only) ---------------------------------------------------
//
// Access AND refresh tokens live in module memory only (FRONTEND.md §2.3:
// "Admin console: refresh token in memory + re-login is acceptable"). Nothing
// is written to sessionStorage/localStorage, so a script that ever ran on this
// origin, a duplicated tab or a restored session cannot lift a 30-day refresh
// token. The price: a reload or a new tab needs a fresh sign-in (the login
// page says so).

type TokenResponse = {
  user_id: string;
  access_token: string;
  refresh_token: string;
};

let access: string | null = null;
let refreshToken: string | null = null;
// When the current access token was minted (login / last rotation). A poll
// may only trigger a rotation if a human touched the console since then.
let mintedAt = 0;
// Bumped whenever a session starts or ends, so a refresh that was in flight
// when the operator signed out can never resurrect the old session.
let sessionGen = 0;
let refreshing: Promise<RefreshOutcome> | null = null;

/** Storage keys used by earlier console builds (tokens in sessionStorage, a
 *  shared pending-deposit record in localStorage). Wiped at startup so an
 *  upgraded browser no longer carries them. */
export function purgeLegacyStorage(): void {
  try {
    sessionStorage.removeItem("console-session");
  } catch {
    /* storage unavailable — nothing to purge */
  }
  try {
    localStorage.removeItem("console-pending-deposit");
  } catch {
    /* storage unavailable — nothing to purge */
  }
}

function applyTokens(t: TokenResponse, now: number = Date.now()) {
  access = t.access_token;
  refreshToken = t.refresh_token;
  mintedAt = now;
}

function clearSession() {
  access = null;
  refreshToken = null;
  mintedAt = 0;
  sessionGen++;
}

/** True while this tab holds a session (tests and the idle watcher use it). */
export function hasSession(): boolean {
  return refreshToken !== null;
}

function revoke(rt: string | null) {
  if (!rt) return Promise.resolve();
  return fetch("/v1/auth/logout", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ refresh_token: rt }),
  }).then(
    () => {},
    () => {},
  );
}

/** End the session locally, tell the login screen why, and (best-effort) revoke
 *  the refresh token server-side unless the server already declared it dead. */
function endSession(notice: string, revokeToken = true) {
  const rt = refreshToken;
  clearSession();
  setSessionNotice(notice);
  setAuthed(false);
  if (revokeToken) void revoke(rt);
}

async function parseError(res: Response): Promise<ApiError> {
  const headerId = res.headers.get("x-request-id");
  try {
    const body = await res.json();
    const id = typeof body.error.request_id === "string" ? body.error.request_id : headerId;
    return new ApiError(res.status, String(body.error.code), String(body.error.message), id);
  } catch {
    return new ApiError(res.status, "unknown", `HTTP ${res.status}`, headerId);
  }
}

// ----- Token refresh -------------------------------------------------------------

type RefreshOutcome =
  | "rotated" // new pair stored
  | "dead401" // refresh token expired / revoked / replayed → sign in again
  | "dead403" // account no longer active (frozen/closed)
  | "unreachable" // 5xx / 429 / network after every retry → keep the session
  | "ended"; // the session ended (sign-out, idle) while we were refreshing

/** Bounded exponential backoff for a refresh the server could not answer.
 *  Exported (mutable) so tests can shorten it. */
export const refreshPolicy = { attempts: 4, baseDelayMs: 500, maxDelayMs: 8_000 };

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

function retryAfterMs(res: Response): number | null {
  const v = res.headers.get("retry-after");
  if (!v) return null;
  const secs = Number(v);
  return Number.isFinite(secs) && secs >= 0 ? secs * 1000 : null;
}

type RefreshAttempt =
  | { kind: "ok"; tokens: TokenResponse }
  | { kind: "dead"; status: 401 | 403 }
  | { kind: "retry"; waitMs: number | null };

async function refreshOnce(rt: string): Promise<RefreshAttempt> {
  let res: Response;
  try {
    res = await fetch("/v1/auth/refresh", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ refresh_token: rt }),
    });
  } catch {
    return { kind: "retry", waitMs: null };
  }
  if (res.ok) {
    try {
      return { kind: "ok", tokens: (await res.json()) as TokenResponse };
    } catch {
      // Rotated but unreadable: the next attempt presents the old token, which
      // the server answers with 401 — so this resolves itself either way.
      return { kind: "retry", waitMs: null };
    }
  }
  // Only the server saying "this token is no good" (401) or "this account is
  // no longer active" (403) ends the session. A 502 during a deploy, a 429 or
  // anything else is transient: keep the session and try again.
  if (res.status === 401 || res.status === 403) return { kind: "dead", status: res.status };
  return { kind: "retry", waitMs: retryAfterMs(res) };
}

/** Single-flight token rotation: concurrent 401s share one refresh, because
 *  parallel refreshes would trip rotation (the second presents a revoked token). */
function tryRefresh(): Promise<RefreshOutcome> {
  if (!refreshToken) return Promise.resolve("dead401");
  refreshing ??= (async (): Promise<RefreshOutcome> => {
    const gen = sessionGen;
    const rt = refreshToken!;
    for (let attempt = 0; ; attempt++) {
      const r = await refreshOnce(rt);
      if (gen !== sessionGen) return "ended";
      if (r.kind === "ok") {
        applyTokens(r.tokens);
        return "rotated";
      }
      if (r.kind === "dead") return r.status === 403 ? "dead403" : "dead401";
      if (attempt + 1 >= refreshPolicy.attempts) return "unreachable";
      const backoff = Math.min(refreshPolicy.baseDelayMs * 2 ** attempt, refreshPolicy.maxDelayMs);
      await sleep(Math.min(Math.max(backoff, r.waitMs ?? 0), refreshPolicy.maxDelayMs));
      if (gen !== sessionGen) return "ended";
    }
  })().finally(() => {
    refreshing = null;
  });
  return refreshing;
}

/** After a 403 `forbidden`, ask the admin gate whether WE lost admin rights
 *  (demoted / frozen mid-session) or the call was refused for its target (e.g.
 *  depositing into your own wallet). Only a definite 403 `forbidden` from the
 *  gate itself ends the session; anything inconclusive keeps it. */
async function adminRightsRevoked(): Promise<boolean> {
  if (!access) return false;
  try {
    const res = await fetch("/v1/admin/status", {
      headers: { authorization: `Bearer ${access}` },
    });
    if (res.status !== 403) return false;
    return (await parseError(res)).code === "forbidden";
  } catch {
    return false;
  }
}

type Init = RequestInit & {
  idempotencyKey?: string;
  /** A poll, not an operator action: never keeps an idle session alive. */
  background?: boolean;
};

async function authedFetch(path: string, init: Init = {}): Promise<Response> {
  const { idempotencyKey, background, ...rest } = init;
  const doFetch = async (token: string | null) => {
    const headers: Record<string, string> = {
      ...(rest.body ? { "content-type": "application/json" } : {}),
      ...(token ? { authorization: `Bearer ${token}` } : {}),
      ...(idempotencyKey ? { "idempotency-key": idempotencyKey } : {}),
    };
    try {
      return await fetch(path, { ...rest, headers });
    } catch (e) {
      if (isAbort(e)) throw e;
      throw new NetworkError(e);
    }
  };

  let used = access;
  let res = await doFetch(used);
  if (res.status === 401 && refreshToken && access && access !== used) {
    // Another request rotated the pair while this one was in flight; the 401
    // is for the old token — replay with the new one, no second rotation.
    used = access;
    res = await doFetch(used);
  }
  if (res.status === 401 && refreshToken) {
    if (background && lastActivityAt() <= mintedAt) throw new IdlePausedError();
    const outcome = await tryRefresh();
    if (outcome === "rotated") {
      res = await doFetch(access);
    } else if (outcome === "dead403") {
      endSession("Signed out: this account is frozen or closed.", false);
    } else if (outcome === "dead401") {
      endSession("Session expired — sign in again.", false);
    } else if (outcome === "unreachable") {
      throw new ApiError(0, "refresh_unavailable", "token refresh unavailable");
    }
  }
  if (res.status === 403) {
    // Every endpoint this console calls sits behind the admin gate, which
    // answers 403 `forbidden` when the caller was demoted or frozen
    // mid-session. But `forbidden` can also describe the target (deposit
    // into the caller's own wallet), so confirm with the gate before ending
    // the session. Other 403 codes (kyc_required, account_blocked,
    // dual_control_required) are about the request, not us.
    const err = await parseError(res);
    if (err.code === "forbidden" && (await adminRightsRevoked())) {
      endSession("Signed out: no longer an admin, or the account is frozen.");
    }
    throw err;
  }
  if (!res.ok) throw await parseError(res);
  return res;
}

/** JSON call with Bearer auth. Pass `init.signal` to make it abortable. */
export async function api<T>(path: string, init: Init = {}): Promise<T> {
  const res = await authedFetch(path, init);
  if (res.status === 204) return undefined as T;
  return res.json();
}

// Fetch a binary resource WITH the Bearer header and hand back an object URL.
// Needed because <img src> / <iframe src> requests carry no Authorization
// header, so protected endpoints can't be used as a src directly. Callers must
// URL.revokeObjectURL() when done.
export async function apiBlobUrl(path: string, signal?: AbortSignal): Promise<string> {
  const res = await authedFetch(path, { signal });
  return URL.createObjectURL(await res.blob());
}

export async function login(phone: string, password: string): Promise<void> {
  let res: Response;
  try {
    res = await fetch("/v1/auth/login", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ phone, password }),
    });
  } catch (e) {
    throw new NetworkError(e);
  }
  if (!res.ok) throw await parseError(res);
  const tokens: TokenResponse = await res.json();
  // Only admins get past this point — probe an admin endpoint before keeping
  // the session so a non-admin login fails here rather than on every screen.
  // A plain fetch, not authedFetch: a 403 here means "not an admin", which
  // must not raise the mid-session "you were demoted" notice.
  let probe: Response;
  try {
    probe = await fetch("/v1/admin/status", {
      headers: { authorization: `Bearer ${tokens.access_token}` },
    });
  } catch (e) {
    void revoke(tokens.refresh_token);
    throw new NetworkError(e);
  }
  if (!probe.ok) {
    // Revoke the pair we will never use; best-effort.
    await revoke(tokens.refresh_token);
    throw await parseError(probe);
  }
  clearSession(); // bump the generation: nothing from a previous session survives
  // The activity clock starts at the mint time, so a poll cannot trigger the
  // first rotation unless the operator does something after signing in.
  const now = Date.now();
  applyTokens(tokens, now);
  resetActivity(now);
  setAuthed(true, tokens.user_id);
}

/** Sign out: forget the tokens, wipe per-admin client state (auth.ts runs the
 *  registered resets), and revoke the refresh token server-side. `notice`
 *  explains a sign-out the operator did not ask for (idle timeout). */
export async function logout(notice?: string) {
  const rt = refreshToken;
  clearSession();
  if (notice) setSessionNotice(notice);
  setAuthed(false);
  await revoke(rt);
}

// ----- Typed endpoint wrappers ------------------------------------------------

export type Wallet = {
  id: string;
  currency: string;
  balance_minor: number;
  display: string;
};

export type KycSubmission = {
  id: string;
  user_id: string;
  phone: string;
  requested_level: number;
  full_name: string;
  document_type: string;
  document_ref: string;
  status: string;
  created_at_ms: number;
  /** Opaque keyset position; pass the last item's as `after` for the next page. */
  cursor?: string;
};

export type AdminUser = {
  id: string;
  phone: string;
  status: string;
  kyc_level: number;
  is_admin: boolean;
  created_at_ms: number;
  blocked_reason: string | null;
  wallets: Wallet[];
};

export type AdminStatus = {
  latest_checkpoint: {
    seq: number;
    to_txn_seq: number;
    txn_count: number;
    created_at_ms: number;
  } | null;
  unsealed_transactions: number;
  conservation: { currency: string; net_minor: number }[];
};

export type FxRate = {
  base: string;
  quote: string;
  rate_num: number;
  rate_den: number;
  updated_at_ms: number;
};

export type UserListItem = {
  id: string;
  phone: string;
  status: string;
  kyc_level: number;
  is_admin: boolean;
  is_blocked: boolean;
  created_at_ms: number;
};

export type UserList = { users: UserListItem[]; next_cursor: string | null };

export type Metrics = {
  daily_volume: { date: string; currency: string; volume_minor: number }[];
  daily_transactions: { date: string; count: number }[];
  mix_30d: { kind: string; count: number }[];
  kyc: {
    pending: number;
    approved: number;
    rejected: number;
    decisions_14d: { date: string; approved: number; rejected: number }[];
  };
  customer_funds: { currency: string; total_minor: number }[];
  users: { total: number; blocked: number; new_30d: { date: string; count: number }[] };
  aml_blocked_30d: number;
};

/** Contract §3. `id` = the Idempotency-Key the request was made with = the
 *  eventual ledger transaction id. */
export type DepositStatus = "pending_approval" | "posted" | "rejected";
export type Deposit = {
  id: string;
  transaction_id: string;
  status: DepositStatus;
  user_account: string;
  amount_minor: number;
  currency: string;
  customer_phone: string | null;
  requested_by: string;
  requested_at: string;
  decided_by: string | null;
  decided_at: string | null;
  reason: string | null;
};
export type DepositPage = { items: Deposit[]; next_cursor: string | null };

/** Contract §8. `api_key` is present only in the create response. */
export type Terminal = {
  id: string;
  merchant_user_id: string;
  label: string;
  created_at: string;
  revoked_at: string | null;
  last_used_at: string | null;
};
export type CreatedTerminal = Terminal & { api_key: string };

export const getMetrics = (signal?: AbortSignal) =>
  api<Metrics>("/v1/admin/metrics", { signal });
export const listUsers = (
  opts: { q?: string; cursor?: string; limit?: number; signal?: AbortSignal } = {},
) => {
  const p = new URLSearchParams();
  if (opts.q) p.set("q", opts.q);
  if (opts.cursor) p.set("cursor", opts.cursor);
  if (opts.limit) p.set("limit", String(opts.limit));
  const qs = p.toString();
  return api<UserList>(`/v1/admin/users/list${qs ? `?${qs}` : ""}`, { signal: opts.signal });
};
export const getStatus = (signal?: AbortSignal) =>
  api<AdminStatus>("/v1/admin/status", { signal });

// Poll fetchers (stable module-level functions for usePoll): flagged as
// background so they never extend an idle session.
export const pollStatus = (signal: AbortSignal) =>
  api<AdminStatus>("/v1/admin/status", { signal, background: true });
export const pollMetrics = (signal: AbortSignal) =>
  api<Metrics>("/v1/admin/metrics", { signal, background: true });
/** How many pending deposit requests the badge fetches; "N+" beyond it. */
export const PENDING_BADGE_LIMIT = 50;
export const pollPendingDeposits = (signal: AbortSignal) =>
  api<DepositPage>(
    `/v1/admin/deposits?status=pending_approval&limit=${PENDING_BADGE_LIMIT}`,
    { signal, background: true },
  );

/** One page of the queue, oldest first. `after` is the previous page's last
 *  cursor; an empty array means the end. */
export const getKycQueue = (
  status: string,
  opts: { limit?: number; after?: string; signal?: AbortSignal } = {},
) => {
  const p = new URLSearchParams({ status });
  if (opts.limit) p.set("limit", String(opts.limit));
  if (opts.after) p.set("after", opts.after);
  return api<KycSubmission[]>(`/v1/admin/kyc/submissions?${p}`, { signal: opts.signal });
};
export const approveKyc = (id: string) =>
  api(`/v1/kyc/submissions/${id}/approve`, { method: "POST" });
export const rejectKyc = (id: string, reason: string) =>
  api(`/v1/kyc/submissions/${id}/reject`, {
    method: "POST",
    body: JSON.stringify({ reason }),
  });
export const lookupUser = (phone: string, signal?: AbortSignal) =>
  api<AdminUser>(`/v1/admin/users?phone=${encodeURIComponent(phone)}`, { signal });
export const blockUser = (user_id: string, reason: string) =>
  api("/v1/admin/blocks", {
    method: "POST",
    body: JSON.stringify({ user_id, reason }),
  });
export const unblockUser = (user_id: string) =>
  api(`/v1/admin/blocks/${user_id}`, { method: "DELETE" });
export const getFxRates = (signal?: AbortSignal) => api<FxRate[]>("/v1/fx/rates", { signal });
/** Upsert one direction; with `alsoReverse` the backend writes the exact
 *  inverse in the same transaction, so the pair can never be half-updated. */
export const setFxRate = (
  base: string,
  quote: string,
  rate_num: number,
  rate_den: number,
  alsoReverse = false,
) =>
  api("/v1/admin/fx-rates", {
    method: "POST",
    body: JSON.stringify({
      base,
      quote,
      rate_num,
      rate_den,
      ...(alsoReverse ? { also_reverse: true } : {}),
    }),
  });

/** Request a deposit (maker step). With dual control on the server answers
 *  202 `pending_approval`; replays of the same key return the same request. */
export const requestDeposit = (
  key: string,
  user_account: string,
  amount_minor: number,
  currency = "TJS",
) =>
  api<Deposit>("/v1/deposits", {
    method: "POST",
    body: JSON.stringify({ user_account, amount_minor, currency }),
    idempotencyKey: key,
  });
export const listDeposits = (
  status: DepositStatus,
  opts: { cursor?: string; limit?: number; signal?: AbortSignal } = {},
) => {
  const p = new URLSearchParams({ status });
  if (opts.cursor) p.set("cursor", opts.cursor);
  if (opts.limit) p.set("limit", String(opts.limit));
  return api<DepositPage>(`/v1/admin/deposits?${p}`, { signal: opts.signal });
};
export const getDeposit = (id: string, signal?: AbortSignal, background = false) =>
  api<Deposit>(`/v1/admin/deposits/${encodeURIComponent(id)}`, { signal, background });
export const approveDeposit = (id: string) =>
  api<Deposit>(`/v1/admin/deposits/${encodeURIComponent(id)}/approve`, { method: "POST" });
export const rejectDeposit = (id: string, reason: string) =>
  api<Deposit>(`/v1/admin/deposits/${encodeURIComponent(id)}/reject`, {
    method: "POST",
    body: JSON.stringify({ reason }),
  });

export const listTerminals = (merchant_user_id: string, signal?: AbortSignal) =>
  api<{ items: Terminal[] }>(
    `/v1/admin/terminals?merchant_user_id=${encodeURIComponent(merchant_user_id)}`,
    { signal },
  );
export const createTerminal = (merchant_user_id: string, label: string) =>
  api<CreatedTerminal>("/v1/admin/terminals", {
    method: "POST",
    body: JSON.stringify({ merchant_user_id, label }),
  });
export const revokeTerminal = (id: string) =>
  api<Terminal>(`/v1/admin/terminals/${encodeURIComponent(id)}/revoke`, { method: "POST" });

export const setUserStatus = (user_id: string, status: "active" | "frozen" | "closed") =>
  api(`/v1/admin/users/${user_id}/status`, {
    method: "POST",
    body: JSON.stringify({ status }),
  });

/** A v4 UUID for an Idempotency-Key, from the platform CSPRNG only.
 *  crypto.randomUUID() exists only in a secure context (HTTPS or localhost);
 *  getRandomValues() works on a plain-HTTP LAN deployment too. With neither,
 *  this throws rather than fall back to a predictable generator — a guessable
 *  or colliding key on a money request is worse than refusing to send it. */
export function uuidv4(): string {
  const c: Crypto | undefined = typeof crypto !== "undefined" ? crypto : undefined;
  if (c && typeof c.randomUUID === "function") return c.randomUUID();
  if (!c || typeof c.getRandomValues !== "function") {
    throw new Error(
      "This browser has no secure random generator, so a request key cannot be created safely. Use a current browser.",
    );
  }
  const b = new Uint8Array(16);
  c.getRandomValues(b);
  b[6] = (b[6] & 0x0f) | 0x40; // version 4
  b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
  const h = Array.from(b, (x) => x.toString(16).padStart(2, "0"));
  return `${h[0]}${h[1]}${h[2]}${h[3]}-${h[4]}${h[5]}-${h[6]}${h[7]}-${h[8]}${h[9]}-${h[10]}${h[11]}${h[12]}${h[13]}${h[14]}${h[15]}`;
}

export function formatTime(ms: number): string {
  return new Date(ms).toLocaleString();
}

/** RFC 3339 → epoch ms (NaN-safe: an unparsable value yields null). */
export function parseTime(s: string | null | undefined): number | null {
  if (!s) return null;
  const ms = Date.parse(s);
  return Number.isNaN(ms) ? null : ms;
}
