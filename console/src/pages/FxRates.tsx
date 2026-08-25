import { FormEvent, useCallback, useEffect, useMemo, useState } from "react";
import { FxRate, getFxRates, setFxRate } from "../api";
import { Ago, Alert, EmptyState, Icon, Select, Skeleton, useToast } from "../ui";

function gcd(a: number, b: number): number {
  return b === 0 ? a : gcd(b, a % b);
}

/** "10.90" → { num: 109, den: 10 } — the exact reduced fraction of the input.
 *  Returns null for anything that isn't a positive decimal. */
function decimalToFraction(s: string): { num: number; den: number } | null {
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

export default function FxRates() {
  const toast = useToast();
  const [rates, setRates] = useState<FxRate[] | null>(null);
  const [base, setBase] = useState("USD");
  const [quote, setQuote] = useState("TJS");
  const [rateStr, setRateStr] = useState("");
  const [alsoInverse, setAlsoInverse] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const reload = useCallback(() => {
    getFxRates()
      .then(setRates)
      .catch((e) => setError(String((e as Error).message ?? e)));
  }, []);
  useEffect(reload, [reload]);

  // Currencies the platform knows about (from configured rates, plus the
  // launch pair) — the pickers never need free-text codes.
  const currencies = useMemo(() => {
    const set = new Set(["TJS", "USD"]);
    for (const r of rates ?? []) {
      set.add(r.base);
      set.add(r.quote);
    }
    return [...set].sort();
  }, [rates]);

  const frac = decimalToFraction(rateStr);
  const valid = frac !== null && base !== quote;

  const missingInverse = (rates ?? []).filter(
    (r) => !(rates ?? []).some((o) => o.base === r.quote && o.quote === r.base),
  );

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (!valid || !frac) return;
    setBusy(true);
    setError(null);
    try {
      await setFxRate(base, quote, frac.num, frac.den);
      if (alsoInverse) {
        await setFxRate(quote, base, frac.den, frac.num);
      }
      toast(
        "success",
        alsoInverse ? `${base}↔${quote} rates updated` : `${base}→${quote} rate updated`,
      );
      setRateStr("");
      reload();
    } catch (err) {
      setError(String((err as Error).message ?? err));
    } finally {
      setBusy(false);
    }
  }

  function editRate(r: FxRate) {
    setBase(r.base);
    setQuote(r.quote);
    setRateStr((r.rate_num / r.rate_den).toFixed(6).replace(/\.?0+$/, ""));
    window.scrollTo({ top: document.body.scrollHeight, behavior: "smooth" });
  }

  return (
    <>
      <header className="page-head">
        <div>
          <h1>FX rates</h1>
          <div className="sub">
            What one unit of a currency buys. Conversions floor to whole minor units; the
            rounding remainder stays in the platform FX position. Each direction is its own
            rate — keep both sides of a pair set.
          </div>
        </div>
      </header>

      {error && <Alert kind="error">{error}</Alert>}
      {missingInverse.length > 0 && (
        <Alert kind="warning" title="Missing reverse rates">
          {missingInverse.map((r) => `${r.quote}→${r.base}`).join(", ")} —
          conversions in that direction will be refused until set.
        </Alert>
      )}

      {rates === null ? (
        <div className="fx-cards">
          {[0, 1].map((i) => (
            <div className="fx-card" key={i}>
              <Skeleton w={90} h={12} />
              <div style={{ height: 10 }} />
              <Skeleton w={170} h={24} />
            </div>
          ))}
        </div>
      ) : rates.length === 0 ? (
        <div className="panel">
          <EmptyState
            icon="exchange"
            title="No rates configured"
            hint="FX conversions and non-TJS AML screening both need a rate."
          />
        </div>
      ) : (
        <div className="fx-cards">
          {rates.map((r) => (
            <div className="fx-card" key={`${r.base}-${r.quote}`}>
              <div className="fx-pair">
                <span>
                  {r.base} → {r.quote}
                </span>
                <button className="quiet" onClick={() => editRate(r)}>
                  Edit
                </button>
              </div>
              <div className="fx-rate">
                1 {r.base} = {(r.rate_num / r.rate_den).toFixed(4)}{" "}
                <span className="fx-unit">{r.quote}</span>
              </div>
              <div className="fx-meta">
                stored as {r.rate_num.toLocaleString()} ⁄ {r.rate_den.toLocaleString()} ·
                updated <Ago ms={r.updated_at_ms} />
              </div>
            </div>
          ))}
        </div>
      )}

      <form className="panel" onSubmit={submit}>
        <div className="panel-title">
          <Icon name="exchange" size={13} />
          Set a rate
        </div>
        <div className="row" style={{ alignItems: "flex-end" }}>
          <label className="field">
            Currency
            <Select
              width={110}
              value={base}
              onChange={setBase}
              options={currencies.map((c) => ({ value: c, label: c }))}
            />
          </label>
          <label className="field">
            Converts to
            <Select
              width={110}
              value={quote}
              onChange={setQuote}
              options={currencies
                .filter((c) => c !== base)
                .map((c) => ({ value: c, label: c }))}
            />
          </label>
          <label className="field">
            1 {base} equals
            <div className="row" style={{ flexWrap: "nowrap" }}>
              <input
                placeholder="10.90"
                inputMode="decimal"
                value={rateStr}
                onChange={(e) => setRateStr(e.target.value)}
                style={{ width: 140 }}
              />
              <span className="muted">{quote}</span>
            </div>
          </label>
          <button className="primary" disabled={busy || !valid}>
            {busy ? "Saving…" : "Set rate"}
          </button>
        </div>
        <label
          className="row small"
          style={{ marginTop: "0.7rem", cursor: "pointer", color: "var(--text-2)" }}
        >
          <input
            type="checkbox"
            checked={alsoInverse}
            onChange={(e) => setAlsoInverse(e.target.checked)}
            style={{ width: "auto" }}
          />
          Also set the reverse rate ({quote} → {base}) as the exact inverse
        </label>
        {frac && base !== quote && (
          <div className="muted small" style={{ marginTop: "0.55rem" }}>
            Will store: 1 {base} = {(frac.num / frac.den).toFixed(6)} {quote} (
            {frac.num.toLocaleString()} ⁄ {frac.den.toLocaleString()})
            {alsoInverse && (
              <>
                {" "}
                and 1 {quote} = {(frac.den / frac.num).toFixed(6)} {base} (
                {frac.den.toLocaleString()} ⁄ {frac.num.toLocaleString()})
              </>
            )}
          </div>
        )}
        {rateStr && !frac && (
          <div className="bad small" style={{ marginTop: "0.55rem" }}>
            Enter a positive decimal, e.g. 10.90 (up to 8 decimal places).
          </div>
        )}
      </form>
    </>
  );
}
