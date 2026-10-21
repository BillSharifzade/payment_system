// Route table, shared by the real browser router (App.tsx) and the tests
// (createMemoryRouter). Auth is a layout route reading the external auth
// store, so the router itself is built once and never depends on React state.

import { useEffect } from "react";
import {
  Link,
  Navigate,
  Outlet,
  RouteObject,
  isRouteErrorResponse,
  useRouteError,
} from "react-router-dom";
import { useAuthed } from "./auth";
import Shell from "./Shell";
import { Icon } from "./ui";
import Login from "./pages/Login";
import Dashboard from "./pages/Dashboard";
import KycQueue from "./pages/KycQueue";
import Users from "./pages/Users";
import FxRates from "./pages/FxRates";
import Funding from "./pages/Funding";
import Deposits from "./pages/Deposits";
import Terminals from "./pages/Terminals";

/** Layout route: everything beneath it needs a signed-in admin. */
function AuthGate() {
  return useAuthed() ? <Outlet /> : <Navigate to="/login" replace />;
}

function LoginRoute() {
  return useAuthed() ? <Navigate to="/" replace /> : <Login />;
}

/** Root errorElement: a render error anywhere below lands here instead of
 *  react-router's default page (which prints the stack). The stack still goes
 *  to the browser console for whoever is debugging. */
export function CrashPage() {
  const err = useRouteError();
  const notFound = isRouteErrorResponse(err) && err.status === 404;
  useEffect(() => {
    if (!notFound) console.error(err);
  }, [err, notFound]);

  if (notFound) {
    return (
      <div className="crash-wrap">
        <div className="crash-card">
          <Icon name="alert" size={26} />
          <h1>Page not found</h1>
          <p className="crash-msg">There is nothing at this address.</p>
          <Link className="button primary" to="/">
            Back to the dashboard
          </Link>
        </div>
      </div>
    );
  }

  const message = err instanceof Error ? err.message : String(err);
  return (
    <div className="crash-wrap">
      <div className="crash-card" role="alert">
        <Icon name="alert" size={26} />
        <h1>Something went wrong</h1>
        <p className="crash-msg">{message}</p>
        <p className="muted small">
          Nothing was lost on the server. Reload to continue; if it happens again, quote the
          message above.
        </p>
        <button className="primary" onClick={() => window.location.reload()}>
          <Icon name="refresh" size={15} />
          Reload
        </button>
      </div>
    </div>
  );
}

export const routes: RouteObject[] = [
  {
    path: "/",
    errorElement: <CrashPage />,
    children: [
      { path: "login", element: <LoginRoute /> },
      {
        element: <AuthGate />,
        children: [
          {
            element: <Shell />,
            children: [
              { index: true, element: <Dashboard /> },
              { path: "kyc", element: <KycQueue /> },
              { path: "users", element: <Users /> },
              { path: "fx", element: <FxRates /> },
              { path: "funding", element: <Funding /> },
              { path: "deposits", element: <Deposits /> },
              { path: "terminals", element: <Terminals /> },
            ],
          },
        ],
      },
    ],
  },
];
