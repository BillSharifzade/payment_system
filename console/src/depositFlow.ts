// The console's half of the deposit idempotency contract (FRONTEND.md §2.2,
// contract §3), as a small external store so it survives navigating away from
// the Funding page — and is wiped on sign-out (auth.ts session reset), never
// shared across admins or persisted to storage.
//
//   idle ─ submit ─> submitting(key) ─┬─> tracked(deposit)      202 pending / 201 posted
//                                     ├─> refused               definite 4xx: never created
//                                     └─> unknown(key) ─ check ─┬─> tracked / mismatch
//                                          (network, 5xx, 504,  └─> not_found(key)
//                                           429, 409, bad body)       └─ retry (same key)
//
// - The key is created when the operator confirms and reused verbatim for
//   every retry; a fresh key is only ever minted by a new confirmation.
// - An unknown outcome is resolved by GET /v1/admin/deposits/{key}, never by
//   guessing. A 409 (conflict / idempotency_conflict / voided) is NOT read as
//   "definitely failed" until that lookup says so.
// - The server's answer is checked against what the operator confirmed; a
//   different wallet/amount under our key is surfaced as a loud mismatch.

import { useSyncExternalStore } from "react";
import {
  ApiError,
  Deposit,
  IdlePausedError,
  describeError,
  getDeposit,
  requestDeposit,
  uuidv4,
} from "./api";
import { onSessionReset } from "./auth";

export type DepositDraft = {
  customer_user_id: string;
  phone: string;
  wallet: string;
  currency: string;
  amount_minor: number;
};

export type FlowState =
  | { phase: "idle" }
  | { phase: "submitting"; key: string; draft: DepositDraft }
  | {
      phase: "unknown";
      key: string;
      draft: DepositDraft;
      /** Why the outcome is unknown (the failed request). */
      error: string;
      /** A status lookup is in flight. */
      checking: boolean;
      /** The last status lookup also failed. */
      checkError: string | null;
    }
  | { phase: "not_found"; key: string; draft: DepositDraft; cause: string }
  | { phase: "tracked"; draft: DepositDraft; deposit: Deposit; refreshError: string | null }
  | { phase: "refused"; draft: DepositDraft; error: string; code: string | null }
  | { phase: "mismatch"; key: string; draft: DepositDraft; deposit: Deposit };

const IDLE: FlowState = { phase: "idle" };
let state: FlowState = IDLE;
// Bumped on every reset/discard/new submission: a response that arrives for a
// superseded attempt (e.g. after sign-out) is dropped instead of written back.
let gen = 0;
const listeners = new Set<() => void>();

function set(next: FlowState) {
  state = next;
  for (const l of listeners) l();
}

export function getFlow(): FlowState {
  return state;
}

function subscribe(l: () => void) {
  listeners.add(l);
  return () => {
    listeners.delete(l);
  };
}

export function useDepositFlow(): FlowState {
  return useSyncExternalStore(subscribe, getFlow, getFlow);
}

/** Back to idle and forget the key. Registered as a session reset, so signing
 *  out (or a different admin signing in) always clears it. */
export function resetFlow(): void {
  gen++;
  set(IDLE);
}
onSessionReset(resetFlow);

/** Does the server's deposit describe exactly what the operator confirmed? */
export function matchesDraft(d: Deposit, draft: DepositDraft): boolean {
  return (
    d.user_account === draft.wallet &&
    d.amount_minor === draft.amount_minor &&
    d.currency === draft.currency
  );
}

/** A definite refusal: the request was validated and turned down, so nothing
 *  was created and the key can be dropped. 409s are deliberately absent (they
 *  need a lookup), as are 429/5xx/504 and transport failures (outcome unknown). */
function isDefiniteRefusal(e: unknown): e is ApiError {
  return e instanceof ApiError && [400, 401, 403, 404, 422].includes(e.status);
}

// Copy for the codes a deposit request can be refused with, sharper than the
// generic table in errors.ts.
export const DEPOSIT_ERROR_COPY: Record<string, string> = {
  forbidden: "You cannot request a deposit into your own wallet.",
  limit_exceeded:
    "The amount is over the per-deposit maximum the server allows. Split it or check the figure.",
  unknown_account: "That wallet no longer exists — look the customer up again.",
  account_blocked: "The customer's account is blocked; deposits are refused.",
};

