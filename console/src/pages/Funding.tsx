import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { AdminUser, ApiError, describeError, lookupUser } from "../api";
import { useAdminId } from "../auth";
import {
  DepositDraft,
  FlowState,
  checkDeposit,
  discardDeposit,
  retryDeposit,
  refreshTracked,
  submitDeposit,
  useDepositFlow,
} from "../depositFlow";
import { BigAmount, DepositDetails, LARGE_DEPOSIT_MINOR } from "../depositViews";
import { formatMinor, minorToInput, toMinor } from "../money";
import { pendingLabel, useOps } from "../ops";
import { Alert, Badge, ConfirmButton, Icon, IdChip, Modal, UserSearch } from "../ui";

// Funding is the MAKER side of dual control (contract §3): it creates a
// deposit REQUEST; a different admin approves it on the Approvals page before
// any money is credited. The request/idempotency state machine lives in
// depositFlow.ts so it survives navigation and is wiped on sign-out.

/** Quick-add chips, in whole somoni. They ADD to the amount — never replace it. */
const QUICK_ADD_MAJOR = [100, 500, 1000, 5000];
/** How often a request awaiting approval is re-read. */
const TRACK_INTERVAL_MS = 15_000;
/** The confirm button ignores clicks for this long after the dialog opens, so
 *  a double-click on "Review" can never fall through onto "Request". */
export const CONFIRM_ARM_DELAY_MS = 700;

