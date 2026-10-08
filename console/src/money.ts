// Pure money and rate helpers shared by pages and covered by unit tests. No
// DOM, no network — anything here must stay trivially testable in Node.
//
// Amounts are integer minor units end to end (FRONTEND.md §2.1) and FX rates
// are exact num/den fractions end to end: parsing and formatting both use
// integer (BigInt) arithmetic. Floats appear only where the result is purely
// visual and never fed back (chart scaling in charts.tsx).

/** Parse an operator-typed amount ("1 500,50" or "1500.50") into minor units.
 *  Integer string arithmetic only — never a float multiply — so "0.29" is 29,
 *  not 28.999…; returns null for anything that isn't a positive amount with at
 *  most two decimals. Group separators other than spaces are refused rather
 *  than guessed: "1,500.50" and "1.500" are ambiguous across locales. */
export function toMinor(raw: string): number | null {
  const cleaned = raw.replace(/\s/g, "").replace(",", ".");
  if (!/^\d+(\.\d{0,2})?$/.test(cleaned)) return null;
  const [whole, frac = ""] = cleaned.split(".");
  const minor = BigInt(whole) * 100n + BigInt((frac + "00").slice(0, 2));
  if (minor <= 0n || minor > BigInt(Number.MAX_SAFE_INTEGER)) return null;
  return Number(minor);
}

/** Minor units → the plain text an amount input holds ("1500", "1500.5" →
 *  "1500.50"): no grouping, "." decimal, so toMinor() reads it back exactly. */
export function minorToInput(minor: number): string {
  const whole = Math.trunc(minor / 100);
  const cents = minor % 100;
  return cents === 0 ? String(whole) : `${whole}.${String(cents).padStart(2, "0")}`;
}

/** The locale's decimal separator ("." in en, "," in de/tr/ru). */
function decimalSeparator(locale?: string): string {
  const part = new Intl.NumberFormat(locale, { minimumFractionDigits: 1 })
    .formatToParts(1.5)
    .find((p) => p.type === "decimal");
  return part?.value ?? ".";
}

/** 123456 → "1,234.56 TJS" (en), "1.234,56 TJS" (de/tr), "1 234,56 TJS" (ru).
 *  Grouping AND the decimal mark both come from the same locale; the integer
 *  part is grouped as a BigInt, so no float ever touches the amount.
 *  Two-decimal currencies only, which is all the platform has. */
export function formatMinor(minor: number, currency: string, locale?: string): string {
  const abs = BigInt(Math.abs(minor));
  const major = new Intl.NumberFormat(locale, { useGrouping: true }).format(abs / 100n);
  const cents = String(abs % 100n).padStart(2, "0");
  return `${minor < 0 ? "-" : ""}${major}${decimalSeparator(locale)}${cents} ${currency}`;
}

// ----- Exact rate fractions --------------------------------------------------------

export type Fraction = { num: number; den: number };

const MAX_SAFE = BigInt(Number.MAX_SAFE_INTEGER);

function bgcd(a: bigint, b: bigint): bigint {
  while (b !== 0n) [a, b] = [b, a % b];
  return a;
}

/** Reduce a positive BigInt fraction; null if either side won't survive JSON
 *  as an exact number (the API takes plain integers). */
function reduced(num: bigint, den: bigint): Fraction | null {
  if (num <= 0n || den <= 0n) return null;
  const g = bgcd(num, den);
  const n = num / g;
  const d = den / g;
  if (n > MAX_SAFE || d > MAX_SAFE) return null;
  return { num: Number(n), den: Number(d) };
}

/** "10.90" → { num: 109, den: 10 } — the exact reduced fraction of the input.
 *  Returns null for anything that isn't a positive decimal (≤ 8 places). */
export function decimalToFraction(s: string): Fraction | null {
  const m = s.trim().match(/^(\d+)(?:\.(\d{1,8}))?$/);
  if (!m) return null;
  const frac = m[2] ?? "";
  return reduced(BigInt(m[1] + frac), 10n ** BigInt(frac.length));
}

