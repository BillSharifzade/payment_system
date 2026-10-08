// Test helpers shared by the *.test.ts(x) files: a scriptable fetch mock, a
// signed-in session, and a few DOM drivers for react-dom + happy-dom (no
// testing-library dependency — the console's tests stay on vitest alone).

import { ReactNode, act } from "react";
import { Root, createRoot } from "react-dom/client";
import { vi } from "vitest";
import { login, logout } from "./api";

export type Call = {
  url: string;
  path: string;
  method: string;
  headers: Record<string, string>;
  body: unknown;
};

export type Handler = (req: Call) => Response | Promise<Response>;

export function json(status: number, body: unknown, headers: Record<string, string> = {}) {
  return new Response(status === 204 ? null : JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...headers },
  });
}

export function apiError(status: number, code: string, headers: Record<string, string> = {}) {
  return json(status, { error: { code, message: `${code} (server text)`, request_id: "req-42" } }, headers);
}

/** Replace global fetch with `handler`; returns the live list of calls. */
export function mockFetch(handler: Handler): Call[] {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init: RequestInit = {}) => {
      const url = String(input);
      const headers: Record<string, string> = {};
      for (const [k, v] of Object.entries((init.headers as Record<string, string>) ?? {})) {
        headers[k.toLowerCase()] = v;
      }
      let body: unknown = init.body;
      if (typeof init.body === "string") {
        try {
          body = JSON.parse(init.body);
        } catch {
          /* keep raw */
        }
      }
      const call: Call = {
        url,
        path: url.split("?")[0],
        method: (init.method ?? "GET").toUpperCase(),
        headers,
        body,
      };
      calls.push(call);
      if (init.signal?.aborted) throw new DOMException("aborted", "AbortError");
      return handler(call);
    }),
  );
  return calls;
}

export const ADMIN_ID = "11111111-1111-4111-8111-111111111111";
export const OTHER_ADMIN_ID = "22222222-2222-4222-8222-222222222222";

export const STATUS_BODY = {
  latest_checkpoint: null,
  unsealed_transactions: 0,
  conservation: [],
};

/** Sign in through the real login() with a mocked backend. */
export async function signIn(adminId = ADMIN_ID, tokens = { access: "a1", refresh: "r1" }) {
  mockFetch((req) => {
    if (req.path === "/v1/auth/login") {
      return json(200, {
        user_id: adminId,
        access_token: tokens.access,
        refresh_token: tokens.refresh,
        token_type: "Bearer",
        expires_in: 900,
      });
    }
    if (req.path === "/v1/admin/status") return json(200, STATUS_BODY);
    throw new Error(`unexpected ${req.method} ${req.url}`);
  });
  await login("992900000001", "correct horse");
}

/** Sign out (swallowing the revoke call) so module state never leaks between tests. */
export async function signOut() {
  mockFetch(() => json(204, null));
  await logout();
}

export function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

export const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

// ----- DOM drivers -----------------------------------------------------------------

(globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let mounted: { host: HTMLElement; root: Root }[] = [];

export async function mount(ui: ReactNode): Promise<HTMLElement> {
  const host = document.createElement("div");
  document.body.appendChild(host);
  const root = createRoot(host);
  mounted.push({ host, root });
  await act(async () => {
    root.render(ui);
  });
  return host;
}

export async function unmountAll() {
  for (const { host, root } of mounted) {
    await act(async () => {
      root.unmount();
    });
    host.remove();
  }
  mounted = [];
  document.body.innerHTML = "";
}

/** Let pending promises (mocked fetches) settle and React re-render. */
export async function flush(ms = 0) {
  await act(async () => {
    await sleep(ms);
  });
}

export async function click(el: Element | null | undefined) {
  if (!el) throw new Error("click: element not found");
  await act(async () => {
    (el as HTMLElement).click();
  });
}

/** Set a React-controlled input's value the way a user typing would. */
export async function typeInto(el: Element | null | undefined, value: string) {
  if (!el) throw new Error("typeInto: element not found");
  const input = el as HTMLInputElement;
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
    setter.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

export async function press(el: Element | null | undefined, key: string) {
  if (!el) throw new Error("press: element not found");
  await act(async () => {
    el.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
  });
}

/** The first button whose text contains `text`. */
export function button(root: ParentNode, text: string | RegExp): HTMLButtonElement | null {
  const all = Array.from(root.querySelectorAll("button"));
  return (
    all.find((b) =>
      typeof text === "string" ? (b.textContent ?? "").includes(text) : text.test(b.textContent ?? ""),
    ) ?? null
  );
}

export function dialog(): HTMLElement | null {
  return document.querySelector('[role="dialog"], [role="alertdialog"]');
}

/** Pick a user through the real UserSearch combobox (debounced server search). */
export async function pickUserViaSearch(root: ParentNode, phone: string) {
  const input = root.querySelector('input[role="combobox"]');
  await typeInto(input, phone);
  await flush(300); // past the 220 ms debounce + the mocked list call
  const option = Array.from(document.querySelectorAll('[role="option"]')).find((o) =>
    (o.textContent ?? "").includes(phone),
  );
  await click(option);
  await flush();
}
