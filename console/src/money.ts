// Pure money helpers shared by pages and covered by unit tests. No DOM, no
// network — anything here must stay trivially testable in Node.

/** Parse an operator-typed amount ("1 500,50" or "1500.50") into minor units.
 *  Integer string arithmetic only — never a float multiply — so "0.29" is 29,
 *  not 28.999…; returns null for anything that isn't a positive amount with at
 *  most two decimals. */
export function toMinor(raw: string): number | null {
  const cleaned = raw.replace(/\s/g, "").replace(",", ".");
  if (!/^\d+(\.\d{0,2})?$/.test(cleaned)) return null;
  const [whole, frac = ""] = cleaned.split(".");
  const minor = Number(whole) * 100 + Number((frac + "00").slice(0, 2));
  return Number.isSafeInteger(minor) && minor > 0 ? minor : null;
}

function gcd(a: number, b: number): number {
  return b === 0 ? a : gcd(b, a % b);
}

export type Fraction = { num: number; den: number };

/** "10.90" → { num: 109, den: 10 } — the exact reduced fraction of the input.
 *  Returns null for anything that isn't a positive decimal (≤ 8 places). */
export function decimalToFraction(s: string): Fraction | null {
  const m = s.trim().match(/^(\d+)(?:\.(\d{1,8}))?$/);
  if (!m) return null;
  const whole = m[1];
  const frac = m[2] ?? "";
  const num = Number(whole + frac);
  const den = Math.pow(10, frac.length);
  if (!Number.isSafeInteger(num) || num <= 0) return null;
  const g = gcd(num, den);
  return { num: num / g, den: den / g };
}

/** Text for a rate-change confirmation: "10.9000 → 11.0000 (+0.92%)",
 *  "10.9000 (unchanged)", or "new rate 11.0000" when nothing was set before. */
export function describeRateChange(prev: Fraction | null, next: Fraction): string {
  const to = next.num / next.den;
  if (!prev) return `new rate ${to.toFixed(4)}`;
  const from = prev.num / prev.den;
  if (prev.num * next.den === next.num * prev.den) return `${to.toFixed(4)} (unchanged)`;
  const pct = ((to - from) / from) * 100;
  return `${from.toFixed(4)} → ${to.toFixed(4)} (${pct > 0 ? "+" : ""}${pct.toFixed(2)}%)`;
}

/** 123456 → "1,234.56 TJS" (two-decimal currencies only, which is all we have). */
export function formatMinor(minor: number, currency: string): string {
  const major = Math.trunc(Math.abs(minor) / 100);
  const cents = String(Math.abs(minor) % 100).padStart(2, "0");
  return `${minor < 0 ? "-" : ""}${major.toLocaleString()}.${cents} ${currency}`;
}
