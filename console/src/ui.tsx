// Shared UI kit for the console: icons, badges, stat tiles, toasts, modal,
// confirm buttons, and small utilities. No dependencies beyond React.

import {
  ReactNode,
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
} from "react";
import { AdminStatus, UserListItem, getStatus, listUsers } from "./api";

// ----- Icons (inline, stroke-based) -------------------------------------------

const ICON_PATHS = {
  dashboard: (
    <>
      <rect x="3" y="3" width="7" height="9" rx="1.5" />
      <rect x="14" y="3" width="7" height="5" rx="1.5" />
      <rect x="14" y="12" width="7" height="9" rx="1.5" />
      <rect x="3" y="16" width="7" height="5" rx="1.5" />
    </>
  ),
  shield: (
    <>
      <path d="M12 3l7 3v5c0 4.5-3 8.5-7 10-4-1.5-7-5.5-7-10V6l7-3z" />
      <path d="M9.5 12l2 2 3.5-4" />
    </>
  ),
  users: (
    <>
      <circle cx="9" cy="8" r="3.5" />
      <path d="M3 20c0-3.3 2.7-6 6-6s6 2.7 6 6" />
      <path d="M16 4.6a3.5 3.5 0 010 6.8M17.5 14.4c2.1.8 3.5 2.9 3.5 5.6" />
    </>
  ),
  exchange: (
    <>
      <path d="M4 7h13l-3-3M20 17H7l3 3" />
    </>
  ),
  bank: (
    <>
      <path d="M3 9l9-5 9 5" />
      <path d="M5 9v8M9.5 9v8M14.5 9v8M19 9v8" />
      <path d="M3 20h18" />
    </>
  ),
  logout: (
    <>
      <path d="M9 4H6a2 2 0 00-2 2v12a2 2 0 002 2h3" />
      <path d="M15 8l4 4-4 4M19 12H9" />
    </>
  ),
  search: (
    <>
      <circle cx="11" cy="11" r="6.5" />
      <path d="M16 16l4.5 4.5" />
    </>
  ),
  x: <path d="M6 6l12 12M18 6L6 18" />,
  check: <path d="M4.5 12.5l5 5 10-11" />,
  alert: (
    <>
      <path d="M12 4L2.5 20h19L12 4z" />
      <path d="M12 10v4.5M12 17.5v.5" />
    </>
  ),
  info: (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M12 11v5M12 8v.5" />
    </>
  ),
  copy: (
    <>
      <rect x="9" y="9" width="11" height="11" rx="2" />
      <path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1" />
    </>
  ),
  refresh: (
    <>
      <path d="M20 12a8 8 0 11-2.9-6.2" />
      <path d="M20 3.5V7h-3.5" />
    </>
  ),
  clock: (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M12 7.5V12l3 2" />
    </>
  ),
  file: (
    <>
      <path d="M6 2.5h8L19 7.5v13a1 1 0 01-1 1H6a1 1 0 01-1-1v-17a1 1 0 011-1z" />
      <path d="M13.5 2.5v5.5H19" />
    </>
  ),
  snowflake: (
    <>
      <path d="M12 3v18M4.2 7.5l15.6 9M4.2 16.5l15.6-9" />
    </>
  ),
  block: (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M6 6l12 12" />
    </>
  ),
  inbox: (
    <>
      <path d="M3 13l3-8h12l3 8v6a1 1 0 01-1 1H4a1 1 0 01-1-1v-6z" />
      <path d="M3 13h5.5a3.5 3.5 0 007 0H21" />
    </>
  ),
  caret: <path d="M6 9l6 6 6-6" />,
  back: <path d="M15 5l-7 7 7 7" />,
  chart: (
    <>
      <path d="M3 3v18h18" />
      <path d="M7 14l4-5 3 3 5-7" />
    </>
  ),
} as const;

export type IconName = keyof typeof ICON_PATHS;

export function Icon({ name, size = 17 }: { name: IconName; size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.8}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {ICON_PATHS[name]}
    </svg>
  );
}

// ----- Badge -------------------------------------------------------------------

export function Badge({
  tone = "neutral",
  icon,
  children,
}: {
  tone?: "neutral" | "ok" | "bad" | "warn" | "accent";
  icon?: IconName;
  children: ReactNode;
}) {
  return (
    <span className={`badge ${tone === "neutral" ? "" : tone}`}>
      {icon && <Icon name={icon} size={12} />}
      {children}
    </span>
  );
}

