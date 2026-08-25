// API client for the ops console. Same-origin (Caddy proxies /v1 to the
// payment-server), Bearer auth with single-flight refresh, and the API's
// stable machine error codes surfaced to callers.

export class ApiError extends Error {
  constructor(
    public status: number,
    public code: string,
    message: string,
  ) {
    super(message);
  }
}

type Tokens = { access_token: string; refresh_token: string };

const STORE_KEY = "console-session";

let access: string | null = null;
let refreshToken: string | null = null;
let refreshing: Promise<boolean> | null = null;
let onSessionLost: () => void = () => {};

export function setSessionLostHandler(fn: () => void) {
  onSessionLost = fn;
}

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

async function parseError(res: Response): Promise<ApiError> {
  try {
    const body = await res.json();
    return new ApiError(res.status, body.error.code, body.error.message);
  } catch {
    return new ApiError(res.status, "unknown", `HTTP ${res.status}`);
  }
}

async function tryRefresh(): Promise<boolean> {
  if (!refreshToken) return false;
  refreshing ??= (async () => {
    const res = await fetch("/v1/auth/refresh", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ refresh_token: refreshToken }),
    });
    if (!res.ok) return false;
    saveSession(await res.json());
    return true;
  })().finally(() => {
    refreshing = null;
  });
  return refreshing;
}

async function authedFetch(
  path: string,
  init: RequestInit & { idempotencyKey?: string } = {},
): Promise<Response> {
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
    if (await tryRefresh()) {
      res = await doFetch();
    } else {
      clearSession();
      onSessionLost();
    }
  }
  if (!res.ok) throw await parseError(res);
  return res;
}

export async function api<T>(
  path: string,
  init: RequestInit & { idempotencyKey?: string } = {},
): Promise<T> {
  const res = await authedFetch(path, init);
  if (res.status === 204) return undefined as T;
  return res.json();
}

// Fetch a binary resource WITH the Bearer header and hand back an object URL.
// Needed because <img src> / <iframe src> requests carry no Authorization
// header, so protected endpoints can't be used as a src directly. Callers must
// URL.revokeObjectURL() when done.
export async function apiBlobUrl(path: string): Promise<string> {
  const res = await authedFetch(path);
  return URL.createObjectURL(await res.blob());
}

export async function login(phone: string, password: string): Promise<void> {
  const res = await fetch("/v1/auth/login", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ phone, password }),
  });
  if (!res.ok) throw await parseError(res);
  saveSession(await res.json());
  // Only admins get past this point — probe an admin endpoint immediately so a
  // non-admin login fails here rather than on every screen. Drop the saved
  // session on failure, or a reload would render the shell with all-403s.
  try {
    await api("/v1/admin/status");
  } catch (e) {
    clearSession();
    throw e;
  }
}

export async function logout() {
  if (refreshToken) {
    await fetch("/v1/auth/logout", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ refresh_token: refreshToken }),
    }).catch(() => {});
  }
  clearSession();
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

export const getMetrics = () => api<Metrics>("/v1/admin/metrics");
export const listUsers = (opts: { q?: string; cursor?: string; limit?: number } = {}) => {
  const p = new URLSearchParams();
  if (opts.q) p.set("q", opts.q);
  if (opts.cursor) p.set("cursor", opts.cursor);
  if (opts.limit) p.set("limit", String(opts.limit));
  const qs = p.toString();
  return api<UserList>(`/v1/admin/users/list${qs ? `?${qs}` : ""}`);
};
export const getStatus = () => api<AdminStatus>("/v1/admin/status");
export const getKycQueue = (status: string) =>
  api<KycSubmission[]>(`/v1/admin/kyc/submissions?status=${status}`);
export const approveKyc = (id: string) =>
  api(`/v1/kyc/submissions/${id}/approve`, { method: "POST" });
export const rejectKyc = (id: string, reason: string) =>
  api(`/v1/kyc/submissions/${id}/reject`, {
    method: "POST",
    body: JSON.stringify({ reason }),
  });
export const lookupUser = (phone: string) =>
  api<AdminUser>(`/v1/admin/users?phone=${encodeURIComponent(phone)}`);
export const blockUser = (user_id: string, reason: string) =>
  api("/v1/admin/blocks", {
    method: "POST",
    body: JSON.stringify({ user_id, reason }),
  });
export const unblockUser = (user_id: string) =>
  api(`/v1/admin/blocks/${user_id}`, { method: "DELETE" });
export const getFxRates = () => api<FxRate[]>("/v1/fx/rates");
export const setFxRate = (base: string, quote: string, rate_num: number, rate_den: number) =>
  api("/v1/admin/fx-rates", {
    method: "POST",
    body: JSON.stringify({ base, quote, rate_num, rate_den }),
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

export function formatMinor(minor: number, currency: string): string {
  const major = Math.trunc(Math.abs(minor) / 100);
  const cents = String(Math.abs(minor) % 100).padStart(2, "0");
  return `${minor < 0 ? "-" : ""}${major.toLocaleString()}.${cents} ${currency}`;
}

export function formatTime(ms: number): string {
  return new Date(ms).toLocaleString();
}
