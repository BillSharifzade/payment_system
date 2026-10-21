// @vitest-environment happy-dom
import { act } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api, hasSession } from "./api";
import { isAuthed, sessionNotice, setSessionNotice } from "./auth";
import { IDLE_TIMEOUT_MS, IdleGuard, idleNotice } from "./idle";
import { button, dialog, json, mockFetch, mount, signIn, signOut, unmountAll } from "./test-utils";

const TIMEOUT = 60_000;
const WARNING = 10_000;

async function advance(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

beforeEach(async () => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-10-08T09:00:00Z"));
  setSessionNotice(null);
  await signIn();
});

afterEach(async () => {
  await unmountAll();
  vi.useRealTimers();
  await signOut();
  vi.unstubAllGlobals();
});

describe("idle timeout", () => {
  it("defaults to 15 minutes", () => {
    expect(IDLE_TIMEOUT_MS).toBe(15 * 60_000);
  });

  it("warns shortly before, then signs out and revokes the refresh token", async () => {
    const calls = mockFetch(() => json(204, null));
    await mount(<IdleGuard timeoutMs={TIMEOUT} warningMs={WARNING} />);

    await advance(TIMEOUT - WARNING - 1_000);
    expect(dialog()).toBeNull();

    await advance(2_000);
    const d = dialog();
    expect(d).not.toBeNull();
    expect(d!.getAttribute("role")).toBe("alertdialog");
    expect(d!.textContent).toMatch(/signed out in \d+ s/);
    // Focus lands on the safe action.
    expect(document.activeElement?.textContent).toBe("Stay signed in");

    await advance(WARNING);
    expect(isAuthed()).toBe(false);
    expect(hasSession()).toBe(false);
    expect(sessionNotice()).toBe(idleNotice(TIMEOUT));
    expect(calls.some((c) => c.path === "/v1/auth/logout")).toBe(true);
  });

  it("any user input resets the clock and dismisses the warning", async () => {
    mockFetch(() => json(204, null));
    await mount(<IdleGuard timeoutMs={TIMEOUT} warningMs={WARNING} />);
    await advance(TIMEOUT - WARNING + 1_000);
    expect(dialog()).not.toBeNull();

    await act(async () => {
      button(document, "Stay signed in")!.dispatchEvent(
        new PointerEvent("pointerdown", { bubbles: true }),
      );
    });
    await advance(1_000);
    expect(dialog()).toBeNull();

    // A keypress later keeps it alive past the original deadline.
    await advance(TIMEOUT - WARNING - 5_000);
    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "a" }));
    });
    await advance(TIMEOUT - WARNING - 5_000);
    expect(isAuthed()).toBe(true);
    expect(dialog()).toBeNull();
  });

  it("background polling is not activity: a busy poller still gets signed out", async () => {
    mockFetch((req) =>
      req.path === "/v1/auth/logout" ? json(204, null) : json(200, { ok: true }),
    );
    await mount(<IdleGuard timeoutMs={TIMEOUT} warningMs={WARNING} />);
    for (let t = 0; t < TIMEOUT; t += 5_000) {
      await act(async () => {
        await api("/v1/admin/status", { background: true });
      });
      await advance(5_000);
    }
    await advance(1_000);
    expect(isAuthed()).toBe(false);
    expect(sessionNotice()).toBe(idleNotice(TIMEOUT));
  });
});
