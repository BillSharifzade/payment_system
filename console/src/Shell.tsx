// The signed-in frame: top bar (brand, nav, integrity pill, sign-out) around
// the routed page. Owns the OpsProvider, so admin status and metrics are
// polled once here for every consumer — health pill, KYC badge, dashboard
// tiles and the queue's counts all read the same data.

import { NavLink, Outlet } from "react-router-dom";
import { logout } from "./api";
import { OpsProvider, useOps } from "./ops";
import { Icon, IconName } from "./ui";

function NavItem({
  to,
  icon,
  label,
  count,
  end,
}: {
  to: string;
  icon: IconName;
  label: string;
  count?: number;
  end?: boolean;
}) {
  return (
    <NavLink to={to} end={end} className="nav-link">
      <Icon name={icon} size={15} />
      {label}
      {count !== undefined && count > 0 && (
        <span className="nav-count">{count.toLocaleString()}</span>
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
  const { metrics } = useOps();
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
      <Topbar />
      <main>
        <Outlet />
      </main>
    </OpsProvider>
  );
}
