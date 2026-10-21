import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Deposit, logout } from "./api";
import { isAuthed } from "./auth";
import {
  DEPOSIT_ERROR_COPY,
  DepositDraft,
  checkDeposit,
  discardDeposit,
  getFlow,
  resetFlow,
  retryDeposit,
  submitDeposit,
} from "./depositFlow";
import {
  ADMIN_ID,
  Call,
  STATUS_BODY,
  apiError,
  deferred,
  json,
  mockFetch,
  signIn,
  signOut,
} from "./test-utils";

const WALLET = "33333333-3333-4333-8333-333333333333";
const draft: DepositDraft = {
  customer_user_id: "44444444-4444-4444-8444-444444444444",
  phone: "992900000077",
  wallet: WALLET,
  currency: "TJS",
  amount_minor: 150_000,
};

function depositFor(key: string, over: Partial<Deposit> = {}): Deposit {
  return {
    id: key,
    transaction_id: key,
    status: "pending_approval",
    user_account: WALLET,
    amount_minor: 150_000,
    currency: "TJS",
    customer_phone: draft.phone,
    requested_by: ADMIN_ID,
    requested_at: "2026-10-08T10:00:00Z",
    decided_by: null,
    decided_at: null,
    reason: null,
    ...over,
  };
}

const posts = (calls: Call[]) => calls.filter((c) => c.method === "POST" && c.path === "/v1/deposits");
const lookups = (calls: Call[]) => calls.filter((c) => c.path.startsWith("/v1/admin/deposits/"));
const keyOf = (c: Call) => c.headers["idempotency-key"];
const lookupKey = (c: Call) => c.path.split("/").pop();

beforeEach(async () => {
  resetFlow();
  await signIn();
});

afterEach(async () => {
  await signOut();
  vi.unstubAllGlobals();
});

describe("request", () => {
  it("202 pending_approval → tracked, with the key sent as Idempotency-Key", async () => {
    const calls = mockFetch((req) => json(202, depositFor(keyOf(req))));
    await submitDeposit(draft);
    const s = getFlow();
    expect(s.phase).toBe("tracked");
    if (s.phase !== "tracked") return;
    expect(s.deposit.status).toBe("pending_approval");
    expect(posts(calls)).toHaveLength(1);
    expect(keyOf(posts(calls)[0])).toBe(s.deposit.id);
    expect(posts(calls)[0].body).toEqual({
      user_account: WALLET,
      amount_minor: 150_000,
      currency: "TJS",
    });
  });

  it("201 posted (dev, dual control off) is tracked as posted", async () => {
    mockFetch((req) => json(201, depositFor(keyOf(req), { status: "posted" })));
    await submitDeposit(draft);
    const s = getFlow();
    expect(s.phase === "tracked" && s.deposit.status).toBe("posted");
  });

  it("every new confirmation mints a fresh key", async () => {
    const calls = mockFetch((req) => json(202, depositFor(keyOf(req))));
    await submitDeposit(draft);
    await submitDeposit(draft);
    expect(new Set(posts(calls).map(keyOf)).size).toBe(2);
  });
});

describe("unknown outcomes keep the key and resolve by lookup", () => {
  it("network error → lookup finds the request → tracked", async () => {
    const calls = mockFetch((req) => {
      if (req.method === "POST") throw new TypeError("Failed to fetch");
      return json(200, depositFor(lookupKey(req)!));
    });
    await submitDeposit(draft);
    expect(getFlow().phase).toBe("tracked");
    expect(lookupKey(lookups(calls)[0])).toBe(keyOf(posts(calls)[0]));
  });

  it("504 timeout → lookup 404 → not_found → retry re-sends the SAME key", async () => {
    let n = 0;
    const calls = mockFetch((req) => {
      if (req.method === "POST") {
        n++;
        return n === 1 ? apiError(504, "timeout") : json(202, depositFor(keyOf(req)));
      }
      return apiError(404, "not_found");
    });
    await submitDeposit(draft);
    const s = getFlow();
    expect(s.phase).toBe("not_found");
    await retryDeposit();
    expect(getFlow().phase).toBe("tracked");
    const [first, second] = posts(calls);
    expect(keyOf(second)).toBe(keyOf(first));
  });

  it("5xx with a failing lookup stays unknown (key kept); a later check resolves it", async () => {
    let lookupOk = false;
    const calls = mockFetch((req) => {
      if (req.method === "POST") return apiError(500, "internal_error");
      return lookupOk ? json(200, depositFor(lookupKey(req)!)) : apiError(503, "retry_later");
    });
    await submitDeposit(draft);
    const s = getFlow();
    expect(s.phase).toBe("unknown");
    if (s.phase !== "unknown") return;
    expect(s.checking).toBe(false);
    expect(s.checkError).toMatch(/temporarily unavailable/);
    const key = s.key;
    lookupOk = true;
    await checkDeposit();
    expect(getFlow().phase).toBe("tracked");
    expect(lookups(calls).every((c) => lookupKey(c) === key)).toBe(true);
  });

  it("429 is not a refusal: it is resolved by lookup", async () => {
    const calls = mockFetch((req) =>
      req.method === "POST" ? apiError(429, "rate_limited") : apiError(404, "not_found"),
    );
    await submitDeposit(draft);
    expect(getFlow().phase).toBe("not_found");
    expect(lookups(calls)).toHaveLength(1);
  });

  it("a 2xx whose body cannot be read is an unknown outcome, not a failure", async () => {
    mockFetch((req) =>
      req.method === "POST"
        ? new Response("<html>proxy</html>", { status: 202 })
        : json(200, depositFor(lookupKey(req)!)),
    );
    await submitDeposit(draft);
    expect(getFlow().phase).toBe("tracked");
  });

  it("while unresolved, a new confirmation is ignored (no second key)", async () => {
    const calls = mockFetch((req) =>
      req.method === "POST" ? apiError(502, "unknown") : apiError(503, "retry_later"),
    );
    await submitDeposit(draft);
    expect(getFlow().phase).toBe("unknown");
    await submitDeposit({ ...draft, amount_minor: 999 });
    expect(posts(calls)).toHaveLength(1);
  });
});

