import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { noteActivity } from "./activity";
import {
  ApiError,
  IdlePausedError,
  NetworkError,
  api,
  describeError,
  hasSession,
  logout,
  refreshPolicy,
  uuidv4,
} from "./api";
import { isAuthed, onSessionReset, sessionNotice, setSessionNotice } from "./auth";
import { API_ERROR_CODES, errorCopy } from "./errors";
import {
  ADMIN_ID,
  STATUS_BODY,
  Call,
  apiError,
  deferred,
  json,
  mockFetch,
  signIn,
  signOut,
} from "./test-utils";

const saved = { ...refreshPolicy };

beforeEach(async () => {
  // Fast, deterministic backoff for the retry tests.
  Object.assign(refreshPolicy, { attempts: 4, baseDelayMs: 1, maxDelayMs: 5 });
  setSessionNotice(null);
  await signIn();
});

afterEach(async () => {
  await signOut();
  Object.assign(refreshPolicy, saved);
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

const bearer = (c: { headers: Record<string, string> }) => c.headers.authorization;
const refreshes = (calls: Call[]) =>
  calls.filter((c) => c.path === "/v1/auth/refresh");
const ROTATED = { user_id: ADMIN_ID, access_token: "a2", refresh_token: "r2" };

describe("login", () => {
  it("keeps tokens in memory only — nothing in web storage", async () => {
    expect(isAuthed()).toBe(true);
    expect(hasSession()).toBe(true);
    // The node test environment has no storage at all; the browser build
    // never touches it either (see purgeLegacyStorage + api.ts header).
    expect(typeof (globalThis as { sessionStorage?: unknown }).sessionStorage).toBe("undefined");
  });
});

describe("single-flight refresh", () => {
  it("concurrent 401s share ONE rotation, then every request replays", async () => {
    const gate = deferred<Response>();
    const calls = mockFetch((req) => {
      if (req.path === "/v1/auth/refresh") return gate.promise;
      return bearer(req) === "Bearer a2" ? json(200, { ok: req.url }) : apiError(401, "unauthorized");
    });
    const all = Promise.all([api("/v1/admin/a"), api("/v1/admin/b"), api("/v1/admin/c")]);
    await vi.waitFor(() => expect(calls.filter((c) => bearer(c) === "Bearer a1")).toHaveLength(3));
    gate.resolve(json(200, ROTATED));
    expect(await all).toEqual([{ ok: "/v1/admin/a" }, { ok: "/v1/admin/b" }, { ok: "/v1/admin/c" }]);
    expect(refreshes(calls)).toHaveLength(1);
    expect(refreshes(calls)[0].body).toEqual({ refresh_token: "r1" });
  });

  it("a 401 that lands after someone else rotated replays without a second refresh", async () => {
    const slow = deferred<Response>();
    const calls = mockFetch((req) => {
      if (req.path === "/v1/auth/refresh") return json(200, ROTATED);
      if (req.path === "/v1/admin/slow" && bearer(req) === "Bearer a1") return slow.promise;
      return bearer(req) === "Bearer a2" ? json(200, { ok: req.path }) : apiError(401, "unauthorized");
    });
    const slowCall = api("/v1/admin/slow"); // sent with a1, answer delayed
    await api("/v1/admin/fast"); // 401 → rotates to a2
    slow.resolve(apiError(401, "unauthorized")); // the old token's 401 arrives late
    expect(await slowCall).toEqual({ ok: "/v1/admin/slow" });
    expect(refreshes(calls)).toHaveLength(1);
  });
});

describe("refresh outcomes", () => {
  it("401 from refresh ends the session: expired", async () => {
    const calls = mockFetch((req) =>
      req.path === "/v1/auth/refresh" ? apiError(401, "unauthorized") : apiError(401, "unauthorized"),
    );
    await expect(api("/v1/admin/metrics")).rejects.toMatchObject({ status: 401 });
    expect(isAuthed()).toBe(false);
    expect(hasSession()).toBe(false);
    expect(sessionNotice()).toBe("Session expired — sign in again.");
    expect(refreshes(calls)).toHaveLength(1); // no retry of a dead token
  });

  it("403 from refresh ends the session: account frozen/closed", async () => {
    mockFetch((req) =>
      req.path === "/v1/auth/refresh" ? apiError(403, "forbidden") : apiError(401, "unauthorized"),
    );
    await expect(api("/v1/admin/metrics")).rejects.toBeInstanceOf(ApiError);
    expect(isAuthed()).toBe(false);
    expect(sessionNotice()).toMatch(/frozen or closed/);
  });

  it("502 during a deploy keeps the session and retries with backoff", async () => {
    let n = 0;
    const calls = mockFetch((req) => {
      if (req.path === "/v1/auth/refresh") {
        n++;
        return n < 3 ? new Response("bad gateway", { status: 502 }) : json(200, ROTATED);
      }
      return bearer(req) === "Bearer a2" ? json(200, { ok: true }) : apiError(401, "unauthorized");
    });
    expect(await api("/v1/admin/metrics")).toEqual({ ok: true });
    expect(isAuthed()).toBe(true);
    expect(refreshes(calls)).toHaveLength(3);
    expect(refreshes(calls).every((c) => (c.body as { refresh_token: string }).refresh_token === "r1")).toBe(true);
  });

  it("429 that never clears keeps the session; the call fails with a retryable error", async () => {
    const calls = mockFetch((req) =>
      req.path === "/v1/auth/refresh"
        ? apiError(429, "rate_limited", { "retry-after": "0" })
        : apiError(401, "unauthorized"),
    );
    const err = await api<never>("/v1/admin/metrics").catch((e: unknown) => e as ApiError);
    expect(err).toBeInstanceOf(ApiError);
    expect(err.code).toBe("refresh_unavailable");
    expect(describeError(err)).toMatch(/still signed in/);
    expect(isAuthed()).toBe(true);
    expect(hasSession()).toBe(true);
    expect(sessionNotice()).toBeNull();
    expect(refreshes(calls)).toHaveLength(refreshPolicy.attempts);
  });

  it("a network failure during refresh keeps the session", async () => {
    let n = 0;
    mockFetch((req) => {
      if (req.path === "/v1/auth/refresh") {
        n++;
        if (n === 1) throw new TypeError("Failed to fetch");
        return json(200, ROTATED);
      }
      return bearer(req) === "Bearer a2" ? json(200, { ok: true }) : apiError(401, "unauthorized");
    });
    expect(await api("/v1/admin/metrics")).toEqual({ ok: true });
    expect(isAuthed()).toBe(true);
  });

  it("a refresh still in flight when the operator signs out cannot resurrect the session", async () => {
    const gate = deferred<Response>();
    const calls = mockFetch((req) => {
      if (req.path === "/v1/auth/refresh") return gate.promise;
      if (req.path === "/v1/auth/logout") return json(204, null);
      return apiError(401, "unauthorized");
    });
    const pending = api("/v1/admin/metrics").catch((e) => e);
    await vi.waitFor(() => expect(refreshes(calls)).toHaveLength(1)); // rotation in flight
    await logout();
    gate.resolve(json(200, ROTATED));
    await pending;
    expect(hasSession()).toBe(false);
    expect(isAuthed()).toBe(false);
    expect(sessionNotice()).toBeNull(); // a deliberate sign-out shows no notice
  });
});

describe("403 handling", () => {
  it("403 forbidden confirmed by the admin gate signs out (demoted mid-session)", async () => {
    const calls = mockFetch((req) => {
      if (req.path === "/v1/admin/status") return apiError(403, "forbidden");
      if (req.path === "/v1/auth/logout") return json(204, null);
      return apiError(403, "forbidden");
    });
    await expect(api("/v1/admin/users?phone=1")).rejects.toMatchObject({ code: "forbidden" });
    expect(isAuthed()).toBe(false);
    expect(sessionNotice()).toMatch(/no longer an admin/);
    // …and the refresh token is revoked server-side.
    await vi.waitFor(() =>
      expect(calls.some((c) => c.path === "/v1/auth/logout")).toBe(true),
    );
  });

  it("403 forbidden about the TARGET (gate still answers) keeps the session", async () => {
    mockFetch((req) => {
      if (req.path === "/v1/admin/status") return json(200, STATUS_BODY);
      return apiError(403, "forbidden"); // e.g. deposit into the caller's own wallet
    });
    await expect(
      api("/v1/deposits", { method: "POST", body: "{}", idempotencyKey: "k" }),
    ).rejects.toMatchObject({ code: "forbidden" });
    expect(isAuthed()).toBe(true);
  });

  it("other 403 codes are about the request, never the session", async () => {
    const calls = mockFetch(() => apiError(403, "dual_control_required"));
    await expect(api("/v1/admin/deposits/x/approve", { method: "POST" })).rejects.toMatchObject({
      code: "dual_control_required",
    });
    expect(isAuthed()).toBe(true);
    expect(calls.some((c) => c.path === "/v1/admin/status")).toBe(false);
  });
});

describe("idle-aware refresh", () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(new Date("2026-10-08T10:00:00Z"));
  });

  it("a background poll never rotates tokens without input since the last rotation", async () => {
    const calls = mockFetch((req) => {
      if (req.path === "/v1/auth/refresh") return json(200, ROTATED);
      return bearer(req) === "Bearer a2" ? json(200, { ok: true }) : apiError(401, "unauthorized");
    });
    // signed in at 10:00 (in beforeEach, before the clock was pinned) — no input since.
    await signIn();
    vi.setSystemTime(new Date("2026-10-08T10:16:00Z"));
    mockFetch((req) => {
      calls.push(req);
      if (req.path === "/v1/auth/refresh") return json(200, ROTATED);
      return bearer(req) === "Bearer a2" ? json(200, { ok: true }) : apiError(401, "unauthorized");
    });
    await expect(api("/v1/admin/status", { background: true })).rejects.toBeInstanceOf(
      IdlePausedError,
    );
    expect(refreshes(calls)).toHaveLength(0);
    expect(isAuthed()).toBe(true); // paused, not signed out — the idle timer decides that

    // The operator moves the mouse: now a poll may rotate.
    noteActivity();
    vi.setSystemTime(new Date("2026-10-08T10:16:01Z"));
    expect(await api("/v1/admin/status", { background: true })).toEqual({ ok: true });
    expect(refreshes(calls)).toHaveLength(1);
  });

  it("an operator action refreshes even without recent input", async () => {
    const calls = mockFetch((req) => {
      if (req.path === "/v1/auth/refresh") return json(200, ROTATED);
      return bearer(req) === "Bearer a2" ? json(200, { ok: true }) : apiError(401, "unauthorized");
    });
    expect(await api("/v1/admin/users?phone=1")).toEqual({ ok: true });
    expect(refreshes(calls)).toHaveLength(1);
  });
});

