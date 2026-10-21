import { FormEvent, useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import {
  ApiError,
  Deposit,
  DepositStatus,
  approveDeposit,
  describeError,
  getDeposit,
  isAbort,
  listDeposits,
  parseTime,
  rejectDeposit,
} from "../api";
import { useAdminId } from "../auth";
import { AdminRef, BigAmount, DepositDetails, DepositStatusBadge } from "../depositViews";
import { formatMinor } from "../money";
import { useOps } from "../ops";
import {
  Ago,
  Alert,
  ConfirmButton,
  EmptyState,
  Icon,
  IdChip,
  Modal,
  Segmented,
  Skeleton,
  useToast,
} from "../ui";

// Approvals: the CHECKER side of deposit dual control (contract §3). A
// pending request is approved or rejected here by an admin other than the one
// who requested it; the server enforces that (403 dual_control_required), the
// console just makes it obvious up front. Also: posted/rejected history and
// tracking any request by id.

type Tab = DepositStatus | "track";
const PAGE_SIZE = 50;

/** Sharper copy for the codes a decision can fail with. */
export const DECISION_ERROR_COPY: Record<string, string> = {
  dual_control_required:
    "Dual control: you cannot approve this request — either you requested it or the wallet is yours. A different admin must approve it.",
  conflict: "This request was already decided by someone else. Its current status is shown above.",
};

function DepositReview({
  deposit: initial,
  onClose,
  onChanged,
}: {
  deposit: Deposit;
  onClose: () => void;
  onChanged: (d: Deposit) => void;
}) {
  const toast = useToast();
  const me = useAdminId();
  const [d, setD] = useState(initial);
  const [reason, setReason] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const mine = me !== null && d.requested_by === me;
  const pending = d.status === "pending_approval";
  const amount = formatMinor(d.amount_minor, d.currency);

  async function decide(fn: () => Promise<Deposit>, done: (d: Deposit) => string) {
    setBusy(true);
    setError(null);
    try {
      const next = await fn();
      setD(next);
      onChanged(next);
      toast("success", done(next));
      onClose();
    } catch (e) {
      setError(describeError(e, DECISION_ERROR_COPY));
      // A refusal for policy reasons (403/422) leaves the request untouched;
      // anything else (409 already decided, lost answer, 5xx) — re-read the
      // request so the operator sees what the server actually holds.
      const definite = e instanceof ApiError && (e.status === 403 || e.status === 422 || e.status === 400);
      if (!definite) {
        try {
          const cur = await getDeposit(d.id);
          setD(cur);
          onChanged(cur);
        } catch {
          /* the error above already says something went wrong */
        }
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal title={`Deposit request — ${d.customer_phone ?? "unknown customer"}`} onClose={onClose}>
      <BigAmount minor={d.amount_minor} currency={d.currency} label="Amount" />
      <DepositDetails d={d} me={me} />

      {pending && mine && (
        <Alert kind="warning" title="This is your own request">
          Dual control: a different admin must approve it. You can withdraw it below.
        </Alert>
      )}

      {pending && (
        <div className="decision">
          {!mine && (
            <ConfirmButton
              className="approve"
              disabled={busy}
              confirmLabel={`Confirm: credit ${amount} to ${d.customer_phone ?? "this wallet"}?`}
              onConfirm={() =>
                void decide(
                  () => approveDeposit(d.id),
                  (n) =>
                    n.status === "posted"
                      ? `Approved — ${amount} credited`
                      : `Request is now ${n.status}`,
                )
              }
            >
              <Icon name="check" size={15} />
              Approve and credit {amount}
            </ConfirmButton>
          )}
          <label className="field">
            {mine ? "Reason for withdrawing" : "Rejection reason"}
            <input
              placeholder={mine ? "e.g. wrong amount entered" : "e.g. no matching bank credit"}
              value={reason}
              onChange={(e) => setReason(e.target.value)}
            />
          </label>
          <button
            className="danger"
            disabled={busy || reason.trim().length < 3}
            onClick={() =>
              void decide(
                () => rejectDeposit(d.id, reason.trim()),
                () => (mine ? "Request withdrawn" : "Request rejected"),
              )
            }
          >
            {mine ? "Withdraw request" : "Reject"}
          </button>
        </div>
      )}
      {error && <Alert kind="error">{error}</Alert>}
    </Modal>
  );
}

function TrackById({ onOpen }: { onOpen: (d: Deposit) => void }) {
  const me = useAdminId();
  const [id, setId] = useState("");
  const [found, setFound] = useState<Deposit | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function lookup(e: FormEvent) {
    e.preventDefault();
    const q = id.trim();
    if (!q) return;
    setBusy(true);
    setError(null);
    setFound(null);
    try {
      setFound(await getDeposit(q));
    } catch (err) {
      setError(
        err instanceof ApiError && err.status === 404
          ? "No deposit request with that id."
          : describeError(err),
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="panel">
      <div className="panel-title">Track a request by id</div>
      {/* A read-only lookup, so Enter submitting is fine here. */}
      <form className="row" onSubmit={lookup}>
        <label className="field grow">
          Request id (the deposit's idempotency key)
          <input
            className="mono"
            placeholder="00000000-0000-0000-0000-000000000000"
            value={id}
            onChange={(e) => setId(e.target.value)}
            autoComplete="off"
          />
        </label>
        <button className="primary align-end" disabled={busy || !id.trim()}>
          <Icon name="search" size={15} />
          {busy ? "Looking up…" : "Look up"}
        </button>
      </form>
      {error && <Alert kind="error">{error}</Alert>}
      {found && (
        <div className="track-result">
          <BigAmount minor={found.amount_minor} currency={found.currency} label="Amount" />
          <DepositDetails d={found} me={me} />
          {found.status === "pending_approval" && (
            <div className="row">
              <button onClick={() => onOpen(found)}>Review…</button>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function When({ at }: { at: string | null }) {
  const ms = parseTime(at);
  return ms === null ? <span className="muted">—</span> : <Ago ms={ms} />;
}

export default function Deposits() {
  const me = useAdminId();
  const { refreshPendingDeposits } = useOps();
  const [tab, setTab] = useState<Tab>("pending_approval");
  const [gen, setGen] = useState(0);
  // The loaded page remembers which (tab, reload) it was fetched for; a
  // mismatch renders as "loading", and a slow response for a tab the operator
  // already left is never shown.
  const key = `${tab}#${gen}`;
  const [page, setPage] = useState<{
    key: string;
    items: Deposit[];
    cursor: string | null;
  } | null>(null);
  const items = page?.key === key ? page.items : null;
  const cursor = page?.key === key ? page.cursor : null;
  const [more, setMore] = useState(false);
  const [open, setOpen] = useState<Deposit | null>(null);
  const [error, setError] = useState<string | null>(null);
  const acRef = useRef<AbortController | null>(null);

  useEffect(() => {
    if (tab === "track") return;
    const ac = new AbortController();
    acRef.current = ac;
    listDeposits(tab, { limit: PAGE_SIZE, signal: ac.signal })
      .then((p) => {
        setPage({ key, items: p.items, cursor: p.next_cursor });
        setError(null);
      })
      .catch((e) => {
        if (!isAbort(e)) setError(describeError(e));
      });
    return () => ac.abort();
  }, [key, tab]);

  const reload = () => {
    setGen((g) => g + 1);
    refreshPendingDeposits();
  };

  async function loadMore() {
    const ac = acRef.current;
    if (tab === "track" || !cursor || more || !ac) return;
    setMore(true);
    try {
      const p = await listDeposits(tab, { limit: PAGE_SIZE, cursor, signal: ac.signal });
      setPage((cur) =>
        cur && cur.key === key
          ? { key, items: [...cur.items, ...p.items], cursor: p.next_cursor }
          : cur,
      );
      setError(null);
    } catch (e) {
      if (!isAbort(e)) setError(describeError(e));
    } finally {
      setMore(false);
    }
  }

  const history = tab === "posted" || tab === "rejected";

  return (
    <>
      <header className="page-head">
        <div>
          <h1>Deposit approvals</h1>
          <div className="sub">
            Every deposit is requested by one admin and approved by another. Check each request
            against the partner-bank record before approving.{" "}
            <Link to="/funding">Request a deposit →</Link>
          </div>
        </div>
        <div className="page-actions">
          <Segmented
            value={tab}
            onChange={(v) => setTab(v as Tab)}
            options={[
              { value: "pending_approval", label: "Pending" },
              { value: "posted", label: "Posted" },
              { value: "rejected", label: "Rejected" },
              { value: "track", label: "Track by id" },
            ]}
          />
          {tab !== "track" && (
            <button className="quiet" onClick={reload} aria-label="Reload">
              <Icon name="refresh" size={15} />
            </button>
          )}
        </div>
      </header>

      {error && <Alert kind="error">{error}</Alert>}

      {tab === "track" ? (
        <TrackById onOpen={setOpen} />
      ) : (
        <>
          <div className="table-wrap">
            {items === null ? (
              <div className="skeleton-stack">
                <Skeleton w="100%" h={18} />
                <Skeleton w="85%" h={18} />
                <Skeleton w="92%" h={18} />
              </div>
            ) : items.length === 0 ? (
              <EmptyState
                title={
                  tab === "pending_approval"
                    ? "No requests awaiting approval"
                    : `No ${tab} deposits`
                }
                hint={
                  tab === "pending_approval"
                    ? "Requests made on the Funding page appear here for a second admin."
                    : undefined
                }
              />
            ) : (
              <table>
                <thead>
                  <tr>
                    <th>{history ? "Decided" : "Requested"}</th>
                    <th>Customer</th>
                    <th className="num">Amount</th>
                    <th>Wallet</th>
                    <th>Requested by</th>
                    {history && <th>Decided by</th>}
                    {tab === "rejected" && <th>Reason</th>}
                    {!history && <th>Status</th>}
                  </tr>
                </thead>
                <tbody>
                  {items.map((d) => (
                    <tr
                      key={d.id}
                      className="clickable"
                      tabIndex={0}
                      onClick={() => setOpen(d)}
                      onKeyDown={(e) => e.key === "Enter" && setOpen(d)}
                    >
                      <td>
                        <When at={history ? d.decided_at : d.requested_at} />
                      </td>
                      <td className="mono">{d.customer_phone ?? "—"}</td>
                      <td className="num strong">{formatMinor(d.amount_minor, d.currency)}</td>
                      <td>
                        <IdChip id={d.user_account} />
                      </td>
                      <td>
                        <AdminRef id={d.requested_by} me={me} />
                      </td>
                      {history && (
                        <td>{d.decided_by ? <AdminRef id={d.decided_by} me={me} /> : "—"}</td>
                      )}
                      {tab === "rejected" && <td className="wrap">{d.reason ?? "—"}</td>}
                      {!history && (
                        <td>
                          {me !== null && d.requested_by === me ? (
                            <span className="muted small">yours — needs another admin</span>
                          ) : (
                            <DepositStatusBadge status={d.status} />
                          )}
                        </td>
                      )}
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
          {items !== null && cursor && (
            <div className="load-more">
              <button onClick={loadMore} disabled={more}>
                {more ? "Loading…" : "Load more"}
              </button>
            </div>
          )}
        </>
      )}

      {open && (
        <DepositReview
          deposit={open}
          onClose={() => setOpen(null)}
          onChanged={() => reload()}
        />
      )}
    </>
  );
}
