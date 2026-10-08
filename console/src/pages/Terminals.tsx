import { FormEvent, useEffect, useState } from "react";
import {
  AdminUser,
  ApiError,
  CreatedTerminal,
  Terminal,
  createTerminal,
  describeError,
  isAbort,
  listTerminals,
  lookupUser,
  parseTime,
  revokeTerminal,
} from "../api";
import {
  Ago,
  Alert,
  Badge,
  ConfirmButton,
  EmptyState,
  Icon,
  IdChip,
  Modal,
  Skeleton,
  UserSearch,
  copyText,
  useToast,
} from "../ui";

// Fingerprint terminals (contract §8). A merchant's physical terminal
// authenticates fingerprint check payments with an X-Terminal-Key. The key's
// plaintext exists exactly once — in the create response — so the console
// shows it in a dialog that cannot be dismissed until the operator confirms
// they stored it, then drops it from memory.

const LABEL_MAX = 64;

/** The one and only display of a new terminal's key. */
function KeyOnceDialog({ terminal, onDone }: { terminal: CreatedTerminal; onDone: () => void }) {
  const toast = useToast();
  const [stored, setStored] = useState(false);
  const [copied, setCopied] = useState(false);
  return (
    <Modal title="Terminal key — shown once" onClose={onDone} dismissible={false}>
      <Alert kind="warning" title="Store this key now">
        It is shown only this once and cannot be retrieved later. If it is lost, revoke the
        terminal and register a new one.
      </Alert>
      <dl className="kv">
        <dt>Terminal</dt>
        <dd>{terminal.label}</dd>
        <dt>Terminal id</dt>
        <dd>
          <IdChip id={terminal.id} short={false} />
        </dd>
      </dl>
      <label className="field" htmlFor="terminal-key">
        API key (X-Terminal-Key)
      </label>
      <div className="row nowrap">
        <input
          id="terminal-key"
          className="mono grow"
          readOnly
          value={terminal.api_key}
          onFocus={(e) => e.currentTarget.select()}
        />
        <button
          type="button"
          data-autofocus
          onClick={() =>
            copyText(terminal.api_key).then(
              () => {
                setCopied(true);
                toast("success", "Key copied to clipboard");
              },
              () => toast("error", "Could not copy — select the key and copy it manually"),
            )
          }
        >
          <Icon name={copied ? "check" : "copy"} size={14} />
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      <label className="check-row">
        <input type="checkbox" checked={stored} onChange={(e) => setStored(e.target.checked)} />
        I've stored this key on the terminal or in a secure place
      </label>
      <div className="row end">
        <button className="primary" disabled={!stored} onClick={onDone}>
          Done — hide the key
        </button>
      </div>
    </Modal>
  );
}

function TerminalStatus({ t }: { t: Terminal }) {
  const revoked = parseTime(t.revoked_at);
  if (t.revoked_at) {
    return (
      <Badge tone="bad" icon="block">
        revoked{revoked !== null ? <> <Ago ms={revoked} /></> : null}
      </Badge>
    );
  }
  return (
    <Badge tone="ok" icon="check">
      active
    </Badge>
  );
}

function When({ at, never }: { at: string | null; never: string }) {
  const ms = parseTime(at);
  return ms === null ? <span className="muted">{never}</span> : <Ago ms={ms} />;
}

