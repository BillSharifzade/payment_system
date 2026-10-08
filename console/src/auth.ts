// Tiny external store for "is an admin signed in?" plus the notice explaining
// a forced sign-out. Living outside React lets the router be created once at
// module scope (react-router's documented pattern) with the auth gate as a
// layout route that re-renders when this flips, and lets the API client end a
// session (refresh failure, 403 demotion, idle timeout) without holding a
// React callback.

import { useSyncExternalStore } from "react";

type Listener = () => void;
type AuthState = { authed: boolean; notice: string | null; adminId: string | null };

let state: AuthState = { authed: false, notice: null, adminId: null };
const listeners = new Set<Listener>();
// Per-admin client state (e.g. an unresolved deposit) registers here so it is
// wiped whenever a session ends or a different admin signs in.
const sessionResets = new Set<Listener>();

function emit() {
  for (const l of listeners) l();
}

function subscribe(l: Listener): () => void {
  listeners.add(l);
  return () => {
    listeners.delete(l);
  };
}

export function isAuthed(): boolean {
  return state.authed;
}

export function sessionNotice(): string | null {
  return state.notice;
}

/** The signed-in admin's user id (from the login response), or null. Used to
 *  mark "your own request" in dual-control queues — the server still enforces. */
export function currentAdminId(): string | null {
  return state.adminId;
}

/** Register a reset for per-admin client state; runs on every sign-in and
 *  sign-out. Returns an unregister function. */
export function onSessionReset(fn: Listener): () => void {
  sessionResets.add(fn);
  return () => {
    sessionResets.delete(fn);
  };
}

function runResets() {
  for (const r of sessionResets) r();
}

/** Flip the signed-in state. Starting a session clears any stale notice;
 *  either transition wipes per-admin client state. */
export function setAuthed(next: boolean, adminId: string | null = null): void {
  const nextId = next ? adminId : null;
  if (state.authed === next && state.adminId === nextId) return;
  runResets();
  state = { authed: next, notice: next ? null : state.notice, adminId: nextId };
  emit();
}

/** Why the last session ended — shown on the login screen until the next sign-in. */
export function setSessionNotice(msg: string | null): void {
  if (state.notice === msg) return;
  state = { ...state, notice: msg };
  emit();
}

/** React binding: re-renders when the signed-in state changes. */
export function useAuthed(): boolean {
  return useSyncExternalStore(subscribe, isAuthed, isAuthed);
}

export function useSessionNotice(): string | null {
  return useSyncExternalStore(subscribe, sessionNotice, sessionNotice);
}

export function useAdminId(): string | null {
  return useSyncExternalStore(subscribe, currentAdminId, currentAdminId);
}
