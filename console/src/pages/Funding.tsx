import { useEffect, useState } from "react";
import {
  AdminUser,
  ApiError,
  Wallet,
  deposit,
  describeError,
  isAbort,
  lookupUser,
  uuidv4,
} from "../api";
import { formatMinor, toMinor } from "../money";
import { Alert, Badge, Icon, IdChip, UserSearch, useToast } from "../ui";

// The console's half of the idempotency contract: the key is created when the
// operator confirms, persisted BEFORE the first request, and reused verbatim on
// retry — so a timeout can never double-fund a wallet.
//
// It lives in localStorage, not sessionStorage: sessionStorage dies with the
// tab, and a tab crash mid-request — after the server had already posted the
// deposit — would leave no banner, inviting the operator to fund it again.
// The record holds no secret (a random key, a wallet id, an amount), so
// surviving the tab is exactly what we want.
const PENDING_KEY = "console-pending-deposit";
const QUICK_AMOUNTS = [100, 500, 1000, 5000];

type Pending = { key: string; wallet: string; amount_minor: number; phone: string };

function readPending(): Pending | null {
  try {
    const raw = localStorage.getItem(PENDING_KEY);
    if (!raw) return null;
    const p = JSON.parse(raw) as Partial<Pending>;
    return typeof p.key === "string" &&
      typeof p.wallet === "string" &&
      typeof p.amount_minor === "number" &&
      typeof p.phone === "string"
      ? (p as Pending)
      : null;
  } catch {
    return null;
  }
}

function writePending(p: Pending | null) {
  try {
    if (p) localStorage.setItem(PENDING_KEY, JSON.stringify(p));
    else localStorage.removeItem(PENDING_KEY);
  } catch {
    /* storage unavailable (private mode / quota) — the in-memory copy still guards this tab */
  }
}

/** The unresolved-deposit banner. Looks up the target wallet's balance so the
 *  operator can see whether the deposit landed before deciding what to do. */
function PendingBanner({
  pending,
  busy,
  checkSeq,
  onRecheck,
  onRetry,
  onDiscard,
}: {
  pending: Pending;
  busy: boolean;
  checkSeq: number;
  onRecheck: () => void;
  onRetry: () => void;
  onDiscard: () => void;
}) {
  const [check, setCheck] = useState<{
    seq: number;
    wallet: Wallet | null;
    error: string | null;
  } | null>(null);

  useEffect(() => {
    const ac = new AbortController();
    lookupUser(pending.phone, ac.signal)
      .then((u) =>
        setCheck({
          seq: checkSeq,
          wallet: u.wallets.find((w) => w.id === pending.wallet) ?? null,
          error: null,
        }),
      )
      .catch((e) => {
        if (!isAbort(e)) setCheck({ seq: checkSeq, wallet: null, error: describeError(e) });
      });
    return () => ac.abort();
  }, [pending.key, pending.phone, pending.wallet, checkSeq]);

  const fresh = check?.seq === checkSeq ? check : null;

  return (
    <Alert kind="warning" title="A deposit is pending confirmation">
      <div className="fund-pending">
        <span>
          {formatMinor(pending.amount_minor, "TJS")} → <IdChip id={pending.wallet} /> for{" "}
          <span className="mono">{pending.phone}</span>. The request was sent but no answer
          arrived, so it may or may not have been posted.
        </span>
        <span>
          Wallet balance now:{" "}
          {fresh === null ? (
            <span className="muted">checking…</span>
          ) : fresh.error ? (
            <span className="bad">unavailable ({fresh.error})</span>
          ) : fresh.wallet ? (
            <strong>{fresh.wallet.display}</strong>
          ) : (
            <span className="bad">wallet not found on this user</span>
          )}
          . Retrying with the same key is always safe — the server de-duplicates on the key, so
          a deposit that already landed is never applied twice.
        </span>
        <div className="row">
          <button className="primary" disabled={busy} onClick={onRetry}>
            <Icon name="refresh" size={14} />
            {busy ? "Retrying…" : "Retry (same key)"}
          </button>
          <button className="quiet" disabled={busy} onClick={onRecheck}>
            Re-check balance
          </button>
          <button className="quiet" disabled={busy} onClick={onDiscard}>
            Discard
          </button>
        </div>
      </div>
    </Alert>
  );
}

