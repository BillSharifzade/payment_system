// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { KycSubmission } from "../api";
import { isAuthed } from "../auth";
import {
  ADMIN_ID,
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
  unmountAll,
} from "../test-utils";
import { ToastProvider } from "../ui";
import KycQueue, { KYC_ERROR_COPY } from "./KycQueue";

function sub(id: string, user_id: string, full_name: string): KycSubmission {
  return {
    id,
    user_id,
    phone: `99290000${id.slice(0, 4)}`,
    requested_level: 1,
    full_name,
    document_type: "passport",
    document_ref: `${id}.jpg`,
    status: "pending",
    created_at_ms: Date.parse("2026-10-08T09:00:00Z"),
  };
}

const OWN = sub("1001aaaa", ADMIN_ID, "Admin Self");
const OTHER = sub("2002bbbb", "99999999-9999-4999-8999-999999999999", "Firuz Rahimov");

beforeEach(async () => {
  await signIn(ADMIN_ID);
  if (!URL.createObjectURL) {
    Object.assign(URL, { createObjectURL: () => "blob:doc", revokeObjectURL: () => {} });
  }
  mockFetch((req) => {
    if (req.path === "/v1/admin/kyc/submissions") return json(200, [OWN, OTHER]);
    if (req.path.startsWith("/v1/admin/kyc/documents/")) return new Response(new Blob(["img"]));
    if (req.path.endsWith("/approve")) return apiError(403, "dual_control_required");
    return json(200, {});
  });
});

afterEach(async () => {
  await unmountAll();
  await signOut();
  vi.unstubAllGlobals();
});

async function open(name: string) {
  const host = await mount(
    <ToastProvider>
      <KycQueue />
    </ToastProvider>,
  );
  await flush();
  const row = Array.from(host.querySelectorAll("tbody tr")).find((r) =>
    (r.textContent ?? "").includes(name),
  );
  await click(row);
  await flush();
  return dialog()!;
}

describe("KYC dual control", () => {
  it("your own submission cannot be decided from the console", async () => {
    const d = await open(OWN.full_name);
    expect(d.textContent).toContain("This is your own submission");
    expect(button(d, "Approve level")!.disabled).toBe(true);
    expect(button(d, "Reject")!.disabled).toBe(true);
  });

  it("a server dual_control_required gets clear copy and keeps the session", async () => {
    const d = await open(OTHER.full_name);
    const approve = button(d, "Approve level")!;
    await click(approve);
    await click(approve);
    await flush();
    expect(dialog()!.textContent).toContain(KYC_ERROR_COPY.dual_control_required);
    expect(isAuthed()).toBe(true);
  });
});
