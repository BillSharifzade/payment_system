import { FormEvent, useState } from "react";
import { ApiError, login } from "../api";
import { useSessionNotice } from "../auth";
import { Alert, Icon } from "../ui";

export default function Login() {
  const notice = useSessionNotice();
  const [phone, setPhone] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await login(phone, password); // flips the auth store; the route redirects
    } catch (err) {
      if (err instanceof ApiError && err.code === "forbidden") {
        setError("This account is not an admin.");
      } else if (err instanceof ApiError && err.status === 429) {
        setError("Too many attempts — wait a few minutes.");
      } else if (!(err instanceof ApiError) || err.status >= 500) {
        // Network failure or a server-side error is not the operator's fault —
        // never tell them their password was wrong.
        const ref = err instanceof ApiError && err.requestId ? ` (Ref: ${err.requestId})` : "";
        setError(`Service unavailable — try again.${ref}`);
      } else {
        setError("Invalid credentials.");
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="login-wrap">
      <form className="login-card" onSubmit={submit}>
        <div className="brand">
          <div className="brand-mark">
            <Icon name="bank" size={16} />
          </div>
          <div>
            <div className="brand-name">Payment Ops</div>
            <div className="brand-sub">admin console</div>
          </div>
        </div>
        {notice && !error && <Alert kind="warning">{notice}</Alert>}
        <label className="field">
          Phone
          <input
            placeholder="992901234567"
            value={phone}
            onChange={(e) => setPhone(e.target.value)}
            autoComplete="username"
            autoFocus
          />
        </label>
        <label className="field">
          Password
          <input
            type="password"
            placeholder="••••••••"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            autoComplete="current-password"
          />
        </label>
        <button className="primary" disabled={busy || !phone || !password}>
          {busy ? "Signing in…" : "Sign in"}
        </button>
        {error && <Alert kind="error">{error}</Alert>}
      </form>
    </div>
  );
}