export default function Funding() {
  const toast = useToast();
  const [user, setUser] = useState<AdminUser | null>(null);
  const [amount, setAmount] = useState("");
  const [result, setResult] = useState<{ txn: string; minor: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [pending, setPending] = useState<Pending | null>(readPending);
  const [checkSeq, setCheckSeq] = useState(0);

  // Another tab may resolve (or create) the pending record; mirror storage so
  // this tab's banner is never stale.
  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key === null || e.key === PENDING_KEY) setPending(readPending());
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, []);

  const tjsWallet = user?.wallets.find((w) => w.currency === "TJS") ?? null;
  const minor = toMinor(amount);
  const newBalance = tjsWallet && minor ? tjsWallet.balance_minor + minor : null;

  async function pickUser(phone: string) {
    setError(null);
    setResult(null);
    setAmount("");
    try {
      setUser(await lookupUser(phone));
    } catch (err) {
      setUser(null);
      setError(describeError(err));
    }
  }

  async function run(p: Pending) {
    setBusy(true);
    setError(null);
    try {
      const res = await deposit(p.wallet, p.amount_minor, p.key);
      writePending(null);
      setPending(null);
      setResult({ txn: res.transaction_id, minor: p.amount_minor });
      setAmount("");
      toast("success", `Deposited ${formatMinor(p.amount_minor, "TJS")}`);
      // Refresh the balance shown.
      try {
        setUser(await lookupUser(p.phone));
      } catch {
        /* balance refresh is best-effort */
      }
    } catch (err) {
      // A definitive rejection (validation, permissions, limits) will never
      // succeed on retry — drop the key so the operator isn't told to keep
      // retrying a refused request. Timeouts / 429 / 5xx keep the key: outcome
      // unknown, retry the SAME key.
      const definite = err instanceof ApiError && err.status < 500 && err.status !== 429;
      const msg = describeError(err);
      if (definite) {
        writePending(null);
        setPending(null);
        setError(`Deposit rejected: ${msg}`);
      } else {
        setCheckSeq((s) => s + 1); // the banner re-reads the balance
        setError(`${msg} — not lost; use “Retry” above (same key) rather than depositing again.`);
      }
    } finally {
      setBusy(false);
    }
  }

  function submit() {
    if (!tjsWallet || !minor || busy || pending) return;
    const p: Pending = {
      key: uuidv4(),
      wallet: tjsWallet.id,
      amount_minor: minor,
      phone: user!.phone,
    };
    writePending(p);
    setPending(p);
    setResult(null);
    void run(p);
  }

  function discardPending() {
    writePending(null);
    setPending(null);
    setError(null);
  }

  // Why can't we deposit right now? Shown inline so the button is never silently dead.
  const blockReason = pending
    ? "Resolve the pending deposit above first."
    : !user
      ? "Find a customer to fund."
      : !tjsWallet
        ? "This customer has no TJS wallet — deposits are TJS-only today."
        : amount.trim() === ""
          ? "Enter an amount."
          : !minor
            ? "Enter a valid amount (up to 2 decimals)."
            : null;
  const canDeposit = !blockReason && !busy;

  return (
    <>
      <header className="page-head">
        <div>
          <h1>Funding</h1>
          <div className="sub">
            Record money that arrived at the partner bank. One idempotency key per
            confirmed deposit — a retry can never double-fund.
          </div>
        </div>
      </header>

      {pending && (
        <PendingBanner
          pending={pending}
          busy={busy}
          checkSeq={checkSeq}
          onRecheck={() => setCheckSeq((s) => s + 1)}
          onRetry={() => void run(pending)}
          onDiscard={discardPending}
        />
      )}

      {/* Step 1 — customer */}
      <div className="panel">
        <div className="panel-title">1 · Customer</div>
        <div className="row" style={{ alignItems: "center", gap: "0.9rem" }}>
          <UserSearch
            width={340}
            autoFocus
            placeholder="Search phone (e.g. 99290…)"
            onSelect={(u) => void pickUser(u.phone)}
          />
          {user && (
            <span className="row" style={{ gap: "0.45rem", alignItems: "center" }}>
              <span className="mono">{user.phone}</span>
              <Badge tone={user.kyc_level > 0 ? "accent" : "neutral"}>KYC {user.kyc_level}</Badge>
              {user.status !== "active" && <Badge tone="bad">{user.status}</Badge>}
              {user.blocked_reason && <Badge tone="bad">blocked</Badge>}
            </span>
          )}
        </div>
      </div>

      {/* Step 2 — amount */}
      {user && tjsWallet && (
        <div className="panel">
          <div className="panel-title">2 · Amount</div>

          <div className="fund-balance">
            <span className="fund-balance-label">Current TJS balance</span>
            <span className="fund-balance-value">{tjsWallet.display}</span>
          </div>

          <div className="chip-row">
            {QUICK_AMOUNTS.map((v) => (
              <button
                key={v}
                type="button"
                className="amount-chip"
                onClick={() => setAmount(String(v))}
              >
                +{v.toLocaleString()}
              </button>
            ))}
          </div>

          <div className="fund-amount">
            <div className="amount-input-wrap">
              <input
                className="amount-input"
                placeholder="0.00"
                inputMode="decimal"
                autoComplete="off"
                value={amount}
                onChange={(e) => setAmount(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && canDeposit) submit();
                }}
              />
              <span className="amount-suffix">TJS</span>
            </div>

            <button className="primary big" disabled={!canDeposit} onClick={submit}>
              {busy ? "Posting…" : minor ? `Deposit ${formatMinor(minor, "TJS")}` : "Deposit"}
            </button>
          </div>

          {newBalance != null ? (
            <div className="fund-preview">
              New balance <strong>{formatMinor(newBalance, "TJS")}</strong>
              <span className="muted small"> · {minor!.toLocaleString()} minor units</span>
            </div>
          ) : (
            blockReason && <div className="fund-hint muted small">{blockReason}</div>
          )}
        </div>
      )}

      {user && !tjsWallet && (
        <Alert kind="warning">
          {user.phone} has no TJS wallet — non-TJS wallets can’t be funded yet.
        </Alert>
      )}

      {result && (
        <Alert kind="success" title={`Deposited ${formatMinor(result.minor, "TJS")}`}>
          <span className="row">
            transaction <IdChip id={result.txn} />
          </span>
        </Alert>
      )}
      {error && <Alert kind="error">{error}</Alert>}
    </>
  );
}
