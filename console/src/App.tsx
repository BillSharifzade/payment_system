import { useEffect, useState } from "react";
import {
  createBrowserRouter,
  NavLink,
  Navigate,
  Outlet,
  RouterProvider,
  useNavigate,
} from "react-router-dom";
import { getKycQueue, loadSession, logout, setSessionLostHandler } from "./api";
import { Icon, IconName, ToastProvider, useAdminStatus } from "./ui";
import Login from "./pages/Login";
import Dashboard from "./pages/Dashboard";
import KycQueue from "./pages/KycQueue";
import Users from "./pages/Users";
import FxRates from "./pages/FxRates";
import Funding from "./pages/Funding";

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
      {count !== undefined && count > 0 && <span className="nav-count">{count}</span>}
    </NavLink>
  );
}

/** The always-visible integrity signal: conservation + sealer backlog. */
function HealthPill() {
  const { status, error } = useAdminStatus(30_000);
  if (error)
    return (
      <div className="health unknown" title={error}>
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

function Shell({ onLogout }: { onLogout: () => void }) {
  const navigate = useNavigate();
  const [pendingKyc, setPendingKyc] = useState(0);

  useEffect(() => {
    setSessionLostHandler(() => {
      onLogout();
      navigate("/login");
    });
  }, [navigate, onLogout]);

  // Pending-review count for the nav badge; silent on failure.
  useEffect(() => {
    let live = true;
    const tick = () =>
      getKycQueue("pending")
        .then((list) => live && setPendingKyc(list.length))
        .catch(() => {});
    tick();
    const t = setInterval(tick, 60_000);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, []);

  return (
    <>
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
          <NavItem to="/kyc" icon="shield" label="KYC" count={pendingKyc} />
          <NavItem to="/users" icon="users" label="Users" />
          <NavItem to="/fx" icon="exchange" label="FX rates" />
          <NavItem to="/funding" icon="bank" label="Funding" />
        </nav>
        <div className="topbar-right">
          <HealthPill />
          <button
            className="quiet"
            title="Sign out"
            onClick={async () => {
              await logout();
              onLogout();
              navigate("/login");
            }}
          >
            <Icon name="logout" size={15} />
            Sign out
          </button>
        </div>
      </header>
      <main>
        <Outlet />
      </main>
    </>
  );
}

export default function App() {
  const [authed, setAuthed] = useState(() => loadSession());

  const router = createBrowserRouter(
    [
      {
        path: "/login",
        element: authed ? <Navigate to="/" /> : <Login onLogin={() => setAuthed(true)} />,
      },
      {
        path: "/",
        element: authed ? <Shell onLogout={() => setAuthed(false)} /> : <Navigate to="/login" />,
        children: [
          { index: true, element: <Dashboard /> },
          { path: "kyc", element: <KycQueue /> },
          { path: "users", element: <Users /> },
          { path: "fx", element: <FxRates /> },
          { path: "funding", element: <Funding /> },
        ],
      },
    ],
    { basename: "/admin" },
  );

  return (
    <ToastProvider>
      <RouterProvider router={router} />
    </ToastProvider>
  );
}
