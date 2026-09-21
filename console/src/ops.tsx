// Ops data shared by the whole shell: the integrity status (30s) and the
// dashboard metrics (60s) are polled ONCE here, and every consumer (health
// pill, dashboard, KYC badge/counts) reads from this context — no page starts
// a second copy of the same poll. Polls pause while the tab is hidden.

import { ReactNode, createContext, useContext, useMemo } from "react";
import { AdminStatus, Metrics, getMetrics, getStatus } from "./api";
import { usePoll } from "./ui";

export type OpsData = {
  status: AdminStatus | null;
  statusError: string | null;
  metrics: Metrics | null;
  metricsError: string | null;
  metricsUpdatedAt: number | null;
  refreshStatus: () => void;
  refreshMetrics: () => void;
};

const noop = () => {};

const OpsCtx = createContext<OpsData>({
  status: null,
  statusError: null,
  metrics: null,
  metricsError: null,
  metricsUpdatedAt: null,
  refreshStatus: noop,
  refreshMetrics: noop,
});

export function useOps(): OpsData {
  return useContext(OpsCtx);
}

export function OpsProvider({ children }: { children: ReactNode }) {
  const s = usePoll(getStatus, 30_000);
  const m = usePoll(getMetrics, 60_000);
  const value = useMemo<OpsData>(
    () => ({
      status: s.data,
      statusError: s.error,
      metrics: m.data,
      metricsError: m.error,
      metricsUpdatedAt: m.updatedAt,
      refreshStatus: s.refresh,
      refreshMetrics: m.refresh,
    }),
    [s.data, s.error, s.refresh, m.data, m.error, m.updatedAt, m.refresh],
  );
  return <OpsCtx.Provider value={value}>{children}</OpsCtx.Provider>;
}