// ----- Stat tile -----------------------------------------------------------------

export function StatTile({
  label,
  icon,
  value,
  tone,
  caption,
  alarm,
  children,
}: {
  label: string;
  icon: IconName;
  value: ReactNode;
  tone?: "ok" | "bad" | "warn";
  caption?: ReactNode;
  alarm?: boolean;
  children?: ReactNode;
}) {
  return (
    <div className={`stat ${alarm ? "alarm" : ""}`}>
      <div className="stat-head">
        <Icon name={icon} size={15} />
        {label}
      </div>
      <div className={`value ${tone ?? ""}`}>{value}</div>
      {caption && <div className="caption">{caption}</div>}
      {children}
    </div>
  );
}

// ----- Toasts --------------------------------------------------------------------

type ToastKind = "success" | "error";
type Toast = { id: number; kind: ToastKind; msg: string };
type PushToast = (kind: ToastKind, msg: string) => void;

const ToastCtx = createContext<PushToast>(() => {});

export function useToast(): PushToast {
  return useContext(ToastCtx);
}

let toastSeq = 0;

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const push = useCallback<PushToast>((kind, msg) => {
    const id = ++toastSeq;
    setToasts((t) => [...t.slice(-3), { id, kind, msg }]);
    setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), 4500);
  }, []);
  return (
    <ToastCtx.Provider value={push}>
      {children}
      <div className="toasts" role="status" aria-live="polite">
        {toasts.map((t) => (
          <div key={t.id} className={`toast ${t.kind}`}>
            <Icon name={t.kind === "success" ? "check" : "alert"} size={15} />
            <div>{t.msg}</div>
          </div>
        ))}
      </div>
    </ToastCtx.Provider>
  );
}

// ----- Modal ---------------------------------------------------------------------

export function Modal({
  title,
  onClose,
  wide,
  children,
}: {
  title: string;
  onClose: () => void;
  wide?: boolean;
  children: ReactNode;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    document.body.style.overflow = "hidden";
    ref.current?.focus();
    return () => {
      document.removeEventListener("keydown", onKey);
      document.body.style.overflow = "";
    };
  }, [onClose]);
  return (
    <div
      className="modal-backdrop"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className={`modal ${wide ? "wide" : ""}`}
        role="dialog"
        aria-modal="true"
        aria-label={title}
        tabIndex={-1}
        ref={ref}
      >
        <div className="modal-head">
          <h3>{title}</h3>
          <button className="icon-btn" onClick={onClose} aria-label="Close">
            <Icon name="x" />
          </button>
        </div>
        <div className="modal-body">{children}</div>
      </div>
    </div>
  );
}

// ----- Confirm button (two-click, disarms after 3s) --------------------------------

export function ConfirmButton({
  onConfirm,
  confirmLabel = "Confirm?",
  className,
  disabled,
  children,
}: {
  onConfirm: () => void;
  confirmLabel?: string;
  className?: string;
  disabled?: boolean;
  children: ReactNode;
}) {
  const [armed, setArmed] = useState(false);
  useEffect(() => {
    if (!armed) return;
    const t = setTimeout(() => setArmed(false), 3000);
    return () => clearTimeout(t);
  }, [armed]);
  return (
    <button
      className={className}
      disabled={disabled}
      onClick={() => {
        if (armed) {
          setArmed(false);
          onConfirm();
        } else {
          setArmed(true);
        }
      }}
    >
      {armed ? confirmLabel : children}
    </button>
  );
}

// ----- Segmented control ------------------------------------------------------------