describe("409 is never read as 'definitely failed' without a lookup", () => {
  it("409 conflict → lookup finds our request → tracked", async () => {
    const calls = mockFetch((req) =>
      req.method === "POST" ? apiError(409, "conflict") : json(200, depositFor(lookupKey(req)!)),
    );
    await submitDeposit(draft);
    expect(getFlow().phase).toBe("tracked");
    expect(lookups(calls)).toHaveLength(1);
  });

  it("409 idempotency_conflict with a different request under the key → loud mismatch", async () => {
    mockFetch((req) =>
      req.method === "POST"
        ? apiError(409, "idempotency_conflict")
        : json(200, depositFor(lookupKey(req)!, { amount_minor: 15_000_000 })),
    );
    await submitDeposit(draft);
    const s = getFlow();
    expect(s.phase).toBe("mismatch");
  });

  it("409 voided + nothing under the key → refused (the key can never post)", async () => {
    mockFetch((req) =>
      req.method === "POST" ? apiError(409, "voided") : apiError(404, "not_found"),
    );
    await submitDeposit(draft);
    const s = getFlow();
    expect(s.phase).toBe("refused");
    expect(s.phase === "refused" && s.code).toBe("voided");
  });
});

describe("definite refusals drop the key without a lookup", () => {
  it("422 limit_exceeded", async () => {
    const calls = mockFetch(() => apiError(422, "limit_exceeded"));
    await submitDeposit(draft);
    const s = getFlow();
    expect(s.phase).toBe("refused");
    expect(s.phase === "refused" && s.error).toBe(DEPOSIT_ERROR_COPY.limit_exceeded);
    expect(lookups(calls)).toHaveLength(0);
  });

  it("403 forbidden (own wallet) is refused and does NOT sign the admin out", async () => {
    mockFetch((req) =>
      req.path === "/v1/admin/status" ? json(200, STATUS_BODY) : apiError(403, "forbidden"),
    );
    await submitDeposit(draft);
    const s = getFlow();
    expect(s.phase === "refused" && s.error).toBe(DEPOSIT_ERROR_COPY.forbidden);
    expect(isAuthed()).toBe(true);
  });

  it("a missing CSPRNG refuses before anything is sent", async () => {
    const calls = mockFetch(() => json(202, {}));
    vi.stubGlobal("crypto", undefined);
    await submitDeposit(draft);
    expect(getFlow().phase).toBe("refused");
    expect(posts(calls)).toHaveLength(0);
  });
});

describe("session boundaries", () => {
  it("signing out wipes the request; a late answer is dropped", async () => {
    const gate = deferred<Response>();
    mockFetch((req) => {
      if (req.path === "/v1/auth/logout") return json(204, null);
      return gate.promise;
    });
    const pending = submitDeposit(draft);
    expect(getFlow().phase).toBe("submitting");
    await logout();
    expect(getFlow().phase).toBe("idle");
    gate.resolve(json(202, depositFor("whatever")));
    await pending;
    expect(getFlow().phase).toBe("idle");
  });

  it("discard returns to idle", async () => {
    mockFetch(() => apiError(422, "invalid_amount"));
    await submitDeposit(draft);
    discardDeposit();
    expect(getFlow().phase).toBe("idle");
  });
});
