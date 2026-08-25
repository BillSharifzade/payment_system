import { useCallback, useEffect, useMemo, useState } from "react";
import {
  KycSubmission,
  apiBlobUrl,
  approveKyc,
  getKycQueue,
  rejectKyc,
} from "../api";
import {
  Ago,
  Alert,
  Badge,
  EmptyState,
  IdChip,
  Modal,
  Segmented,
  Skeleton,
  SortableTh,
  useToast,
} from "../ui";

type SortKey = "created_at_ms" | "phone" | "full_name" | "requested_level";

// The document endpoint requires the admin Bearer token, which <img>/<iframe>
// src requests never carry — so fetch the bytes with auth and render a blob URL.
function DocumentViewer({ documentRef }: { documentRef: string }) {
  const [url, setUrl] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let revoked: string | null = null;
    let cancelled = false;
    setUrl(null);
    setError(null);
    apiBlobUrl(`/v1/admin/kyc/documents/${documentRef}`)
      .then((u) => {
        if (cancelled) {
          URL.revokeObjectURL(u);
        } else {
          revoked = u;
          setUrl(u);
        }
      })
      .catch((e) => {
        if (!cancelled) setError(String((e as Error).message ?? e));
      });
    return () => {
      cancelled = true;
      if (revoked) URL.revokeObjectURL(revoked);
    };
  }, [documentRef]);

  if (error) return <Alert kind="error">Document failed to load: {error}</Alert>;
  if (!url) return <Skeleton w="100%" h={320} />;
  return documentRef.endsWith(".pdf") ? (
    <iframe className="docframe" style={{ height: 480 }} src={url} title="document" />
  ) : (
    <img className="docframe" src={url} alt="submitted document" />
  );
}

