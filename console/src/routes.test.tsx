// @vitest-environment happy-dom
import { act } from "react";
import { Root, createRoot } from "react-dom/client";
import { RouteObject, RouterProvider, createMemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { setAuthed, setSessionNotice } from "./auth";
import { CrashPage, routes } from "./routes";
import { ToastProvider } from "./ui";

// React's act() needs this flag or it warns about un-acted updates.
(globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let host: HTMLDivElement;
let root: Root;

async function mount(table: RouteObject[], path: string) {
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  const router = createMemoryRouter(table, { initialEntries: [path] });
  await act(async () => {
    root.render(
      <ToastProvider>
        <RouterProvider router={router} />
      </ToastProvider>,
    );
  });
  return router;
}

beforeEach(() => {
  setAuthed(false);
  setSessionNotice(null);
  // The shell polls status/metrics on mount; leave those requests pending so
  // nothing here depends on a backend.
  vi.stubGlobal(
    "fetch",
    vi.fn(() => new Promise<Response>(() => {})),
  );
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  host.remove();
  vi.unstubAllGlobals();
});

describe("auth gate", () => {
  it("sends a signed-out visitor to /login", async () => {
    const router = await mount(routes, "/kyc");
    expect(router.state.location.pathname).toBe("/login");
    expect(host.textContent).toContain("Sign in");
  });

  it("keeps a signed-in admin out of /login and renders the shell", async () => {
    setAuthed(true);
    const router = await mount(routes, "/login");
    expect(router.state.location.pathname).toBe("/");
    expect(host.querySelector("nav.topnav")).not.toBeNull();
  });

  it("routes the new Approvals and Terminals pages, with nav entries", async () => {
    setAuthed(true);
    const router = await mount(routes, "/deposits");
    expect(host.textContent).toContain("Deposit approvals");
    expect(host.querySelector('nav.topnav a[href="/deposits"]')).not.toBeNull();
    expect(host.querySelector('nav.topnav a[href="/terminals"]')).not.toBeNull();
    await act(async () => {
      await router.navigate("/terminals");
    });
    expect(host.textContent).toContain("Fingerprint terminals");
  });

  it("tells the operator on the login page that sessions are memory-only", async () => {
    await mount(routes, "/login");
    expect(host.textContent).toMatch(/reloading or closing the tab signs you\s+out/);
  });

  it("drops to /login with the notice when the session ends mid-visit", async () => {
    setAuthed(true);
    const router = await mount(routes, "/users");
    expect(router.state.location.pathname).toBe("/users");
    await act(async () => {
      setSessionNotice("Signed out: no longer an admin, or the account is frozen.");
      setAuthed(false);
    });
    expect(router.state.location.pathname).toBe("/login");
    expect(host.textContent).toContain("no longer an admin");
  });
});

describe("crash page", () => {
  it("shows the message and a Reload button, never the stack", async () => {
    const Boom = () => {
      throw new Error("kaboom at render");
    };
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    await mount([{ path: "/", element: <Boom />, errorElement: <CrashPage /> }], "/");
    expect(host.textContent).toContain("kaboom at render");
    expect(host.querySelector("button")?.textContent).toContain("Reload");
    expect(host.textContent).not.toMatch(/\bat Boom\b|node_modules/);
    expect(spy).toHaveBeenCalled();
    spy.mockRestore();
  });

  it("turns an unknown path into a not-found page with a way home", async () => {
    setAuthed(true);
    await mount(routes, "/nope");
    expect(host.textContent).toContain("Page not found");
  });
});
