// Operator-facing copy for the API's stable machine error codes (FRONTEND.md
// §2.4: screens are driven by `error.code`, never by the server's English
// `message`). Every code in the shared contract §9 has an entry; a test pins
// that, so a new backend code shows up as a failing test rather than as raw
// text on a screen. Pages can override the generic copy where the context
// gives a code a sharper meaning (e.g. `forbidden` on a deposit request).

export const API_ERROR_CODES = [
  "bad_request",
  "not_found",
  "unauthorized",
  "forbidden",
  "conflict",
  "idempotency_conflict",
  "kyc_required",
  "account_blocked",
  "limit_exceeded",
  "insufficient_funds",
  "no_match",
  "ambiguous_match",
  "rate_limited",
  "retry_later",
  "timeout",
  "internal_error",
  "invalid_transaction",
  "currency_mismatch",
  "unknown_account",
  "duplicate_transaction",
  "invalid_amount",
  "unknown_currency",
  "amount_too_large",
  "rejected",
  "voided",
  "dual_control_required",
  "recipient_unavailable",
  "terminal_unauthorized",
  "probe_replayed",
  "check_locked",
] as const;

export type ApiErrorCode = (typeof API_ERROR_CODES)[number];

/** Codes the client itself produces (never sent by the server). */
export type ClientErrorCode = "network" | "refresh_unavailable" | "unknown";

const COPY: Record<ApiErrorCode | ClientErrorCode, string> = {
  bad_request: "The server rejected the request as invalid.",
  not_found: "Not found.",
  unauthorized: "Your session is no longer valid — sign in again.",
  forbidden: "This action is not allowed.",
  conflict:
    "This conflicts with the current state — it may already have been decided by someone else. Reload and check.",
  idempotency_conflict:
    "This request key was already used for a different request. That is a console bug — do not retry; report it with the reference.",
  kyc_required: "The customer's KYC level is too low for this.",
  account_blocked: "The account is blocked.",
  limit_exceeded: "The amount is over the configured limit.",
  insufficient_funds: "Insufficient funds in the source wallet.",
  no_match: "No matching fingerprint was found.",
  ambiguous_match: "More than one person matched the fingerprint — identify the payer by phone.",
  rate_limited: "Too many requests — wait a moment and try again.",
  retry_later: "The service is temporarily unavailable — try again shortly.",
  timeout: "The server timed out before answering. The outcome is unknown.",
  internal_error: "The server hit an internal error.",
  invalid_transaction: "The ledger rejected the transaction as invalid.",
  currency_mismatch: "The wallet's currency does not match the amount's currency.",
  unknown_account: "That wallet does not exist.",
  duplicate_transaction: "A transaction with this id already exists.",
  invalid_amount: "The amount is not valid.",
  unknown_currency: "That currency is not supported.",
  amount_too_large: "The amount is too large.",
  rejected: "Rejected by AML screening.",
  voided: "This request's key was voided — it can never post. Start a new request.",
  dual_control_required:
    "Dual control: you cannot decide on your own request or on anything touching your own account. Another admin must do it.",
  recipient_unavailable: "The recipient's account is not active.",
  terminal_unauthorized:
    "Terminal key missing, revoked, or not registered to this check's merchant.",
  probe_replayed: "That fingerprint probe was already used — capture a fresh one.",
  check_locked: "Too many failed fingerprint attempts — the check was cancelled.",
  network: "Could not reach the server — check the connection and try again.",
  refresh_unavailable:
    "Couldn't renew your session because the server is unreachable or busy. You are still signed in — try again in a moment.",
  unknown: "The server sent an unexpected response.",
};

export function hasCopy(code: string): boolean {
  return Object.prototype.hasOwnProperty.call(COPY, code);
}

/** Copy for `code`, or null for a code this console does not know. */
export function errorCopy(code: string): string | null {
  return hasCopy(code) ? COPY[code as ApiErrorCode] : null;
}
