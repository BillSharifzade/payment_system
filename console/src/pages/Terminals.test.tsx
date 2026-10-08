// @vitest-environment happy-dom
import { act } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AdminUser, Terminal } from "../api";
import {
  Call,
  button,
  click,
  dialog,
  flush,
  json,
  mockFetch,
  mount,
  pickUserViaSearch,
  press,
  signIn,
  signOut,
  typeInto,
  unmountAll,
} from "../test-utils";
import { ToastProvider } from "../ui";
import Terminals from "./Terminals";

const SECRET = "tk_live_5f3c9a1e7b2d4c6f8a0b1c2d3e4f5a6b";
const MERCHANT: AdminUser = {
  id: "55555555-5555-4555-8555-555555555555",
  phone: "992900000055",
  status: "active",
  kyc_level: 2,
  is_admin: false,
  created_at_ms: 0,
  blocked_reason: null,
  wallets: [],
};
const EXISTING: Terminal = {
  id: "66666666-6666-4666-8666-666666666666",
  merchant_user_id: MERCHANT.id,
  label: "Till 1",
  created_at: "2026-10-01T08:00:00Z",
  revoked_at: null,
  last_used_at: null,
};

function backend() {
  let terminals: Terminal[] = [EXISTING];
  const calls = mockFetch((req: Call) => {
    if (req.path === "/v1/admin/users/list") {
      return json(200, { users: [{ ...MERCHANT, is_blocked: false }], next_cursor: null });
    }
    if (req.path === "/v1/admin/users") return json(200, MERCHANT);
    if (req.path === "/v1/admin/terminals" && req.method === "GET") {
      return json(200, { items: terminals });
    }
    if (req.path === "/v1/admin/terminals" && req.method === "POST") {
      const t: Terminal = {
        id: "77777777-7777-4777-8777-777777777777",
        merchant_user_id: MERCHANT.id,
        label: (req.body as { label: string }).label,
        created_at: "2026-10-08T10:00:00Z",
        revoked_at: null,
        last_used_at: null,
      };
      terminals = [...terminals, t];
      return json(201, { ...t, api_key: SECRET });
    }
    if (req.path.endsWith("/revoke")) {
      const id = req.path.split("/")[4];
      terminals = terminals.map((t) => (t.id === id ? { ...t, revoked_at: "2026-10-08T11:00:00Z" } : t));
      return json(200, terminals.find((t) => t.id === id));
    }
    throw new Error(`unexpected ${req.method} ${req.url}`);
  });
  return calls;
}

async function withMerchant() {
  const calls = backend();
  const host = await mount(
    <ToastProvider>
      <Terminals />
    </ToastProvider>,
  );
  await pickUserViaSearch(host, MERCHANT.phone);
  await flush();
  return { calls, host };
}

beforeEach(async () => {
  await signIn();
});

afterEach(async () => {
  await unmountAll();
  await signOut();
  vi.unstubAllGlobals();
});

describe("Terminals", () => {
  it("lists the merchant's terminals with last use", async () => {
    const { calls, host } = await withMerchant();
    expect(calls.some((c) => c.url === `/v1/admin/terminals?merchant_user_id=${MERCHANT.id}`)).toBe(true);
    const r = host.querySelector("tbody tr")!;
    expect(r.textContent).toContain("Till 1");
    expect(r.textContent).toContain("never");
    expect(r.textContent).toContain("active");
  });

  it("shows the API key exactly once, behind an acknowledgement", async () => {
    const { calls, host } = await withMerchant();
    await typeInto(host.querySelector('input[maxlength="64"]'), "Shop 1 — till 2");
    await act(async () => {
      host.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    });
    await flush();
    expect(calls.find((c) => c.method === "POST")!.body).toEqual({
      merchant_user_id: MERCHANT.id,
      label: "Shop 1 — till 2",
    });

    const d = dialog()!;
    expect(d.querySelector<HTMLInputElement>("#terminal-key")!.value).toBe(SECRET);
    // Not dismissible by accident: no close button, Escape does nothing.
    expect(d.querySelector('[aria-label="Close"]')).toBeNull();
    await press(document.body, "Escape");
    expect(dialog()).not.toBeNull();

    const done = button(d, "Done")!;
    expect(done.disabled).toBe(true);
    await click(d.querySelector('input[type="checkbox"]'));
    expect(done.disabled).toBe(false);
    await click(done);
    await flush();

    expect(dialog()).toBeNull();
    // The key is gone from the page; the refreshed list never carries it.
    expect(document.body.innerHTML).not.toContain(SECRET);
    expect(host.textContent).toContain("Shop 1 — till 2");
  });

  it("revokes only after confirmation", async () => {
    const { calls, host } = await withMerchant();
    const revoke = button(host, "Revoke")!;
    await click(revoke);
    expect(calls.some((c) => c.path.endsWith("/revoke"))).toBe(false);
    expect(revoke.textContent).toContain("Confirm revoke “Till 1”?");
    await click(revoke);
    await flush();
    expect(calls.some((c) => c.method === "POST" && c.path === `/v1/admin/terminals/${EXISTING.id}/revoke`)).toBe(true);
    expect(host.querySelector("tbody tr")!.textContent).toContain("revoked");
    expect(button(host, "Revoke")).toBeNull();
  });
});
