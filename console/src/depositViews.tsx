// Presentational pieces shared by Funding (the maker side) and Approvals (the
// checker side + history + track-by-id), so a deposit reads the same everywhere.

import { Deposit, DepositStatus, parseTime } from "./api";
import { formatMinor } from "./money";
import { Ago, Badge, IdChip } from "./ui";

/** Amounts at or above this get an explicit "large amount" warning in every
 *  confirmation (100,000.00 TJS). Display-only; the server enforces its own max. */
export const LARGE_DEPOSIT_MINOR = 10_000_000;

const STATUS_LABEL: Record<DepositStatus, string> = {
  pending_approval: "awaiting approval",
  posted: "posted",
  rejected: "rejected",
};

export function DepositStatusBadge({ status }: { status: DepositStatus }) {
  const tone = status === "posted" ? "ok" : status === "rejected" ? "bad" : "warn";
  const icon = status === "posted" ? "check" : status === "rejected" ? "x" : "clock";
  return (
    <Badge tone={tone} icon={icon}>
      {STATUS_LABEL[status] ?? status}
    </Badge>
  );
}

function When({ at }: { at: string | null }) {
  const ms = parseTime(at);
  return ms === null ? <span className="muted">—</span> : <Ago ms={ms} />;
}

/** "you" next to an admin id that is the signed-in admin. */
export function AdminRef({ id, me }: { id: string; me: string | null }) {
  return (
    <span className="row tight">
      <IdChip id={id} />
      {me !== null && id === me && <Badge tone="accent">you</Badge>}
    </span>
  );
}

/** The big, unambiguous amount line used in confirmations and detail views. */
export function BigAmount({ minor, currency, label }: { minor: number; currency: string; label: string }) {
  return (
    <div className="big-amount">
      <div className="big-amount-label">{label}</div>
      <div className="big-amount-value" data-testid="big-amount">
        {formatMinor(minor, currency)}
      </div>
      <div className="muted small">
        {minor.toLocaleString()} minor units (diram) · {currency}
      </div>
    </div>
  );
}

/** Every field of a deposit request, for review and tracking. */
export function DepositDetails({ d, me }: { d: Deposit; me: string | null }) {
  return (
    <dl className="kv">
      <dt>Status</dt>
      <dd>
        <DepositStatusBadge status={d.status} />
      </dd>
      <dt>Customer</dt>
      <dd className="mono">{d.customer_phone ?? <span className="muted">unknown</span>}</dd>
      <dt>Wallet</dt>
      <dd>
        <IdChip id={d.user_account} short={false} />
      </dd>
      <dt>Requested by</dt>
      <dd>
        <AdminRef id={d.requested_by} me={me} />
      </dd>
      <dt>Requested</dt>
      <dd>
        <When at={d.requested_at} />
      </dd>
      {d.decided_by && (
        <>
          <dt>{d.status === "rejected" ? "Rejected by" : "Approved by"}</dt>
          <dd>
            <AdminRef id={d.decided_by} me={me} />
          </dd>
          <dt>Decided</dt>
          <dd>
            <When at={d.decided_at} />
          </dd>
        </>
      )}
      {d.reason && (
        <>
          <dt>Reason</dt>
          <dd>{d.reason}</dd>
        </>
      )}
      <dt>Request id</dt>
      <dd>
        <IdChip id={d.id} short={false} />
      </dd>
      {d.status === "posted" && (
        <>
          <dt>Transaction</dt>
          <dd>
            <IdChip id={d.transaction_id} short={false} />
          </dd>
        </>
      )}
    </dl>
  );
}
