// API client for the ops console. Same-origin (Caddy proxies /v1 to the
// payment-server), Bearer auth with single-flight refresh, and the API's
// stable machine error codes surfaced to callers.

import { setAuthed, setSessionNotice } from "./auth";

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

/** Operator-facing text for anything an API call throws. Server-side failures
 *  (5xx) carry the request id so the operator can quote it when escalating. */
export function describeError(e: unknown): string {
  if (e instanceof ApiError) {
    return e.status >= 500 && e.requestId ? `${e.message} (Ref: ${e.requestId})` : e.message;
  }
  return e instanceof Error ? e.message : String(e);
}

/** The rejection fetch produces when its AbortSignal fires — never worth showing. */
export function isAbort(e: unknown): boolean {
  return typeof e === "object" && e !== null && (e as { name?: unknown }).name === "AbortError";
}

type Tokens = { access_token: string; refresh_token: string };

// Tokens live in sessionStorage: per tab, gone when the tab closes, never
// visible to other sites. One caveat worth knowing before an operator reports
// "it logged me out of both tabs": Chrome's "Duplicate tab" (and session
// restore) COPIES sessionStorage, so two tabs then hold the same refresh
// token. Whichever refreshes first rotates it; the other's next refresh
// presents the now-revoked token, which the backend treats as replay and
// answers by revoking the whole token family — signing BOTH tabs out. That is
// the intended anti-replay behaviour, not a bug. Open a fresh tab and sign in
// there instead of duplicating.
const STORE_KEY = "console-session";

let access: string | null = null;
let refreshToken: string | null = null;
let refreshing: Promise<number> | null = null;

export function loadSession(): boolean {
  const raw = sessionStorage.getItem(STORE_KEY);
  if (!raw) return false;
  try {
    const t: Tokens = JSON.parse(raw);
    access = t.access_token;
    refreshToken = t.refresh_token;
    return true;
  } catch {
    return false;
  }
}

function saveSession(t: Tokens) {
  access = t.access_token;
  refreshToken = t.refresh_token;
  sessionStorage.setItem(STORE_KEY, JSON.stringify(t));
}

export function clearSession() {
  access = null;
  refreshToken = null;
  sessionStorage.removeItem(STORE_KEY);
}

/** End the session locally and tell the login screen why. */
function endSession(notice: string) {
  clearSession();
  setSessionNotice(notice);
  setAuthed(false);
}

async function parseError(res: Response): Promise<ApiError> {
  const headerId = res.headers.get("x-request-id");
  try {
    const body = await res.json();
    const id = typeof body.error.request_id === "string" ? body.error.request_id : headerId;
    return new ApiError(res.status, body.error.code, body.error.message, id);
  } catch {
    return new ApiError(res.status, "unknown", `HTTP ${res.status}`, headerId);
  }
}

/** Single-flight token rotation. Resolves 0 on success, otherwise the HTTP
 *  status the refresh endpoint answered with. */
async function tryRefresh(): Promise<number> {
  if (!refreshToken) return 401;
  refreshing ??= (async () => {
    const res = await fetch("/v1/auth/refresh", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ refresh_token: refreshToken }),
    });
    if (!res.ok) return res.status;
    saveSession(await res.json());
    return 0;
  })().finally(() => {
    refreshing = null;
  });
  return refreshing;
}

type Init = RequestInit & { idempotencyKey?: string };

async function authedFetch(path: string, init: Init = {}): Promise<Response> {
  const doFetch = () => {
    const headers: Record<string, string> = {
      ...(init.body ? { "content-type": "application/json" } : {}),
      ...(access ? { authorization: `Bearer ${access}` } : {}),
      ...(init.idempotencyKey ? { "idempotency-key": init.idempotencyKey } : {}),
    };
    return fetch(path, { ...init, headers });
  };

  let res = await doFetch();
  if (res.status === 401 && refreshToken) {
    const failed = await tryRefresh();
    if (failed === 0) {
      res = await doFetch();
    } else {
      // The refresh endpoint answers 403 when the account is no longer active
      // (frozen/closed); anything else is an expired or replayed token.
      endSession(
        failed === 403
          ? "Signed out: this account is frozen or closed."
          : "Session expired — sign in again.",
      );
    }
  }
  if (res.status === 403) {
    // Every endpoint this console calls sits behind the admin gate, which
    // answers 403 `forbidden` when the caller was demoted or frozen
    // mid-session. Other 403 codes (kyc_required, account_blocked) describe
    // the customer a call targets, not us — those surface as plain errors.
    const err = await parseError(res);
    if (err.code === "forbidden") {
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
  const res = await fetch("/v1/auth/login", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ phone, password }),
  });
  if (!res.ok) throw await parseError(res);
  const tokens: Tokens = await res.json();
  // Only admins get past this point — probe an admin endpoint before saving
  // the session so a non-admin login fails here rather than on every screen.
  // A plain fetch, not authedFetch: a 403 here means "not an admin", which
  // must not raise the mid-session "you were demoted" notice.
  const probe = await fetch("/v1/admin/status", {
    headers: { authorization: `Bearer ${tokens.access_token}` },
  });
  if (!probe.ok) {
    // Revoke the pair we will never use; best-effort.
    await fetch("/v1/auth/logout", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ refresh_token: tokens.refresh_token }),
    }).catch(() => {});
    throw await parseError(probe);
  }
  saveSession(tokens);
  setAuthed(true);
}

export async function logout() {
  const rt = refreshToken;
  clearSession();
  setAuthed(false);
  if (rt) {
    await fetch("/v1/auth/logout", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ refresh_token: rt }),
    }).catch(() => {});
  }
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
export const deposit = (
  user_account: string,
  amount_minor: number,
  key: string,
  currency = "TJS",
) =>
  api<{ transaction_id: string; status: string }>("/v1/deposits", {
    method: "POST",
    body: JSON.stringify({ user_account, amount_minor, currency }),
    idempotencyKey: key,
  });
export const setUserStatus = (user_id: string, status: "active" | "frozen" | "closed") =>
  api(`/v1/admin/users/${user_id}/status`, {
    method: "POST",
    body: JSON.stringify({ status }),
  });

// crypto.randomUUID() exists only in a secure context (HTTPS or localhost). This
// console can be served over plain HTTP on a LAN IP, where it is undefined — so
// derive a v4 UUID from getRandomValues (available in insecure contexts too),
// falling back to Math.random only if even that is missing.
export function uuidv4(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  const b = new Uint8Array(16);
  if (typeof crypto !== "undefined" && crypto.getRandomValues) {
    crypto.getRandomValues(b);
  } else {
    for (let i = 0; i < 16; i++) b[i] = Math.floor(Math.random() * 256);
  }
  b[6] = (b[6] & 0x0f) | 0x40; // version 4
  b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
  const h = Array.from(b, (x) => x.toString(16).padStart(2, "0"));
  return `${h[0]}${h[1]}${h[2]}${h[3]}-${h[4]}${h[5]}-${h[6]}${h[7]}-${h[8]}${h[9]}-${h[10]}${h[11]}${h[12]}${h[13]}${h[14]}${h[15]}`;
}

export function formatTime(ms: number): string {
  return new Date(ms).toLocaleString();
}
