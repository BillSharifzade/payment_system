// @vitest-environment happy-dom
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Deposit } from "../api";
import { isAuthed } from "../auth";
import { formatMinor } from "../money";
import {
  ADMIN_ID,
  Call,
  OTHER_ADMIN_ID,
  apiError,
  button,
  click,
  dialog,
  flush,
  json,
  mockFetch,
  mount,
  signIn,
  signOut,
  typeInto,
  unmountAll,
} from "../test-utils";
import { ToastProvider } from "../ui";
import Deposits, { DECISION_ERROR_COPY } from "./Deposits";

function dep(id: string, over: Partial<Deposit> = {}): Deposit {
  return {
    id,
    transaction_id: id,
    status: "pending_approval",
    user_account: "33333333-3333-4333-8333-333333333333",
    amount_minor: 150_000,
    currency: "TJS",
    customer_phone: "992900000077",
    requested_by: OTHER_ADMIN_ID,
    requested_at: "2026-10-08T10:00:00Z",
    decided_by: null,
    decided_at: null,
    reason: null,
    ...over,
  };
}

const MINE = dep("aaaaaaaa-0000-4000-8000-000000000001", {
  requested_by: ADMIN_ID,
  customer_phone: "992900000011",
  amount_minor: 20_000,
});
const THEIRS = dep("bbbbbbbb-0000-4000-8000-000000000002", { customer_phone: "992900000022" });
const LATER = dep("cccccccc-0000-4000-8000-000000000003", { customer_phone: "992900000033" });

type Overrides = { approve?: (req: Call) => Response; reject?: (req: Call) => Response };

function backend(o: Overrides = {}) {
  return mockFetch((req) => {
    if (req.path === "/v1/admin/deposits") {
      const q = new URL(req.url, "http://x").searchParams;
      if (q.get("status") !== "pending_approval") return json(200, { items: [], next_cursor: null });
      return q.get("cursor") === "c1"
        ? json(200, { items: [LATER], next_cursor: null })
        : json(200, { items: [MINE, THEIRS], next_cursor: "c1" });
    }
    if (req.path.endsWith("/approve")) {
      return o.approve ? o.approve(req) : json(200, { ...THEIRS, status: "posted", decided_by: ADMIN_ID });
    }
    if (req.path.endsWith("/reject")) {
      return o.reject
        ? o.reject(req)
        : json(200, { ...THEIRS, status: "rejected", reason: (req.body as { reason: string }).reason });
    }
    if (req.path.startsWith("/v1/admin/deposits/")) return json(200, THEIRS);
    return json(200, {});
  });
}

async function render() {
  const host = await mount(
    <MemoryRouter>
      <ToastProvider>
        <Deposits />
      </ToastProvider>
    </MemoryRouter>,
  );
  await flush();
  return host;
}

function row(host: HTMLElement, phone: string) {
  return Array.from(host.querySelectorAll("tbody tr")).find((r) =>
    (r.textContent ?? "").includes(phone),
  );
}

beforeEach(async () => {
  await signIn(ADMIN_ID);
});

afterEach(async () => {
  await unmountAll();
  await signOut();
  vi.unstubAllGlobals();
});

describe("Approvals queue", () => {
  it("lists pending requests and marks the operator's own", async () => {
    backend();
    const host = await render();
    expect(row(host, MINE.customer_phone!)!.textContent).toContain("you");
    expect(row(host, MINE.customer_phone!)!.textContent).toContain("needs another admin");
    expect(row(host, THEIRS.customer_phone!)!.textContent).toContain(formatMinor(150_000, "TJS"));
  });

  it("pages with the keyset cursor", async () => {
    const calls = backend();
    const host = await render();
    await click(button(host, "Load more"));
    await flush();
    expect(calls.some((c) => c.url.includes("cursor=c1"))).toBe(true);
    expect(row(host, LATER.customer_phone!)).toBeTruthy();
    expect(button(host, "Load more")).toBeNull();
  });

  it("own request: approve is not offered; withdrawing is", async () => {
    backend();
    const host = await render();
    await click(row(host, MINE.customer_phone!));
    const d = dialog()!;
    expect(d.textContent).toContain("This is your own request");
    expect(button(d, "Approve")).toBeNull();
    expect(button(d, "Withdraw request")).not.toBeNull();
  });

  it("the server's dual_control_required is shown clearly and keeps the session", async () => {
    const calls = backend({ approve: () => apiError(403, "dual_control_required") });
    const host = await render();
    await click(row(host, THEIRS.customer_phone!));
    const approve = button(dialog()!, "Approve and credit")!;
    await click(approve); // arm
    expect(approve.textContent).toContain(`Confirm: credit ${formatMinor(150_000, "TJS")}`);
    await click(approve); // fire
    await flush();
    expect(dialog()!.textContent).toContain(DECISION_ERROR_COPY.dual_control_required);
    expect(isAuthed()).toBe(true);
    expect(calls.some((c) => c.path === "/v1/admin/status")).toBe(false);
  });

  it("approving another admin's request posts it", async () => {
    const calls = backend();
    const host = await render();
    await click(row(host, THEIRS.customer_phone!));
    const approve = button(dialog()!, "Approve and credit")!;
    await click(approve);
    await click(approve);
    await flush();
    expect(calls.some((c) => c.method === "POST" && c.path === `/v1/admin/deposits/${THEIRS.id}/approve`)).toBe(true);
    expect(dialog()).toBeNull();
    expect(document.body.textContent).toContain("Approved");
  });

  it("rejecting needs a reason, which is sent", async () => {
    const calls = backend();
    const host = await render();
    await click(row(host, THEIRS.customer_phone!));
    const d = dialog()!;
    const reject = button(d, "Reject")!;
    expect(reject.disabled).toBe(true);
    await typeInto(d.querySelector("input"), "no bank credit");
    expect(reject.disabled).toBe(false);
    await click(reject);
    await flush();
    const sent = calls.find((c) => c.path.endsWith("/reject"))!;
    expect(sent.body).toEqual({ reason: "no bank credit" });
  });

  it("a 409 on approve re-reads the request instead of guessing", async () => {
    const calls = backend({ approve: () => apiError(409, "conflict") });
    const host = await render();
    await click(row(host, THEIRS.customer_phone!));
    const approve = button(dialog()!, "Approve and credit")!;
    await click(approve);
    await click(approve);
    await flush();
    expect(dialog()!.textContent).toContain(DECISION_ERROR_COPY.conflict);
    expect(calls.some((c) => c.method === "GET" && c.path === `/v1/admin/deposits/${THEIRS.id}`)).toBe(true);
  });
});

describe("Track by id", () => {
  it("looks a request up by id", async () => {
    const calls = backend();
    const host = await render();
    await click(button(host, "Track by id"));
    await typeInto(host.querySelector("input.mono"), THEIRS.id);
    await click(button(host, "Look up"));
    await flush();
    expect(calls.some((c) => c.path === `/v1/admin/deposits/${THEIRS.id}`)).toBe(true);
    expect(host.textContent).toContain("awaiting approval");
    expect(host.querySelector('[data-testid="big-amount"]')!.textContent).toBe(
      formatMinor(150_000, "TJS"),
    );
  });
});
