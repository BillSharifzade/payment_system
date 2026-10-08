// The signed-in frame: top bar (brand, nav, integrity pill, sign-out) around
// the routed page. Owns the OpsProvider, so admin status, metrics and the
// pending-deposit queue are polled once here for every consumer — health
// pill, nav badges, dashboard tiles and the queues' counts all read the same
// data — and the idle guard, which signs an unattended console out.

import { NavLink, Outlet } from "react-router-dom";
import { logout } from "./api";
import { IdleGuard } from "./idle";
import { OpsProvider, pendingLabel, useOps } from "./ops";
import { Icon, IconName } from "./ui";

function NavItem({
  to,
  icon,
  label,
  count,
  countLabel,
  end,
}: {
  to: string;
  icon: IconName;
  label: string;
  count?: number;
  /** Overrides how `count` is shown (e.g. "50+"). */
  countLabel?: string;
  end?: boolean;
}) {
  const shown = count !== undefined && count > 0;
  return (
    <NavLink to={to} end={end} className="nav-link">
      <Icon name={icon} size={15} />
      {label}
      {shown && (
        <span className="nav-count" aria-label={`${countLabel ?? count} waiting`}>
          {countLabel ?? count.toLocaleString()}
        </span>
      )}
    </NavLink>
  );
}

/** The always-visible integrity signal: conservation + sealer backlog. */
function HealthPill() {
  const { status, statusError } = useOps();
  if (statusError)
    return (
      <div className="health unknown" title={statusError}>
        <span className="dot" />
        <span className="health-label">Status unavailable</span>
      </div>
    );
  if (!status)
    return (
      <div className="health unknown">
        <span className="dot" />
        <span className="health-label">Checking…</span>
      </div>
    );
  const conserved = status.conservation.every((c) => c.net_minor === 0);
  if (!conserved)
    return (
      <div className="health bad">
        <span className="dot" />
        <span className="health-label">INTEGRITY ALARM</span>
      </div>
    );
  if (status.unsealed_transactions > 5000)
    return (
      <div className="health warn" title={`${status.unsealed_transactions} unsealed`}>
        <span className="dot" />
        <span className="health-label">Sealer backlog</span>
      </div>
    );
  return (
    <div className="health ok">
      <span className="dot" />
      <span className="health-label">Ledger balanced</span>
    </div>
  );
}

function Topbar() {
  const { metrics, pendingDeposits } = useOps();
  return (
    <header className="topbar">
      <div className="brand">
        <div className="brand-mark">
          <Icon name="bank" size={16} />
        </div>
        <div>
          <div className="brand-name">Payment Ops</div>
          <div className="brand-sub">admin console</div>
        </div>
      </div>
      <nav className="topnav" aria-label="Main">
        <NavItem to="/" end icon="dashboard" label="Dashboard" />
        {/* Badge = the real pending count from metrics, not a capped list length. */}
        <NavItem to="/kyc" icon="shield" label="KYC" count={metrics?.kyc.pending} />
        <NavItem to="/users" icon="users" label="Users" />
        <NavItem to="/fx" icon="exchange" label="FX rates" />
        <NavItem to="/funding" icon="bank" label="Funding" />
        <NavItem
          to="/deposits"
          icon="inbox"
          label="Approvals"
          count={pendingDeposits?.count}
          countLabel={pendingDeposits ? pendingLabel(pendingDeposits) : undefined}
        />
        <NavItem to="/terminals" icon="fingerprint" label="Terminals" />
      </nav>
      <div className="topbar-right">
        <HealthPill />
        <button className="quiet" title="Sign out" onClick={() => void logout()}>
          <Icon name="logout" size={15} />
          Sign out
        </button>
      </div>
    </header>
  );
}

export default function Shell() {
  return (
    <OpsProvider>
      <IdleGuard />
      <Topbar />
      <main>
        <Outlet />
      </main>
    </OpsProvider>
  );
}