describe("logout", () => {
  it("forgets the tokens, revokes server-side and wipes per-admin state", async () => {
    const reset = vi.fn();
    const off = onSessionReset(reset);
    const calls = mockFetch(() => json(204, null));
    await logout();
    off();
    expect(hasSession()).toBe(false);
    expect(isAuthed()).toBe(false);
    expect(reset).toHaveBeenCalled();
    expect(calls).toHaveLength(1);
    expect(calls[0]).toMatchObject({ path: "/v1/auth/logout", body: { refresh_token: "r1" } });
  });
});

describe("network errors", () => {
  it("surface as NetworkError with plain copy", async () => {
    mockFetch(() => {
      throw new TypeError("Failed to fetch");
    });
    const err = await api("/v1/admin/metrics").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(NetworkError);
    expect(describeError(err)).toMatch(/Could not reach the server/);
  });
});

describe("uuidv4 (idempotency keys)", () => {
  const V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

  it("uses crypto.randomUUID when available", () => {
    vi.stubGlobal("crypto", { randomUUID: () => "11111111-2222-4333-8444-555555555555" });
    expect(uuidv4()).toBe("11111111-2222-4333-8444-555555555555");
  });

  it("derives a v4 UUID from getRandomValues in an insecure context", () => {
    vi.stubGlobal("crypto", {
      getRandomValues: (b: Uint8Array) => {
        b.fill(0xff);
        return b;
      },
    });
    const id = uuidv4();
    expect(id).toMatch(V4);
  });

  it("fails closed without a CSPRNG — never Math.random", () => {
    const rnd = vi.spyOn(Math, "random");
    vi.stubGlobal("crypto", undefined);
    expect(() => uuidv4()).toThrow(/secure random/);
    vi.stubGlobal("crypto", {});
    expect(() => uuidv4()).toThrow(/secure random/);
    expect(rnd).not.toHaveBeenCalled();
    rnd.mockRestore();
  });
});

describe("error copy", () => {
  it("every contract §9 code has operator copy (never the server's message)", () => {
    for (const code of API_ERROR_CODES) {
      expect(errorCopy(code), code).toBeTruthy();
      const text = describeError(new ApiError(400, code, "server text"));
      if (code !== "bad_request") expect(text, code).not.toContain("server text");
    }
  });

  it("bad_request carries the server detail; 5xx carries the reference", () => {
    expect(describeError(new ApiError(400, "bad_request", "malformed phone number"))).toMatch(
      /Details: malformed phone number/,
    );
    expect(describeError(new ApiError(500, "internal_error", "x", "req-9"))).toMatch(/Ref: req-9/);
    expect(describeError(new ApiError(504, "timeout", "x"))).toMatch(/outcome is unknown/);
  });

  it("unknown codes fall back to a generic line; overrides win", () => {
    expect(describeError(new ApiError(418, "teapot", "x"))).toMatch(/teapot, HTTP 418/);
    expect(describeError(new ApiError(403, "forbidden", "x"), { forbidden: "Own wallet." })).toBe(
      "Own wallet.",
    );
  });
});
