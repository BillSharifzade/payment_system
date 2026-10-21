// Idle sign-out. With tokens in memory only, the remaining way an unattended
// console stays signed in is an operator walking away from an open tab — so
// after IDLE_TIMEOUT_MS without user input the session ends (refresh token
// revoked server-side), with a warning dialog for the last IDLE_WARNING_MS.
//
// Only real input counts (activity.ts). Background polls do not, and the API
// client refuses to rotate tokens for a poll unless there was input since the
// current token was minted — so polling can neither reset this timer nor keep
// the server-side session alive on its own.

import { useEffect, useState } from "react";
import { ACTIVITY_EVENTS, lastActivityAt, noteActivity } from "./activity";
import { logout } from "./api";
import { Icon, Modal } from "./ui";

const DEFAULT_IDLE_MINUTES = 15;

/** Build-time override: VITE_IDLE_TIMEOUT_MINUTES (1–240), else 15. */
function configuredIdleMs(): number {
  const raw = Number(import.meta.env.VITE_IDLE_TIMEOUT_MINUTES);
  const minutes =
    Number.isFinite(raw) && raw >= 1 && raw <= 240 ? raw : DEFAULT_IDLE_MINUTES;
  return minutes * 60_000;
}

export const IDLE_TIMEOUT_MS = configuredIdleMs();
export const IDLE_WARNING_MS = 60_000;

export function idleNotice(timeoutMs: number): string {
  const minutes = Math.round(timeoutMs / 60_000);
  return `Signed out after ${minutes} minute${minutes === 1 ? "" : "s"} without activity.`;
}

/** Watches for input while mounted (i.e. while signed in). Returns the
 *  seconds left once inside the warning window, else null. */
export function useIdleTimeout(
  timeoutMs: number = IDLE_TIMEOUT_MS,
  warningMs: number = IDLE_WARNING_MS,
): number | null {
  const [secondsLeft, setSecondsLeft] = useState<number | null>(null);

  useEffect(() => {
    const onInput = () => noteActivity();
    for (const ev of ACTIVITY_EVENTS) {
      window.addEventListener(ev, onInput, { capture: true, passive: true });
    }
    let done = false;
    const check = () => {
      if (done) return;
      const left = timeoutMs - (Date.now() - lastActivityAt());
      if (left <= 0) {
        done = true;
        setSecondsLeft(null);
        void logout(idleNotice(timeoutMs));
      } else if (left <= warningMs) {
        setSecondsLeft(Math.ceil(left / 1000));
      } else {
        setSecondsLeft(null);
      }
    };
    // Wall-clock based, so a throttled background tab or a laptop waking from
    // sleep is judged correctly on the next tick.
    const t = setInterval(check, 1000);
    document.addEventListener("visibilitychange", check);
    return () => {
      done = true;
      clearInterval(t);
      document.removeEventListener("visibilitychange", check);
      for (const ev of ACTIVITY_EVENTS) {
        window.removeEventListener(ev, onInput, { capture: true });
      }
    };
  }, [timeoutMs, warningMs]);

  return secondsLeft;
}

/** Mounted inside the signed-in shell. */
export function IdleGuard({
  timeoutMs = IDLE_TIMEOUT_MS,
  warningMs = IDLE_WARNING_MS,
}: {
  timeoutMs?: number;
  warningMs?: number;
}) {
  const secondsLeft = useIdleTimeout(timeoutMs, warningMs);
  if (secondsLeft === null) return null;
  // Any input already counts as activity; the button is the obvious target.
  const stay = () => noteActivity();
  return (
    <Modal title="Still there?" onClose={stay} role="alertdialog">
      <div className="idle-warning">
        <Icon name="clock" size={22} />
        <p>
          You will be signed out in <strong>{secondsLeft} s</strong> because there has been no
          activity for a while. Unsaved input on this page will be lost.
        </p>
      </div>
      <div className="row end">
        <button className="quiet" onClick={() => void logout()}>
          Sign out now
        </button>
        <button className="primary" data-autofocus onClick={stay}>
          Stay signed in
        </button>
      </div>
    </Modal>
  );
}