export function Segmented({
  options,
  value,
  onChange,
}: {
  options: { value: string; label: string }[];
  value: string;
  onChange: (v: string) => void;
}) {
  return (
    <div className="segmented" role="tablist">
      {options.map((o) => (
        <button
          key={o.value}
          role="tab"
          aria-selected={o.value === value}
          className={o.value === value ? "active" : ""}
          onClick={() => onChange(o.value)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

// ----- Empty state & skeleton ---------------------------------------------------------

export function EmptyState({
  icon = "inbox",
  title,
  hint,
}: {
  icon?: IconName;
  title: string;
  hint?: string;
}) {
  return (
    <div className="empty">
      <div>
        <Icon name={icon} size={28} />
        <div className="empty-title">{title}</div>
        {hint && <div className="empty-hint">{hint}</div>}
      </div>
    </div>
  );
}

export function Skeleton({ w, h = 16 }: { w: number | string; h?: number }) {
  return <div className="skeleton" style={{ width: w, height: h }} />;
}

// ----- Alert box -------------------------------------------------------------------

export function Alert({
  kind,
  title,
  children,
}: {
  kind: "error" | "success" | "warning";
  title?: string;
  children: ReactNode;
}) {
  return (
    <div className={`alert ${kind}`} role={kind === "error" ? "alert" : undefined}>
      <Icon name={kind === "success" ? "check" : "alert"} size={15} />
      <div>
        {title && <div className="alert-title">{title}</div>}
        <div className="alert-body">{children}</div>
      </div>
    </div>
  );
}

// ----- Copyable id chip ----------------------------------------------------------------

// navigator.clipboard exists only in a secure context (HTTPS/localhost); over
// plain HTTP it is undefined. Fall back to the legacy execCommand path so copy
// works on an IP-based LAN deployment too.
function copyText(text: string): Promise<void> {
  if (navigator.clipboard?.writeText) return navigator.clipboard.writeText(text);
  return new Promise((resolve, reject) => {
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand("copy");
      document.body.removeChild(ta);
      ok ? resolve() : reject(new Error("copy rejected"));
    } catch (e) {
      reject(e);
    }
  });
}

export function IdChip({ id, short = true }: { id: string; short?: boolean }) {
  const toast = useToast();
  return (
    <button
      className="id-chip"
      title={`${id} — click to copy`}
      onClick={() => {
        copyText(id)
          .then(() => toast("success", "Copied to clipboard"))
          .catch(() => toast("error", "Could not copy"));
      }}
    >
      {short ? `${id.slice(0, 8)}…` : id}
      <Icon name="copy" size={11} />
    </button>
  );
}

// ----- Time helpers -----------------------------------------------------------------

export function timeAgo(ms: number): string {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 10) return "just now";
  if (s < 60) return `${s}s ago`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60);
  if (h < 48) return `${h}h ago`;
  return new Date(ms).toLocaleDateString();
}

/** Relative time that also carries the absolute moment as a tooltip. */
export function Ago({ ms }: { ms: number }) {
  return <span title={new Date(ms).toLocaleString()}>{timeAgo(ms)}</span>;
}

// ----- Custom select (no native <select>) ----------------------------------------------

/** Close a popover when clicking anywhere outside `ref`. */
function useClickOutside(ref: React.RefObject<HTMLElement>, onOutside: () => void) {
  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onOutside();
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [ref, onOutside]);
}

export type SelectOption = { value: string; label: ReactNode; hint?: ReactNode };

export function Select({
  value,
  onChange,
  options,
  placeholder = "Select…",
  width,
}: {
  value: string;
  onChange: (v: string) => void;
  options: SelectOption[];
  placeholder?: string;
  width?: number | string;
}) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const wrap = useRef<HTMLDivElement>(null);
  useClickOutside(wrap, () => setOpen(false));

  const current = options.find((o) => o.value === value);

  function onKey(e: React.KeyboardEvent) {
    if (e.key === "Escape") setOpen(false);
    else if (e.key === "ArrowDown") {
      e.preventDefault();
      if (!open) setOpen(true);
      else setActive((a) => Math.min(a + 1, options.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((a) => Math.max(a - 1, 0));
    } else if (e.key === "Enter" && open) {
      e.preventDefault();
      const o = options[active];
      if (o) {
        onChange(o.value);
        setOpen(false);
      }
    }
  }

  return (
    <div className="select-wrap" ref={wrap} style={{ width }} onKeyDown={onKey}>
      <button
        type="button"
        className="select-btn"
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => {
          setOpen((o) => !o);
          setActive(Math.max(0, options.findIndex((o) => o.value === value)));
        }}
      >
        <span>{current ? current.label : <span className="muted">{placeholder}</span>}</span>
        <span className="select-caret">
          <Icon name="caret" size={14} />
        </span>
      </button>
      {open && (
        <div className="popover" role="listbox">
          {options.length === 0 && <div className="popover-empty">No options</div>}
          {options.map((o, i) => (
            <div
              key={o.value}
              role="option"
              aria-selected={o.value === value}
              className={`option ${i === active ? "active" : ""} ${o.value === value ? "selected" : ""}`}
              onMouseEnter={() => setActive(i)}
              onClick={() => {
                onChange(o.value);
                setOpen(false);
              }}
            >
              <span>{o.label}</span>
              {o.hint && <span className="option-hint">{o.hint}</span>}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// ----- User search combobox --------------------------------------------------------------

/** Server-backed phone typeahead over the admin user directory. */
export function UserSearch({
  onSelect,
  placeholder = "Search by phone…",
  autoFocus,
  width,
}: {
  onSelect: (u: UserListItem) => void;
  placeholder?: string;
  autoFocus?: boolean;
  width?: number | string;
}) {
  const [q, setQ] = useState("");
  const [results, setResults] = useState<UserListItem[]>([]);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const [busy, setBusy] = useState(false);
  const wrap = useRef<HTMLDivElement>(null);
  useClickOutside(wrap, () => setOpen(false));

  // Debounced server search on digits.
  useEffect(() => {
    const digits = q.replace(/\D/g, "");
    if (digits.length < 3) {
      setResults([]);
      setBusy(false);
      return;
    }
    setBusy(true);
    const t = setTimeout(() => {
      listUsers({ q: digits, limit: 8 })
        .then((r) => {
          setResults(r.users);
          setActive(0);
          setOpen(true);
        })
        .catch(() => setResults([]))
        .finally(() => setBusy(false));
    }, 220);
    return () => clearTimeout(t);
  }, [q]);

  function pick(u: UserListItem) {
    setOpen(false);
    setQ(u.phone);
    onSelect(u);
  }

  function onKey(e: React.KeyboardEvent) {
    if (!open) return;
    if (e.key === "Escape") setOpen(false);
    else if (e.key === "ArrowDown") {
      e.preventDefault();
      setActive((a) => Math.min(a + 1, results.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((a) => Math.max(a - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      const u = results[active];
      if (u) pick(u);
    }
  }

  return (
    <div className="select-wrap" ref={wrap} style={{ width }}>
      <input
        placeholder={placeholder}
        value={q}
        autoFocus={autoFocus}
        onChange={(e) => setQ(e.target.value)}
        onFocus={() => results.length > 0 && setOpen(true)}
        onKeyDown={onKey}
        role="combobox"
        aria-expanded={open}
        aria-autocomplete="list"
        style={{ width: "100%" }}
      />
      {open && (
        <div className="popover" role="listbox">
          {busy && results.length === 0 && <div className="popover-empty">Searching…</div>}
          {!busy && results.length === 0 && (
            <div className="popover-empty">No matching phone numbers</div>
          )}
          {results.map((u, i) => (
            <div
              key={u.id}
              role="option"
              aria-selected={i === active}
              className={`option ${i === active ? "active" : ""}`}
              onMouseEnter={() => setActive(i)}
              onClick={() => pick(u)}
            >
              <span className="mono">{u.phone}</span>
              <span className="option-hint">
                KYC {u.kyc_level}
                {u.status !== "active" ? ` · ${u.status}` : ""}
                {u.is_blocked ? " · blocked" : ""}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// ----- Shared admin-status polling ----------------------------------------------------

export function useAdminStatus(intervalMs: number) {
  const [status, setStatus] = useState<AdminStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [updatedAt, setUpdatedAt] = useState<number | null>(null);
  const [bump, setBump] = useState(0);

  useEffect(() => {
    let live = true;
    const tick = () =>
      getStatus()
        .then((s) => {
          if (!live) return;
          setStatus(s);
          setError(null);
          setUpdatedAt(Date.now());
        })
        .catch((e) => live && setError(String((e as Error).message ?? e)));
    tick();
    const t = setInterval(tick, intervalMs);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, [intervalMs, bump]);

  const refresh = useCallback(() => setBump((b) => b + 1), []);
  return { status, error, updatedAt, refresh };
}

// ----- Sortable table headers ---------------------------------------------------------

/** Column header that drives a client-side sort; pairs with th.sortable CSS. */
export function SortableTh({
  label,
  active,
  dir,
  onSort,
  className,
}: {
  label: string;
  active: boolean;
  dir: 1 | -1;
  onSort: () => void;
  className?: string;
}) {
  return (
    <th
      className={`sortable${className ? ` ${className}` : ""}`}
      aria-sort={active ? (dir === 1 ? "ascending" : "descending") : undefined}
      onClick={onSort}
    >
      {label}
      {active && <span className="sort-arrow">{dir === 1 ? "▲" : "▼"}</span>}
    </th>
  );
}