export default function Terminals() {
  const toast = useToast();
  const [merchant, setMerchant] = useState<AdminUser | null>(null);
  const [lookupError, setLookupError] = useState<string | null>(null);
  const [gen, setGen] = useState(0);
  // Keyed on the merchant + reload it was fetched for, so a slow list for a
  // previous merchant is never shown under the current one.
  const listKey = merchant ? `${merchant.id}#${gen}` : null;
  const [list, setList] = useState<{ key: string; items: Terminal[] } | null>(null);
  const items = list && list.key === listKey ? list.items : null;
  const [listError, setListError] = useState<string | null>(null);
  const [label, setLabel] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [created, setCreated] = useState<CreatedTerminal | null>(null);

  useEffect(() => {
    if (!merchant || !listKey) return;
    const ac = new AbortController();
    listTerminals(merchant.id, ac.signal)
      .then((r) => {
        setList({ key: listKey, items: r.items });
        setListError(null);
      })
      .catch((e) => {
        if (!isAbort(e)) setListError(describeError(e));
      });
    return () => ac.abort();
  }, [merchant, listKey]);

  const reload = () => setGen((g) => g + 1);

  async function pickMerchant(phone: string) {
    setLookupError(null);
    setError(null);
    try {
      setMerchant(await lookupUser(phone));
    } catch (e) {
      setMerchant(null);
      setLookupError(
        e instanceof ApiError && e.status === 404
          ? "No user with that phone number."
          : describeError(e),
      );
    }
  }

  async function register(e: FormEvent) {
    e.preventDefault();
    const l = label.trim();
    if (!merchant || !l || busy) return;
    setBusy(true);
    setError(null);
    try {
      setCreated(await createTerminal(merchant.id, l));
      setLabel("");
    } catch (err) {
      setError(describeError(err));
    } finally {
      setBusy(false);
      reload();
    }
  }

  async function revoke(t: Terminal) {
    setError(null);
    try {
      await revokeTerminal(t.id);
      toast("success", `Revoked “${t.label}” — it can no longer take payments`);
    } catch (err) {
      setError(describeError(err));
    } finally {
      reload();
    }
  }

  return (
    <>
      <header className="page-head">
        <div>
          <h1>Fingerprint terminals</h1>
          <div className="sub">
            Register a merchant's fingerprint terminal to get its API key, see when each terminal
            was last used, and revoke lost or retired ones.
          </div>
        </div>
      </header>

      <div className="panel">
        <div className="panel-title">1 · Merchant</div>
        <div className="row">
          <div className="fund-search">
            <UserSearch
              autoFocus
              placeholder="Merchant phone (e.g. 99290…)"
              onSelect={(u) => void pickMerchant(u.phone)}
            />
          </div>
          {merchant && (
            <span className="row tight">
              <span className="mono">{merchant.phone}</span>
              <IdChip id={merchant.id} />
              <Badge tone={merchant.kyc_level > 0 ? "accent" : "neutral"}>
                KYC {merchant.kyc_level}
              </Badge>
              {merchant.status !== "active" && <Badge tone="bad">{merchant.status}</Badge>}
            </span>
          )}
        </div>
        {lookupError && <Alert kind="error">{lookupError}</Alert>}
        {merchant && merchant.status !== "active" && (
          <Alert kind="warning">
            This account is {merchant.status}; its checks cannot be paid until it is active again.
          </Alert>
        )}
      </div>

      {merchant && (
        <>
          <form className="panel" onSubmit={register}>
            <div className="panel-title">
              <Icon name="plus" size={13} />
              2 · Register a terminal
            </div>
            <div className="row">
              <label className="field grow">
                Label (where it is, so it can be told apart later)
                <input
                  placeholder="e.g. Shop 1 — till 2"
                  maxLength={LABEL_MAX}
                  value={label}
                  onChange={(e) => setLabel(e.target.value)}
                />
              </label>
              <button className="primary align-end" disabled={busy || !label.trim()}>
                <Icon name="key" size={15} />
                {busy ? "Registering…" : "Register terminal"}
              </button>
            </div>
            {error && <Alert kind="error">{error}</Alert>}
          </form>

          {listError && <Alert kind="error">{listError}</Alert>}
          <div className="table-wrap">
            {items === null ? (
              <div className="skeleton-stack">
                <Skeleton w="100%" h={18} />
                <Skeleton w="80%" h={18} />
              </div>
            ) : items.length === 0 ? (
              <EmptyState
                icon="fingerprint"
                title="No terminals for this merchant"
                hint="Register one above to get its API key."
              />
            ) : (
              <table>
                <thead>
                  <tr>
                    <th>Label</th>
                    <th>Terminal</th>
                    <th>Registered</th>
                    <th>Last used</th>
                    <th>Status</th>
                    <th>
                      <span className="sr-only">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {items.map((t) => (
                    <tr key={t.id}>
                      <td>{t.label}</td>
                      <td>
                        <IdChip id={t.id} />
                      </td>
                      <td>
                        <When at={t.created_at} never="—" />
                      </td>
                      <td>
                        <When at={t.last_used_at} never="never" />
                      </td>
                      <td>
                        <TerminalStatus t={t} />
                      </td>
                      <td className="num">
                        {!t.revoked_at && (
                          <ConfirmButton
                            className="danger"
                            confirmLabel={`Confirm revoke “${t.label}”?`}
                            onConfirm={() => void revoke(t)}
                          >
                            Revoke
                          </ConfirmButton>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        </>
      )}

      {created && <KeyOnceDialog terminal={created} onDone={() => setCreated(null)} />}
    </>
  );
}
