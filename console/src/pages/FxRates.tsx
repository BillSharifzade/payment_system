import { useEffect, useMemo, useState } from "react";
import { FxRate, describeError, getFxRates, isAbort, setFxRate } from "../api";
import { describeRateChange, formatRate, parseRate, rateToInput } from "../money";
import { Ago, Alert, ConfirmButton, EmptyState, Icon, Select, Skeleton, useToast } from "../ui";

export default function FxRates() {
  const toast = useToast();
  const [rates, setRates] = useState<FxRate[] | null>(null);
  const [gen, setGen] = useState(0);
  const [base, setBase] = useState("USD");
  const [quote, setQuote] = useState("TJS");
  const [rateStr, setRateStr] = useState("");
  const [alsoInverse, setAlsoInverse] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const ac = new AbortController();
    getFxRates(ac.signal)
      .then(setRates)
      .catch((e) => {
        if (!isAbort(e)) setError(describeError(e));
      });
    return () => ac.abort();
  }, [gen]);
  const reload = () => setGen((g) => g + 1);

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

  // Exact num/den from the operator's text (decimal or "a/b"), integer math only.
  const frac = parseRate(rateStr);
  const valid = frac !== null && base !== quote;

  const missingInverse = (rates ?? []).filter(
    (r) => !(rates ?? []).some((o) => o.base === r.quote && o.quote === r.base),
  );

  // What the confirm click will replace — shown as a delta so a fat-fingered
  // 109.0 instead of 10.90 is obvious before it goes live.
  const current = (rates ?? []).find((r) => r.base === base && r.quote === quote) ?? null;
  const confirmLabel = frac
    ? `Confirm ${describeRateChange(
        current ? { num: current.rate_num, den: current.rate_den } : null,
        frac,
      )}?`
    : "Confirm?";

  async function submit() {
    if (!valid || !frac || busy) return;
    setBusy(true);
    setError(null);
    try {
      // One request: with alsoInverse the backend upserts both directions in
      // a single transaction, so the pair can never be left half-updated.
      await setFxRate(base, quote, frac.num, frac.den, alsoInverse);
      toast(
        "success",
        alsoInverse ? `${base}↔${quote} rates updated` : `${base}→${quote} rate updated`,
      );
      setRateStr("");
    } catch (err) {
      setError(describeError(err));
    } finally {
      setBusy(false);
      reload(); // whatever happened, show what the server actually holds
    }
  }

  function editRate(r: FxRate) {
    setBase(r.base);
    setQuote(r.quote);
    // The exact stored value: a terminating decimal when there is one, else
    // the fraction itself ("1/3") — never a rounded float that would lose
    // precision when saved back.
    setRateStr(rateToInput({ num: r.rate_num, den: r.rate_den }));
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
              <div className="gap-10" />
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
                1 {r.base} = {formatRate({ num: r.rate_num, den: r.rate_den })}{" "}
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

      {/* Enter never submits: a live rate needs the two-click confirmation. */}
      <form className="panel" onSubmit={(e) => e.preventDefault()}>
        <div className="panel-title">
          <Icon name="exchange" size={13} />
          Set a rate
        </div>
        <div className="row align-bottom">
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
            <div className="row nowrap">
              <input
                className="rate-input"
                placeholder="10.90 or 1/3"
                value={rateStr}
                onChange={(e) => setRateStr(e.target.value)}
              />
              <span className="muted">{quote}</span>
            </div>
          </label>
          <ConfirmButton
            className="primary"
            disabled={busy || !valid}
            confirmLabel={confirmLabel}
            onConfirm={() => void submit()}
          >
            {busy ? "Saving…" : "Set rate"}
          </ConfirmButton>
        </div>
        <label className="check-row small">
          <input
            type="checkbox"
            checked={alsoInverse}
            onChange={(e) => setAlsoInverse(e.target.checked)}
          />
          Also set the reverse rate ({quote} → {base}) as the exact inverse — both written in
          one transaction
        </label>
        {frac && base !== quote && (
          <div className="muted small mt-055">
            Will store the exact fraction {frac.num.toLocaleString()} ⁄{" "}
            {frac.den.toLocaleString()} (1 {base} = {formatRate(frac, 6)} {quote})
            {alsoInverse && (
              <>
                {" "}
                and its inverse {frac.den.toLocaleString()} ⁄ {frac.num.toLocaleString()} (1{" "}
                {quote} = {formatRate({ num: frac.den, den: frac.num }, 6)} {base})
              </>
            )}
            {current && (
              <>
                {" "}
                · currently {formatRate({ num: current.rate_num, den: current.rate_den })}
              </>
            )}
          </div>
        )}
        {rateStr && !frac && (
          <div className="bad small mt-055">
            Enter a positive decimal, e.g. 10.90 (up to 8 decimal places), or an exact fraction
            such as 1/3.
          </div>
        )}
      </form>
    </>
  );
}
