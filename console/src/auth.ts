// Tiny external store for "is an admin signed in?" plus the notice explaining
// a forced sign-out. Living outside React lets the router be created once at
// module scope (react-router's documented pattern) with the auth gate as a
// layout route that re-renders when this flips, and lets the API client end a
// session (refresh failure, 403 demotion) without holding a React callback.

import { useSyncExternalStore } from "react";

type Listener = () => void;
type AuthState = { authed: boolean; notice: string | null };

let state: AuthState = { authed: false, notice: null };
const listeners = new Set<Listener>();

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

/** Flip the signed-in state. Starting a session clears any stale notice. */
export function setAuthed(next: boolean): void {
  if (state.authed === next) return;
  state = { authed: next, notice: next ? null : state.notice };
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
