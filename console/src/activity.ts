// When did a human last touch this tab? Only real input (keyboard, pointer,
// wheel, touch) moves the clock — background polling, timers and network
// responses never do. Both the idle sign-out (idle.tsx) and the API client's
// refresh gate (api.ts) read it; it imports nothing so neither can form a cycle.

let lastInput = Date.now();

/** Record user input now. Cheap enough to call from every pointermove. */
export function noteActivity(at: number = Date.now()): void {
  if (at > lastInput) lastInput = at;
}

/** Epoch ms of the last user input (or of module load / sign-in). */
export function lastActivityAt(): number {
  return lastInput;
}

/** Reset the clock — called when a session starts so a fresh sign-in is never
 *  judged against input from before it. */
export function resetActivity(at: number = Date.now()): void {
  lastInput = at;
}

/** The DOM events that count as "the operator is here". */
export const ACTIVITY_EVENTS = [
  "keydown",
  "pointerdown",
  "pointermove",
  "wheel",
  "touchstart",
] as const;