/** Status of the current request, whatever phase it is in. */
function FlowBanner({ flow }: { flow: FlowState }) {
  const me = useAdminId();

  // Re-read a request while it awaits approval (background: never keeps an
  // idle session alive).
  const awaiting = flow.phase === "tracked" && flow.deposit.status === "pending_approval";
  useEffect(() => {
    if (!awaiting) return;
    const ac = new AbortController();
    const t = setInterval(() => void refreshTracked(ac.signal), TRACK_INTERVAL_MS);
    return () => {
      clearInterval(t);
      ac.abort();
    };
  }, [awaiting]);

  switch (flow.phase) {
    case "idle":
      return null;
    case "submitting":
      return (
        <Alert kind="warning" title="Sending the deposit request…">
          {formatMinor(flow.draft.amount_minor, flow.draft.currency)} for{" "}
          <span className="mono">{flow.draft.phone}</span> · key <IdChip id={flow.key} />
        </Alert>
      );
    case "unknown":
      return (
        <Alert kind="warning" title="Outcome unknown — the request may or may not have reached the server">
          <div className="fund-pending">
            <span>
              {formatMinor(flow.draft.amount_minor, flow.draft.currency)} →{" "}
              <IdChip id={flow.draft.wallet} /> for <span className="mono">{flow.draft.phone}</span>
              , key <IdChip id={flow.key} />. {flow.error}
            </span>
            <span>
              {flow.checking
                ? "Asking the server what it holds under this key…"
                : flow.checkError
                  ? `Could not check yet: ${flow.checkError}`
                  : null}{" "}
              Sending again with the same key is always safe — the server de-duplicates on it, so
              this can never create a second request.
            </span>
            <div className="row">
              <button className="primary" disabled={flow.checking} onClick={() => void checkDeposit()}>
                <Icon name="refresh" size={14} />
                {flow.checking ? "Checking…" : "Check status"}
              </button>
              <button disabled={flow.checking} onClick={() => void retryDeposit()}>
                Send again (same key)
              </button>
              <ConfirmButton
                className="quiet"
                disabled={flow.checking}
                confirmLabel="Forget it? If it reached the server it will be in Approvals."
                onConfirm={discardDeposit}
              >
                Forget this request
              </ConfirmButton>
            </div>
          </div>
        </Alert>
      );
    case "not_found":
      return (
        <Alert kind="warning" title="No deposit request exists under this key">
          <div className="fund-pending">
            <span>
              The server has nothing under key <IdChip id={flow.key} /> (
              {formatMinor(flow.draft.amount_minor, flow.draft.currency)} for{" "}
              <span className="mono">{flow.draft.phone}</span>), so nothing was created. If the
              original request is still being processed it may appear shortly — sending again with
              the same key is safe either way.
            </span>
            <div className="row">
              <button className="primary" onClick={() => void retryDeposit()}>
                Send again (same key)
              </button>
              <button className="quiet" onClick={() => void checkDeposit()}>
                Check again
              </button>
              <button className="quiet" onClick={discardDeposit}>
                Discard
              </button>
            </div>
          </div>
        </Alert>
      );
    case "refused":
      return (
        <Alert kind="error" title="Deposit not requested">
          <div className="fund-pending">
            <span>{flow.error} Nothing was created.</span>
            <div className="row">
              <button className="quiet" onClick={discardDeposit}>
                Dismiss
              </button>
            </div>
          </div>
        </Alert>
      );
    case "mismatch":
      return (
        <Alert kind="error" title="The server holds a DIFFERENT deposit under this key">
          <div className="fund-pending">
            <span>
              You confirmed {formatMinor(flow.draft.amount_minor, flow.draft.currency)} to{" "}
              <IdChip id={flow.draft.wallet} />, but key <IdChip id={flow.key} /> belongs to{" "}
              {formatMinor(flow.deposit.amount_minor, flow.deposit.currency)} to{" "}
              <IdChip id={flow.deposit.user_account} />. Do not retry — report this with the key.
            </span>
            <div className="row">
              <button className="quiet" onClick={discardDeposit}>
                Dismiss
              </button>
            </div>
          </div>
        </Alert>
      );
    case "tracked": {
      const d = flow.deposit;
      const title =
        d.status === "pending_approval"
          ? `Requested ${formatMinor(d.amount_minor, d.currency)} — waiting for a second admin`
          : d.status === "posted"
            ? `Posted ${formatMinor(d.amount_minor, d.currency)} — the customer is credited`
            : `Request for ${formatMinor(d.amount_minor, d.currency)} was rejected`;
      return (
        <Alert kind={d.status === "rejected" ? "error" : d.status === "posted" ? "success" : "warning"} title={title}>
          <div className="fund-pending">
            {d.status === "pending_approval" && (
              <span>
                No money moves until a different admin approves it on the{" "}
                <Link to="/deposits">Approvals</Link> page. This status refreshes automatically.
              </span>
            )}
            <DepositDetails d={d} me={me} />
            {flow.refreshError && <span className="bad">Refresh failed: {flow.refreshError}</span>}
            <div className="row">
              {d.status === "pending_approval" && (
                <button className="quiet" onClick={() => void refreshTracked()}>
                  <Icon name="refresh" size={14} />
                  Refresh now
                </button>
              )}
              <button className="quiet" onClick={discardDeposit}>
                {d.status === "pending_approval" ? "Hide (keeps the request)" : "Done"}
              </button>
            </div>
          </div>
        </Alert>
      );
    }
  }
}

