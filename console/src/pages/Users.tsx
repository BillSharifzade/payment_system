import { useCallback, useEffect, useState } from "react";
import {
  AdminUser,
  ApiError,
  UserListItem,
  blockUser,
  describeError,
  formatTime,
  isAbort,
  listUsers,
  lookupUser,
  setUserStatus,
  unblockUser,
} from "../api";
import {
  Ago,
  Alert,
  Badge,
  ConfirmButton,
  EmptyState,
  Icon,
  IdChip,
  Skeleton,
  UserSearch,
  useToast,
} from "../ui";

function StatusBadges({ u }: { u: { status: string; kyc_level: number; is_admin: boolean; is_blocked?: boolean } }) {
  return (
    <span className="row" style={{ gap: "0.35rem", flexWrap: "nowrap" }}>
      {u.status === "active" ? (
        <Badge tone="ok" icon="check">
          active
        </Badge>
      ) : (
        <Badge tone="bad" icon="snowflake">
          {u.status}
        </Badge>
      )}
      {u.is_admin && <Badge tone="warn">admin</Badge>}
      {u.is_blocked && (
        <Badge tone="bad" icon="block">
          blocked
        </Badge>
      )}
    </span>
  );
}

/** Full profile + enforcement for one user (loaded by phone). `onUser` fires
 *  with every fresh profile — including after Freeze/Block/Unblock — so the
 *  list behind this view can patch its row instead of showing stale badges. */