function describeDepositError(e: unknown): string {
  return describeError(e, DEPOSIT_ERROR_COPY);
}

function landed(draft: DepositDraft, key: string, d: Deposit): FlowState {
  return matchesDraft(d, draft)
    ? { phase: "tracked", draft, deposit: d, refreshError: null }
    : { phase: "mismatch", key, draft, deposit: d };
}

async function send(key: string, draft: DepositDraft, g: number): Promise<void> {
  try {
    const d = await requestDeposit(key, draft.wallet, draft.amount_minor, draft.currency);
    if (g === gen) set(landed(draft, key, d));
  } catch (e) {
    if (g !== gen) return;
    if (isDefiniteRefusal(e)) {
      set({ phase: "refused", draft, error: describeDepositError(e), code: e.code });
      return;
    }
    // Anything else — network error, 5xx, 504, 429, a 409 of any code, or a
    // 2xx whose body could not be read — is an unknown outcome: keep the key
    // and ask the server what it holds under it.
    set({
      phase: "unknown",
      key,
      draft,
      error: describeDepositError(e),
      checking: true,
      checkError: null,
    });
    await lookup(key, draft, g, e instanceof ApiError ? e.code : null);
  }
}

async function lookup(key: string, draft: DepositDraft, g: number, cause: string | null) {
  try {
    const d = await getDeposit(key);
    if (g === gen) set(landed(draft, key, d));
  } catch (e) {
    if (g !== gen) return;
    if (e instanceof ApiError && e.status === 404) {
      if (cause === "voided") {
        // The key can never post; the only way forward is a new request.
        const voided = new ApiError(409, "voided", "voided");
        set({ phase: "refused", draft, error: describeDepositError(voided), code: "voided" });
        return;
      }
      set({ phase: "not_found", key, draft, cause: cause ?? "unknown" });
      return;
    }
    const cur = state;
    if (cur.phase === "unknown" && cur.key === key) {
      set({ ...cur, checking: false, checkError: describeError(e) });
    }
  }
}

/** The operator confirmed `draft`: mint the key and send the request. */
export async function submitDeposit(draft: DepositDraft): Promise<void> {
  if (state.phase === "submitting" || state.phase === "unknown" || state.phase === "not_found") {
    return; // an unresolved request must be resolved first
  }
  const g = ++gen;
  let key: string;
  try {
    key = uuidv4();
  } catch (e) {
    set({ phase: "refused", draft, error: describeError(e), code: null });
    return;
  }
  set({ phase: "submitting", key, draft });
  await send(key, draft, g);
}

/** Re-send the unresolved request with the SAME key — always safe: the
 *  server de-duplicates on it, so it can never create a second deposit. */
export async function retryDeposit(): Promise<void> {
  const cur = state;
  if (cur.phase !== "unknown" && cur.phase !== "not_found") return;
  if (cur.phase === "unknown" && cur.checking) return;
  set({ phase: "submitting", key: cur.key, draft: cur.draft });
  await send(cur.key, cur.draft, gen);
}

/** Ask the server what it holds under the unresolved key. */
export async function checkDeposit(): Promise<void> {
  const cur = state;
  if (cur.phase !== "unknown" && cur.phase !== "not_found") return;
  if (cur.phase === "unknown" && cur.checking) return;
  set({
    phase: "unknown",
    key: cur.key,
    draft: cur.draft,
    error: cur.phase === "unknown" ? cur.error : "No deposit was found under this key yet.",
    checking: true,
    checkError: null,
  });
  await lookup(cur.key, cur.draft, gen, cur.phase === "not_found" ? cur.cause : null);
}

/** Re-read a tracked request (polled while it awaits approval). */
export async function refreshTracked(signal?: AbortSignal): Promise<void> {
  const cur = state;
  if (cur.phase !== "tracked") return;
  const g = gen;
  try {
    const d = await getDeposit(cur.deposit.id, signal, true);
    if (g === gen && state.phase === "tracked") set({ ...state, deposit: d, refreshError: null });
  } catch (e) {
    if (g !== gen || state.phase !== "tracked") return;
    if (signal?.aborted || e instanceof IdlePausedError) return;
    set({ ...state, refreshError: describeError(e) });
  }
}

/** Forget the current request (done, refused, or deliberately abandoned). */
export function discardDeposit(): void {
  resetFlow();
}
