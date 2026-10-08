// @vitest-environment happy-dom
import { act } from "react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AdminUser } from "../api";
import { resetFlow } from "../depositFlow";
import { formatMinor } from "../money";
import {
  ADMIN_ID,
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
import Funding, { CONFIRM_ARM_DELAY_MS } from "./Funding";

const WALLET = "33333333-3333-4333-8333-333333333333";
const CUSTOMER: AdminUser = {
  id: "44444444-4444-4444-8444-444444444444",
  phone: "992900000077",
  status: "active",
  kyc_level: 1,
  is_admin: false,
  created_at_ms: 0,
  blocked_reason: null,
  wallets: [{ id: WALLET, currency: "TJS", balance_minor: 10_000, display: "100.00 TJS" }],
};

function backend(user: AdminUser = CUSTOMER) {
  return mockFetch((req) => {
    if (req.path === "/v1/admin/users/list") {
      return json(200, {
        users: [{ ...user, is_blocked: false }],
        next_cursor: null,
      });
    }
    if (req.path === "/v1/admin/users") return json(200, user);
    if (req.path === "/v1/deposits") {
      const key = req.headers["idempotency-key"];
      return json(202, {
        id: key,
        transaction_id: key,
        status: "pending_approval",
        user_account: WALLET,
        amount_minor: (req.body as { amount_minor: number }).amount_minor,
        currency: "TJS",
        customer_phone: user.phone,
        requested_by: ADMIN_ID,
        requested_at: "2026-10-08T10:00:00Z",
        decided_by: null,
        decided_at: null,
        reason: null,
      });
    }
    return json(200, { items: [], next_cursor: null });
  });
}

const posts = (calls: Call[]) => calls.filter((c) => c.path === "/v1/deposits");

async function render() {
  return mount(
    <MemoryRouter>
      <ToastProvider>
        <Funding />
      </ToastProvider>
    </MemoryRouter>,
  );
}

async function withCustomer(user: AdminUser = CUSTOMER) {
  const calls = backend(user);
  const host = await render();
  await pickUserViaSearch(host, user.phone);
  const amount = host.querySelector<HTMLInputElement>("#fund-amount");
  return { calls, host, amount };
}

beforeEach(async () => {
  await signIn();
  resetFlow();
});

afterEach(async () => {
  await unmountAll();
  await signOut();
  vi.unstubAllGlobals();
});

describe("Funding — money creation UX", () => {
  it("Enter in the amount field never posts and never opens the review", async () => {
    const { calls, host, amount } = await withCustomer();
    await typeInto(amount, "150000");
    await press(amount, "Enter");
    await act(async () => {
      host.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    });
    await flush();
    expect(posts(calls)).toHaveLength(0);
    expect(dialog()).toBeNull();
  });

  it("quick chips ADD to the amount and the result is shown", async () => {
    const { host, amount } = await withCustomer();
    await typeInto(amount, "1000");
    await click(button(host, "+500 TJS"));
    expect(amount!.value).toBe("1500");
    await click(button(host, "+5,000 TJS"));
    expect(amount!.value).toBe("6500");
    expect(host.querySelector("#fund-amount-preview")!.textContent).toContain(
      formatMinor(650_000, "TJS"),
    );
    // From empty, a chip starts at zero.
    await click(button(host, "Clear"));
    await click(button(host, "+100 TJS"));
    expect(amount!.value).toBe("100");
  });

  it("the review dialog shows customer, wallet and the amount in big major units", async () => {
    const { calls, host, amount } = await withCustomer();
    await typeInto(amount, "150000"); // someone meaning 1,500.00 sees 150,000.00 — loudly
    await click(button(host, "Review deposit"));
    const d = dialog()!;
    expect(d).not.toBeNull();
    expect(d.textContent).toContain(CUSTOMER.phone);
    expect(d.textContent).toContain(WALLET);
    expect(d.querySelector('[data-testid="big-amount"]')!.textContent).toBe(
      formatMinor(15_000_000, "TJS"),
    );
    expect(d.textContent).toContain("Large amount");
    // The safe choice has focus; the money button is not armed yet.
    expect(document.activeElement?.textContent).toBe("Cancel");
    expect(button(d, "Request deposit of")!.disabled).toBe(true);
    expect(posts(calls)).toHaveLength(0);
  });

  it("only a deliberate click on the armed confirm button sends the request", async () => {
    const { calls, host, amount } = await withCustomer();
    await typeInto(amount, "1500.50");
    await click(button(host, "Review deposit"));
    const confirm = button(dialog()!, "Request deposit of")!;
    await click(confirm); // a double-click falling through: ignored
    expect(posts(calls)).toHaveLength(0);
    await flush(CONFIRM_ARM_DELAY_MS + 50);
    expect(confirm.disabled).toBe(false);
    await click(confirm);
    await flush();
    const sent = posts(calls);
    expect(sent).toHaveLength(1);
    expect(sent[0].headers["idempotency-key"]).toMatch(/^[0-9a-f-]{36}$/);
    expect(sent[0].body).toEqual({ user_account: WALLET, amount_minor: 150_050, currency: "TJS" });
    expect(dialog()).toBeNull();
    expect(host.textContent).toContain("waiting for a second admin");
    expect(amount!.value).toBe(""); // cleared, so a stray click can't re-send it
  });

  it("Cancel closes the review without sending", async () => {
    const { calls, host, amount } = await withCustomer();
    await typeInto(amount, "10");
    await click(button(host, "Review deposit"));
    await click(button(dialog()!, "Cancel"));
    expect(dialog()).toBeNull();
    expect(posts(calls)).toHaveLength(0);
  });

  it("refuses to review a deposit into the admin's own wallet", async () => {
    const { host, amount } = await withCustomer({ ...CUSTOMER, id: ADMIN_ID });
    await typeInto(amount, "10");
    expect(button(host, "Review deposit")!.disabled).toBe(true);
    expect(host.textContent).toContain("You cannot fund your own wallet.");
  });
});