/** A rate as typed by the operator: a decimal ("10.90", ≤ 8 places) or an
 *  exact fraction ("1/3", "109 / 10"). Both parse with integer math only. */
export function parseRate(s: string): Fraction | null {
  const f = s.trim().match(/^(\d+)\s*\/\s*(\d+)$/);
  if (f) return reduced(BigInt(f[1]), BigInt(f[2]));
  return decimalToFraction(s);
}

/** The exact decimal text of a fraction if it terminates within `maxPlaces`
 *  ("109/10" → "10.9"), else null ("1/3"). */
export function exactDecimal(f: Fraction, maxPlaces = 8): string | null {
  const num = BigInt(f.num);
  const den = BigInt(f.den);
  for (let places = 0; places <= maxPlaces; places++) {
    const scaled = num * 10n ** BigInt(places);
    if (scaled % den === 0n) {
      const digits = (scaled / den).toString().padStart(places + 1, "0");
      return places === 0
        ? digits
        : `${digits.slice(0, -places)}.${digits.slice(-places)}`;
    }
  }
  return null;
}

/** What the rate input should hold to edit a stored rate WITHOUT losing a
 *  digit: the exact decimal when one exists, otherwise the fraction itself
 *  ("1/3"). Never a rounded float. */
export function rateToInput(f: Fraction): string {
  return exactDecimal(f) ?? `${f.num}/${f.den}`;
}

/** A positive fraction rounded half-up to `places` decimals, for display
 *  ("1/3", 4 → "0.3333"). Integer math only. */
export function fractionToFixed(f: Fraction, places: number): string {
  const scale = 10n ** BigInt(places);
  const num = BigInt(f.num) * scale;
  const den = BigInt(f.den);
  const q = (num * 2n + den) / (den * 2n); // round half up
  const digits = q.toString().padStart(places + 1, "0");
  return places === 0 ? digits : `${digits.slice(0, -places)}.${digits.slice(-places)}`;
}

/** Display text for a rate: exact when it terminates within `places`, else
 *  prefixed with "≈" so nobody mistakes a rounded figure for the stored one. */
export function formatRate(f: Fraction, places = 4): string {
  const exact = exactDecimal(f, places);
  if (exact !== null) {
    const [w, d = ""] = exact.split(".");
    return `${w}.${d.padEnd(places, "0")}`;
  }
  return `≈${fractionToFixed(f, places)}`;
}

export function fractionsEqual(a: Fraction, b: Fraction): boolean {
  return BigInt(a.num) * BigInt(b.den) === BigInt(b.num) * BigInt(a.den);
}

/** (next − prev) / prev × 100, rounded half away from zero to 2 places, exact. */
function percentChange(prev: Fraction, next: Fraction): string {
  // next/prev − 1 = (nn·pd − pn·nd) / (pn·nd)
  const diff = BigInt(next.num) * BigInt(prev.den) - BigInt(prev.num) * BigInt(next.den);
  const base = BigInt(prev.num) * BigInt(next.den);
  const neg = diff < 0n;
  const mag = neg ? -diff : diff;
  const hundredths = (mag * 10000n * 2n + base) / (base * 2n); // % with 2 decimals, half-up
  const s = hundredths.toString().padStart(3, "0");
  return `${neg ? "-" : "+"}${s.slice(0, -2)}.${s.slice(-2)}`;
}

/** Text for a rate-change confirmation: "10.9000 → 11.0000 (+0.92%)",
 *  "10.9000 (unchanged)", or "new rate 11.0000" when nothing was set before. */
export function describeRateChange(prev: Fraction | null, next: Fraction): string {
  const to = formatRate(next);
  if (!prev) return `new rate ${to}`;
  if (fractionsEqual(prev, next)) return `${to} (unchanged)`;
  return `${formatRate(prev)} → ${to} (${percentChange(prev, next)}%)`;
}
