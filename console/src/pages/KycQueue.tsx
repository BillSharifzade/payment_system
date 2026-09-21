import { useEffect, useMemo, useRef, useState } from "react";
import {
  KycSubmission,
  apiBlobUrl,
  approveKyc,
  describeError,
  getKycQueue,
  isAbort,
  rejectKyc,
} from "../api";
import { useOps } from "../ops";
import {
  Ago,
  Alert,
  Badge,
  ConfirmButton,
  EmptyState,
  IdChip,
  Modal,
  Segmented,
  Skeleton,
  SortableTh,
  useToast,
} from "../ui";

type SortKey = "created_at_ms" | "phone" | "full_name" | "requested_level";
type QueueStatus = "pending" | "approved" | "rejected";

/** Rows per request; the next page is keyed off the last row's cursor. */
const PAGE_SIZE = 50;

// The document endpoint requires the admin Bearer token, which <img>/<iframe>
// src requests never carry — so fetch the bytes with auth and render a blob URL.
function DocumentViewer({ documentRef }: { documentRef: string }) {
  // Keyed on the ref they were fetched for, so a stale result never shows.
  const [doc, setDoc] = useState<{ ref: string; url: string } | null>(null);
  const [failure, setFailure] = useState<{ ref: string; error: string } | null>(null);
  const url = doc?.ref === documentRef ? doc.url : null;
  const error = failure?.ref === documentRef ? failure.error : null;

  useEffect(() => {
    const ac = new AbortController();
    let objectUrl: string | null = null;
    apiBlobUrl(`/v1/admin/kyc/documents/${documentRef}`, ac.signal)
      .then((u) => {
        if (ac.signal.aborted) {
          URL.revokeObjectURL(u);
          return;
        }
        objectUrl = u;
        setDoc({ ref: documentRef, url: u });
      })
      .catch((e) => {
        if (!isAbort(e)) setFailure({ ref: documentRef, error: describeError(e) });
      });
    return () => {
      ac.abort();
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [documentRef]);

  if (error) return <Alert kind="error">Document failed to load: {error}</Alert>;
  if (!url) return <Skeleton w="100%" h={320} />;
  return documentRef.endsWith(".pdf") ? (
    // Sandboxed WITHOUT allow-same-origin: the blob: URL is same-origin with
    // the console, so an un-sandboxed frame would let a malicious PDF that
    // ever achieved script execution in the viewer read sessionStorage and
    // lift the admin tokens. allow-scripts alone is what Chrome's PDF viewer
    // needs to run; the frame's origin stays opaque.
    <iframe
      className="docframe docframe-pdf"
      sandbox="allow-scripts"
      src={url}
      title="Submitted document"
    />
  ) : (
    <img className="docframe" src={url} alt="Submitted document" />
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
      setError(describeError(e));
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
              <ConfirmButton
                className="approve"
                disabled={busy}
                confirmLabel={`Confirm: approve ${submission.full_name} to level ${submission.requested_level}?`}
                onConfirm={() =>
                  act(
                    () => approveKyc(submission.id),
                    `Approved ${submission.full_name} to level ${submission.requested_level}`,
                  )
                }
              >
                Approve level {submission.requested_level}
              </ConfirmButton>
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
  const { metrics, refreshMetrics } = useOps();
  const [status, setStatus] = useState<QueueStatus>("pending");
  const [gen, setGen] = useState(0);
  // The loaded page remembers which (tab, reload) it was fetched for; a
  // mismatch renders as "loading" — no state reset in an effect, and a slow
  // response for a tab the operator already left is simply never shown.
  const key = `${status}#${gen}`;
  const [page, setPage] = useState<{ key: string; items: KycSubmission[]; end: boolean } | null>(
    null,
  );
  const items = page?.key === key ? page.items : null;
  const end = page?.key === key ? page.end : false;
  const [more, setMore] = useState(false);
  const [open, setOpen] = useState<KycSubmission | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Oldest first by default: a review queue is fairest in FIFO order.
  const [sort, setSort] = useState<{ key: SortKey; dir: 1 | -1 }>({
    key: "created_at_ms",
    dir: 1,
  });
  // Aborts the first-page request and any "load more" for a superseded key.
  const acRef = useRef<AbortController | null>(null);

  useEffect(() => {
    const ac = new AbortController();
    acRef.current = ac;
    getKycQueue(status, { limit: PAGE_SIZE, signal: ac.signal })
      .then((list) => {
        setPage({ key, items: list, end: list.length < PAGE_SIZE });
        setError(null);
      })
      .catch((e) => {
        if (!isAbort(e)) setError(describeError(e));
      });
    return () => ac.abort();
  }, [key, status]);

  const reload = () => {
    setGen((g) => g + 1);
    refreshMetrics(); // the badge and the counts below come from metrics
  };

  const lastCursor = items && items.length > 0 ? items[items.length - 1].cursor : undefined;
  const canLoadMore = items !== null && !end && lastCursor !== undefined;

  async function loadMore() {
    const ac = acRef.current;
    if (!canLoadMore || more || !ac || !lastCursor) return;
    setMore(true);
    try {
      const list = await getKycQueue(status, {
        limit: PAGE_SIZE,
        after: lastCursor,
        signal: ac.signal,
      });
      setPage((p) =>
        p && p.key === key
          ? { key, items: [...p.items, ...list], end: list.length < PAGE_SIZE }
          : p,
      );
      setError(null);
    } catch (e) {
      if (!isAbort(e)) setError(describeError(e));
    } finally {
      setMore(false);
    }
  }

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

  // The true size of each queue comes from metrics, not from how much is loaded.
  const total = metrics ? metrics.kyc[status] : undefined;
  const remaining = items && total !== undefined ? Math.max(0, total - items.length) : 0;
  const countLabel = items
    ? remaining > 0
      ? `${items.length.toLocaleString()} of ${total!.toLocaleString()} ${status} loaded`
      : `${items.length.toLocaleString()} ${status}`
    : "";

  return (
    <>
      <header className="page-head">
        <div>
          <h1>KYC review</h1>
          <div className="sub">Identity submissions{countLabel ? ` · ${countLabel}` : ""}</div>
        </div>
        <div className="page-actions">
          <Segmented
            value={status}
            onChange={(v) => setStatus(v as QueueStatus)}
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

      {canLoadMore && (
        <div className="load-more">
          <button onClick={loadMore} disabled={more}>
            {more
              ? "Loading…"
              : remaining > 0
                ? `Load more (${remaining.toLocaleString()} remaining)`
                : "Load more"}
          </button>
        </div>
      )}

      {open && (
        <ReviewModal submission={open} onClose={() => setOpen(null)} onDone={reload} />
      )}
    </>
  );
}