function UserDetail({
  phone,
  onBack,
  onUser,
}: {
  phone: string;
  onBack: () => void;
  onUser: (u: AdminUser) => void;
}) {
  const toast = useToast();
  const [user, setUser] = useState<AdminUser | null>(null);
  const [gen, setGen] = useState(0);
  const [blockReason, setBlockReason] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const ac = new AbortController();
    lookupUser(phone, ac.signal)
      .then((u) => {
        setUser(u);
        setError(null);
        onUser(u);
      })
      .catch((err) => {
        if (isAbort(err)) return;
        setError(
          err instanceof ApiError && err.status === 404
            ? "No user with that phone number."
            : describeError(err),
        );
      });
    return () => ac.abort();
  }, [phone, gen, onUser]);

  async function act(fn: () => Promise<unknown>, done: string) {
    setBusy(true);
    setError(null);
    try {
      await fn();
      toast("success", done);
      setGen((g) => g + 1); // re-fetch; the effect above also patches the list row
    } catch (e) {
      setError(describeError(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <div className="row" style={{ marginBottom: "0.9rem" }}>
        <button className="quiet" onClick={onBack}>
          <Icon name="back" size={15} />
          All users
        </button>
      </div>

      {error && <Alert kind="error">{error}</Alert>}
      {!user && !error && (
        <div className="panel">
          <Skeleton w="60%" h={22} />
          <div className="gap-12" />
          <Skeleton w="35%" h={14} />
        </div>
      )}

      {user && (
        <>
          <div className="panel">
            <div className="row" style={{ justifyContent: "space-between", alignItems: "flex-start" }}>
              <div>
                <h2 style={{ fontSize: "1.15rem" }} className="mono">
                  {user.phone}
                </h2>
                <div className="muted small" style={{ marginTop: "0.25rem" }}>
                  joined {formatTime(user.created_at_ms)} · <IdChip id={user.id} />
                </div>
              </div>
              <div className="row">
                <StatusBadges
                  u={{ ...user, is_blocked: user.blocked_reason !== null }}
                />
                <Badge tone={user.kyc_level > 0 ? "accent" : "neutral"}>
                  KYC level {user.kyc_level}
                </Badge>
              </div>
            </div>
            {user.blocked_reason !== null && (
              <Alert kind="warning" title="On the transfer blocklist">
                {user.blocked_reason}
              </Alert>
            )}
          </div>

          <div className="table-wrap">
            {user.wallets.length === 0 ? (
              <EmptyState title="No wallets" hint="This user has not opened a wallet yet." />
            ) : (
              <table>
                <thead>
                  <tr>
                    <th>Wallet</th>
                    <th>Currency</th>
                    <th className="num">Balance</th>
                  </tr>
                </thead>
                <tbody>
                  {user.wallets.map((w) => (
                    <tr key={w.id}>
                      <td>
                        <IdChip id={w.id} />
                      </td>
                      <td>{w.currency}</td>
                      <td className="num">{w.display}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>

          <div className="panel danger-zone">
            <div className="panel-title">Enforcement</div>
            <div className="row" style={{ marginBottom: "0.75rem" }}>
              {user.status === "active" ? (
                <ConfirmButton
                  className="danger"
                  disabled={busy}
                  confirmLabel="Confirm freeze?"
                  onConfirm={() =>
                    act(
                      () => setUserStatus(user.id, "frozen"),
                      "Account frozen — all sessions revoked",
                    )
                  }
                >
                  <Icon name="snowflake" size={15} />
                  Freeze account
                </ConfirmButton>
              ) : (
                <ConfirmButton
                  className=""
                  disabled={busy}
                  confirmLabel="Confirm reactivation?"
                  onConfirm={() =>
                    act(() => setUserStatus(user.id, "active"), "Account reactivated")
                  }
                >
                  Reactivate ({user.status})
                </ConfirmButton>
              )}
              <span className="muted small">
                Freezing ends every session immediately; the user cannot log in or refresh.
              </span>
            </div>

            <div className="row">
              {user.blocked_reason === null ? (
                <>
                  <input
                    placeholder="Block reason (sanctions / fraud …)"
                    value={blockReason}
                    onChange={(e) => setBlockReason(e.target.value)}
                    style={{ width: 300 }}
                  />
                  <ConfirmButton
                    className="danger"
                    disabled={busy || blockReason.trim().length < 3}
                    confirmLabel="Confirm block?"
                    onConfirm={() =>
                      act(() => blockUser(user.id, blockReason.trim()), "User blocked")
                    }
                  >
                    <Icon name="block" size={15} />
                    Block transfers
                  </ConfirmButton>
                </>
              ) : (
                <ConfirmButton
                  className=""
                  disabled={busy}
                  confirmLabel="Confirm unblock?"
                  onConfirm={() => act(() => unblockUser(user.id), "User unblocked")}
                >
                  Unblock transfers
                </ConfirmButton>
              )}
              <span className="muted small">
                Blocking stops sending and receiving but leaves the session alive.
              </span>
            </div>
          </div>
        </>
      )}
    </>
  );
}

export default function Users() {
  const [rows, setRows] = useState<UserListItem[] | null>(null);
  const [cursor, setCursor] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [openPhone, setOpenPhone] = useState<string | null>(null);

  useEffect(() => {
    const ac = new AbortController();
    listUsers({ limit: 25, signal: ac.signal })
      .then((r) => {
        setRows(r.users);
        setCursor(r.next_cursor);
        setError(null);
      })
      .catch((e) => {
        if (!isAbort(e)) setError(describeError(e));
      });
    return () => ac.abort();
  }, []);

  async function loadMore() {
    if (!cursor) return;
    setLoadingMore(true);
    try {
      const r = await listUsers({ limit: 25, cursor });
      setRows((prev) => [...(prev ?? []), ...r.users]);
      setCursor(r.next_cursor);
    } catch (e) {
      setError(describeError(e));
    } finally {
      setLoadingMore(false);
    }
  }

  // Keep the list's row in step with what the detail view just learned, so
  // "back" never shows a badge the operator has just changed.
  const patchRow = useCallback((u: AdminUser) => {
    setRows(
      (prev) =>
        prev &&
        prev.map((r) =>
          r.id === u.id
            ? {
                ...r,
                status: u.status,
                kyc_level: u.kyc_level,
                is_admin: u.is_admin,
                is_blocked: u.blocked_reason !== null,
              }
            : r,
        ),
    );
  }, []);

  if (openPhone) {
    return <UserDetail phone={openPhone} onBack={() => setOpenPhone(null)} onUser={patchRow} />;
  }

  return (
    <>
      <header className="page-head">
        <div>
          <h1>Users</h1>
          <div className="sub">
            Browse newest accounts or search by phone; open a user to inspect wallets and
            apply enforcement.
          </div>
        </div>
        <div className="page-actions">
          <UserSearch width={300} onSelect={(u) => setOpenPhone(u.phone)} />
        </div>
      </header>

      {error && <Alert kind="error">{error}</Alert>}

      <div className="table-wrap">
        {rows === null ? (
          <div style={{ padding: "1rem", display: "flex", flexDirection: "column", gap: "0.7rem" }}>
            <Skeleton w="100%" h={18} />
            <Skeleton w="88%" h={18} />
            <Skeleton w="94%" h={18} />
          </div>
        ) : rows.length === 0 ? (
          <EmptyState icon="users" title="No users yet" />
        ) : (
          <table>
            <thead>
              <tr>
                <th>Phone</th>
                <th>Status</th>
                <th>KYC</th>
                <th>Joined</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((u) => (
                <tr
                  key={u.id}
                  className="clickable"
                  tabIndex={0}
                  onClick={() => setOpenPhone(u.phone)}
                  onKeyDown={(e) => e.key === "Enter" && setOpenPhone(u.phone)}
                >
                  <td className="mono">{u.phone}</td>
                  <td>
                    <StatusBadges u={u} />
                  </td>
                  <td>
                    <Badge tone={u.kyc_level > 0 ? "accent" : "neutral"}>
                      level {u.kyc_level}
                    </Badge>
                  </td>
                  <td className="muted">
                    <Ago ms={u.created_at_ms} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      {cursor && (
        <div className="load-more">
          <button onClick={loadMore} disabled={loadingMore}>
            {loadingMore ? "Loading…" : "Load more"}
          </button>
        </div>
      )}
    </>
  );
}
