// Ops data shared by the whole shell: the integrity status (30s), the
// dashboard metrics (60s) and the pending-deposit queue (30s) are polled ONCE
// here, and every consumer (health pill, dashboard, nav badges, queue counts)
// reads from this context — no page starts a second copy of the same poll.
// Polls pause while the tab is hidden, and are flagged as background requests
// so they never keep an idle session alive (see api.ts / idle.tsx).

import { ReactNode, createContext, useContext, useMemo } from "react";
import {
  AdminStatus,
  DepositPage,
  Metrics,
  PENDING_BADGE_LIMIT,
  pollMetrics,
  pollPendingDeposits,
  pollStatus,
} from "./api";
import { usePoll } from "./ui";

export type PendingDeposits = {
  /** Requests awaiting a second admin (up to PENDING_BADGE_LIMIT). */
  count: number;
  /** True when there are more than `count`. */
  more: boolean;
};

export type OpsData = {
  status: AdminStatus | null;
  statusError: string | null;
  metrics: Metrics | null;
  metricsError: string | null;
  metricsUpdatedAt: number | null;
  pendingDeposits: PendingDeposits | null;
  refreshStatus: () => void;
  refreshMetrics: () => void;
  refreshPendingDeposits: () => void;
};

const noop = () => {};

const OpsCtx = createContext<OpsData>({
  status: null,
  statusError: null,
  metrics: null,
  metricsError: null,
  metricsUpdatedAt: null,
  pendingDeposits: null,
  refreshStatus: noop,
  refreshMetrics: noop,
  refreshPendingDeposits: noop,
});

export function useOps(): OpsData {
  return useContext(OpsCtx);
}

export function pendingSummary(page: DepositPage | null): PendingDeposits | null {
  if (!page) return null;
  return {
    count: page.items.length,
    more: page.next_cursor !== null && page.items.length >= PENDING_BADGE_LIMIT,
  };
}

/** "3", or "50+" when the queue is longer than the badge fetches. */
export function pendingLabel(p: PendingDeposits): string {
  return `${p.count.toLocaleString()}${p.more ? "+" : ""}`;
}

export function OpsProvider({ children }: { children: ReactNode }) {
  const s = usePoll(pollStatus, 30_000);
  const m = usePoll(pollMetrics, 60_000);
  const d = usePoll(pollPendingDeposits, 30_000);
  const pendingDeposits = useMemo(() => pendingSummary(d.data), [d.data]);
  const value = useMemo<OpsData>(
    () => ({
      status: s.data,
      statusError: s.error,
      metrics: m.data,
      metricsError: m.error,
      metricsUpdatedAt: m.updatedAt,
      pendingDeposits,
      refreshStatus: s.refresh,
      refreshMetrics: m.refresh,
      refreshPendingDeposits: d.refresh,
    }),
    [
      s.data,
      s.error,
      s.refresh,
      m.data,
      m.error,
      m.updatedAt,
      m.refresh,
      pendingDeposits,
      d.refresh,
    ],
  );
  return <OpsCtx.Provider value={value}>{children}</OpsCtx.Provider>;
}