function ReviewModal({
  submission,
  onClose,
  onDone,
}: {
  submission: KycSubmission;
  onClose: () => void;
  onDone: () => void;
}) {
  const toast = useToast();
  const [reason, setReason] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function act(fn: () => Promise<unknown>, done: string) {
    setBusy(true);
    setError(null);
    try {
      await fn();
      toast("success", done);
      onDone();
      onClose();
    } catch (e) {
      setError(String((e as Error).message ?? e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal title={`KYC review — ${submission.full_name}`} onClose={onClose} wide>
      <div
        style={{
          display: "grid",
          gridTemplateColumns: "minmax(280px, 1.4fr) minmax(240px, 1fr)",
          gap: "1.25rem",
          alignItems: "start",
        }}
      >
        <DocumentViewer documentRef={submission.document_ref} />
        <div>
          <dl className="kv">
            <dt>Name</dt>
            <dd>{submission.full_name}</dd>
            <dt>Phone</dt>
            <dd>{submission.phone}</dd>
            <dt>Requested level</dt>
            <dd>
              <Badge tone="accent">level {submission.requested_level}</Badge>
            </dd>
            <dt>Document type</dt>
            <dd>{submission.document_type || <span className="muted">unspecified</span>}</dd>
            <dt>Submitted</dt>
            <dd>
              <Ago ms={submission.created_at_ms} />
            </dd>
            <dt>User</dt>
            <dd>
              <IdChip id={submission.user_id} />
            </dd>
          </dl>

          {submission.status === "pending" ? (
            <div style={{ marginTop: "1.25rem", display: "flex", flexDirection: "column", gap: "0.6rem" }}>
              <button
                className="approve"
                disabled={busy}
                onClick={() =>
                  act(
                    () => approveKyc(submission.id),
                    `Approved ${submission.full_name} to level ${submission.requested_level}`,
                  )
                }
              >
                Approve level {submission.requested_level}
              </button>
              <label className="field">
                Rejection reason
                <input
                  placeholder="e.g. document unreadable"
                  value={reason}
                  onChange={(e) => setReason(e.target.value)}
                />
              </label>
              <button
                className="danger"
                disabled={busy || reason.trim().length < 3}
                onClick={() =>
                  act(
                    () => rejectKyc(submission.id, reason.trim()),
                    `Rejected ${submission.full_name}`,
                  )
                }
              >
                Reject
              </button>
            </div>
          ) : (
            <div style={{ marginTop: "1rem" }}>
              <Badge tone={submission.status === "approved" ? "ok" : "bad"}>
                {submission.status}
              </Badge>
            </div>
          )}
          {error && <Alert kind="error">{error}</Alert>}
        </div>
      </div>
    </Modal>
  );
}

export default function KycQueue() {
  const [status, setStatus] = useState("pending");
  const [items, setItems] = useState<KycSubmission[] | null>(null);
  const [open, setOpen] = useState<KycSubmission | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Oldest first by default: a review queue is fairest in FIFO order.
  const [sort, setSort] = useState<{ key: SortKey; dir: 1 | -1 }>({
    key: "created_at_ms",
    dir: 1,
  });

  const sorted = useMemo(() => {
    if (items === null) return null;
    return [...items].sort((a, b) => {
      const av = a[sort.key];
      const bv = b[sort.key];
      const cmp =
        typeof av === "number" && typeof bv === "number"
          ? av - bv
          : String(av).localeCompare(String(bv));
      return cmp * sort.dir;
    });
  }, [items, sort]);

  const toggleSort = (key: SortKey) =>
    setSort((s) => (s.key === key ? { key, dir: -s.dir as 1 | -1 } : { key, dir: 1 }));

  const reload = useCallback(() => {
    getKycQueue(status)
      .then((list) => {
        setItems(list);
        setError(null);
      })
      .catch((e) => setError(String((e as Error).message ?? e)));
  }, [status]);

  useEffect(() => {
    setItems(null);
    reload();
  }, [reload]);

  return (
    <>
      <header className="page-head">
        <div>
          <h1>KYC review</h1>
          <div className="sub">
            Identity submissions{items ? ` · ${items.length} ${status}` : ""}
          </div>
        </div>
        <div className="page-actions">
          <Segmented
            value={status}
            onChange={setStatus}
            options={[
              { value: "pending", label: "Pending" },
              { value: "approved", label: "Approved" },
              { value: "rejected", label: "Rejected" },
            ]}
          />
        </div>
      </header>

      {error && <Alert kind="error">{error}</Alert>}

      <div className="table-wrap">
        {items === null ? (
          <div style={{ padding: "1rem", display: "flex", flexDirection: "column", gap: "0.7rem" }}>
            <Skeleton w="100%" h={18} />
            <Skeleton w="85%" h={18} />
            <Skeleton w="92%" h={18} />
          </div>
        ) : items.length === 0 ? (
          <EmptyState
            title={`No ${status} submissions`}
            hint={status === "pending" ? "New submissions appear here for review." : undefined}
          />
        ) : (
          <table>
            <thead>
              <tr>
                <SortableTh
                  label="Submitted"
                  active={sort.key === "created_at_ms"}
                  dir={sort.dir}
                  onSort={() => toggleSort("created_at_ms")}
                />
                <SortableTh
                  label="Phone"
                  active={sort.key === "phone"}
                  dir={sort.dir}
                  onSort={() => toggleSort("phone")}
                />
                <SortableTh
                  label="Name"
                  active={sort.key === "full_name"}
                  dir={sort.dir}
                  onSort={() => toggleSort("full_name")}
                />
                <SortableTh
                  label="Level"
                  active={sort.key === "requested_level"}
                  dir={sort.dir}
                  onSort={() => toggleSort("requested_level")}
                />
                <th>Document</th>
              </tr>
            </thead>
            <tbody>
              {(sorted ?? []).map((it) => (
                <tr
                  key={it.id}
                  className="clickable"
                  onClick={() => setOpen(it)}
                  tabIndex={0}
                  onKeyDown={(e) => e.key === "Enter" && setOpen(it)}
                >
                  <td>
                    <Ago ms={it.created_at_ms} />
                  </td>
                  <td className="mono">{it.phone}</td>
                  <td>{it.full_name}</td>
                  <td>
                    <Badge tone="accent">level {it.requested_level}</Badge>
                  </td>
                  <td className="muted">{it.document_type || "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      {open && (
        <ReviewModal submission={open} onClose={() => setOpen(null)} onDone={reload} />
      )}
    </>
  );
}