function ConfirmDialog({
  user,
  draft,
  balance,
  onCancel,
  onConfirm,
}: {
  user: AdminUser;
  draft: DepositDraft;
  balance: string;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const [armed, setArmed] = useState(false);
  useEffect(() => {
    const t = setTimeout(() => setArmed(true), CONFIRM_ARM_DELAY_MS);
    return () => clearTimeout(t);
  }, []);
  const large = draft.amount_minor >= LARGE_DEPOSIT_MINOR;
  const amountText = formatMinor(draft.amount_minor, draft.currency);
  return (
    <Modal title="Confirm deposit request" onClose={onCancel}>
      <p className="muted small confirm-lead">
        Check every line against the bank record before requesting.
      </p>
      <dl className="kv">
        <dt>Customer phone</dt>
        <dd className="mono confirm-phone">{user.phone}</dd>
        <dt>Customer</dt>
        <dd className="row tight">
          <Badge tone={user.kyc_level > 0 ? "accent" : "neutral"}>KYC {user.kyc_level}</Badge>
          {user.status !== "active" && <Badge tone="bad">{user.status}</Badge>}
          {user.blocked_reason && <Badge tone="bad">blocked</Badge>}
          <IdChip id={user.id} />
        </dd>
        <dt>Wallet</dt>
        <dd>
          <IdChip id={draft.wallet} short={false} /> · {draft.currency}
        </dd>
        <dt>Balance now</dt>
        <dd>{balance}</dd>
      </dl>
      <BigAmount minor={draft.amount_minor} currency={draft.currency} label="Amount to credit" />
      {large && (
        <Alert kind="warning" title="Large amount">
          This is {formatMinor(LARGE_DEPOSIT_MINOR, draft.currency)} or more. Make sure the figure is
          in somoni, not diram.
        </Alert>
      )}
      <p className="small">
        This creates a request only. A different admin must approve it before the customer is
        credited.
      </p>
      <div className="row end">
        <button data-autofocus onClick={onCancel}>
          Cancel
        </button>
        <button className="primary" disabled={!armed} onClick={onConfirm}>
          Request deposit of {amountText}
        </button>
      </div>
    </Modal>
  );
}

export default function Funding() {
  const flow = useDepositFlow();
  const me = useAdminId();
  const { pendingDeposits, refreshPendingDeposits } = useOps();
  const [user, setUser] = useState<AdminUser | null>(null);
  const [amount, setAmount] = useState("");
  const [lookupError, setLookupError] = useState<string | null>(null);
  const [reviewing, setReviewing] = useState(false);

  const tjsWallet = user?.wallets.find((w) => w.currency === "TJS") ?? null;
  const minor = toMinor(amount);
  const amountInvalid = amount.trim() !== "" && minor === null;

  // When the tracked request posts, re-read the customer so the balance shown
  // is the real one.
  const postedFor =
    flow.phase === "tracked" && flow.deposit.status === "posted" ? flow.draft.phone : null;
  useEffect(() => {
    if (!postedFor) return;
    const ac = new AbortController();
    lookupUser(postedFor, ac.signal).then(
      (u) => setUser((cur) => (cur && cur.id === u.id ? u : cur)),
      () => {},
    );
    return () => ac.abort();
  }, [postedFor]);

  async function pickUser(phone: string) {
    setLookupError(null);
    setAmount("");
    try {
      setUser(await lookupUser(phone));
    } catch (err) {
      setUser(null);
      setLookupError(
        err instanceof ApiError && err.status === 404
          ? "No user with that phone number."
          : describeError(err),
      );
    }
  }

  function addQuick(major: number) {
    const base = amount.trim() === "" ? 0 : minor;
    if (base === null) return;
    setAmount(minorToInput(base + major * 100));
  }

  const unresolved =
    flow.phase === "submitting" || flow.phase === "unknown" || flow.phase === "not_found";
  const ownWallet = user !== null && me !== null && user.id === me;

  // Why can't we request right now? Shown inline so the button is never silently dead.
  const blockReason = unresolved
    ? "Resolve the request above first."
    : !user
      ? "Find a customer to fund."
      : ownWallet
        ? "You cannot fund your own wallet."
        : !tjsWallet
          ? "This customer has no TJS wallet — deposits are TJS-only today."
          : amount.trim() === ""
            ? "Enter an amount."
            : !minor
              ? "Enter a valid amount in somoni (up to 2 decimals, e.g. 1500 or 1500.50)."
              : null;

  const draft: DepositDraft | null =
    user && tjsWallet && minor
      ? {
          customer_user_id: user.id,
          phone: user.phone,
          wallet: tjsWallet.id,
          currency: "TJS",
          amount_minor: minor,
        }
      : null;

  function confirm() {
    if (!draft || blockReason) return;
    setReviewing(false);
    setAmount("");
    void submitDeposit(draft).finally(refreshPendingDeposits);
  }

  return (
    <>
      <header className="page-head">
        <div>
          <h1>Funding</h1>
          <div className="sub">
            Request a deposit for money that arrived at the partner bank. A different admin must
            approve each request before the customer is credited.
          </div>
        </div>
        {pendingDeposits && pendingDeposits.count > 0 && (
          <div className="page-actions">
            <Link className="button" to="/deposits">
              <Icon name="inbox" size={15} />
              {pendingLabel(pendingDeposits)} awaiting approval
            </Link>
          </div>
        )}
      </header>

      <FlowBanner flow={flow} />

      {/* Enter never submits anything on this page: money needs the review dialog. */}
      <form onSubmit={(e) => e.preventDefault()}>
        <div className="panel">
          <div className="panel-title">1 · Customer</div>
          <div className="row">
            <div className="fund-search">
              <UserSearch
                autoFocus
                placeholder="Search phone (e.g. 99290…)"
                onSelect={(u) => void pickUser(u.phone)}
              />
            </div>
            {user && (
              <span className="row tight">
                <span className="mono">{user.phone}</span>
                <Badge tone={user.kyc_level > 0 ? "accent" : "neutral"}>KYC {user.kyc_level}</Badge>
                {user.status !== "active" && <Badge tone="bad">{user.status}</Badge>}
                {user.blocked_reason && <Badge tone="bad">blocked</Badge>}
              </span>
            )}
          </div>
          {lookupError && <Alert kind="error">{lookupError}</Alert>}
        </div>

        {user && tjsWallet && (
          <div className="panel">
            <div className="panel-title">2 · Amount</div>

            <div className="fund-balance">
              <span className="fund-balance-label">Current TJS balance</span>
              <span className="fund-balance-value">{tjsWallet.display}</span>
            </div>

            <label className="field" htmlFor="fund-amount">
              Amount in TJS — type somoni, e.g. 1500 or 1500.50 (not diram)
            </label>
            <div className="fund-amount">
              <div className="amount-input-wrap">
                <input
                  id="fund-amount"
                  className="amount-input"
                  placeholder="0.00"
                  inputMode="decimal"
                  autoComplete="off"
                  aria-describedby="fund-amount-preview"
                  aria-invalid={amountInvalid}
                  value={amount}
                  onChange={(e) => setAmount(e.target.value)}
                />
                <span className="amount-suffix">TJS</span>
              </div>
              <button
                type="button"
                className="primary big"
                disabled={!!blockReason}
                onClick={() => setReviewing(true)}
              >
                Review deposit…
              </button>
            </div>

            <div className="chip-row" role="group" aria-label="Add to amount">
              <span className="muted small">Add:</span>
              {QUICK_ADD_MAJOR.map((v) => (
                <button
                  key={v}
                  type="button"
                  className="amount-chip"
                  disabled={amountInvalid}
                  onClick={() => addQuick(v)}
                  aria-label={`Add ${v.toLocaleString()} TJS`}
                >
                  +{v.toLocaleString()} TJS
                </button>
              ))}
              <button
                type="button"
                className="amount-chip"
                disabled={amount === ""}
                onClick={() => setAmount("")}
              >
                Clear
              </button>
            </div>

            <div id="fund-amount-preview" className="fund-preview" aria-live="polite">
              {minor ? (
                <>
                  <div className="fund-preview-amount">{formatMinor(minor, "TJS")}</div>
                  <div className="muted small">
                    {minor.toLocaleString()} diram · balance after approval{" "}
                    {formatMinor(tjsWallet.balance_minor + minor, "TJS")}
                    {minor >= LARGE_DEPOSIT_MINOR && (
                      <>
                        {" "}
                        · <span className="warn">large amount</span>
                      </>
                    )}
                  </div>
                </>
              ) : (
                blockReason && <div className="fund-hint muted small">{blockReason}</div>
              )}
            </div>
            {minor !== null && blockReason && (
              <div className="fund-hint muted small">{blockReason}</div>
            )}
          </div>
        )}
      </form>

      {user && !tjsWallet && (
        <Alert kind="warning">
          {user.phone} has no TJS wallet — non-TJS wallets can’t be funded yet.
        </Alert>
      )}

      {reviewing && user && draft && tjsWallet && (
        <ConfirmDialog
          user={user}
          draft={draft}
          balance={tjsWallet.display}
          onCancel={() => setReviewing(false)}
          onConfirm={confirm}
        />
      )}
    </>
  );
}
