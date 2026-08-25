import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { Metrics, formatMinor, getMetrics } from "../api";
import { BarChart, MixBar, Sparkline, StackedBars, TrendArea, fmtCompact } from "../charts";
import { Ago, Alert, Icon, Skeleton, StatTile, useAdminStatus } from "../ui";

const SERIES = {
  transfer: "var(--series-transfer)",
  deposit: "var(--series-deposit)",
  fx: "var(--series-fx)",
};

/** The last `days` ISO dates (UTC), oldest first — the shared x-domain. */
function dayDomain(days: number): string[] {
  const out: string[] = [];
  const now = Date.now();
  for (let i = days - 1; i >= 0; i--) {
    out.push(new Date(now - i * 86_400_000).toISOString().slice(0, 10));
  }
  return out;
}

/** Join sparse per-day rows onto a continuous day domain (missing days = 0). */
function onDomain(domain: string[], rows: { date: string; value: number }[]) {
  const byDate = new Map(rows.map((r) => [r.date, r.value]));
  return domain.map((d) => ({ label: d, value: byDate.get(d) ?? 0 }));
}

export default function Dashboard() {
  const { status, error: statusError } = useAdminStatus(30_000);
  const [metrics, setMetrics] = useState<Metrics | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [updatedAt, setUpdatedAt] = useState<number | null>(null);

  const load = useCallback(() => {
    getMetrics()
      .then((m) => {
        setMetrics(m);
        setError(null);
        setUpdatedAt(Date.now());
      })
      .catch((e) => setError(String((e as Error).message ?? e)));
  }, []);

  useEffect(() => {
    load();
    const t = setInterval(load, 60_000);
    return () => clearInterval(t);
  }, [load]);

  const conserved = status?.conservation.every((c) => c.net_minor === 0) ?? true;

  const domain30 = dayDomain(30);
  const domain14 = dayDomain(14);

  const volume = metrics
    ? onDomain(
        domain30,
        metrics.daily_volume
          .filter((v) => v.currency === "TJS")
          .map((v) => ({ date: v.date, value: v.volume_minor })),
      )
    : [];
  const txns = metrics
    ? onDomain(
        domain30,
        metrics.daily_transactions.map((v) => ({ date: v.date, value: v.count })),
      )
    : [];
  const newUsers = metrics
    ? onDomain(
        domain30,
        metrics.users.new_30d.map((v) => ({ date: v.date, value: v.count })),
      )
    : [];
  const decisions = metrics
    ? (() => {
        const byDate = new Map(metrics.kyc.decisions_14d.map((d) => [d.date, d]));
        return domain14.map((d) => ({
          label: d,
          parts: [byDate.get(d)?.approved ?? 0, byDate.get(d)?.rejected ?? 0],
        }));
      })()
    : [];

  const mixCount = (kind: string) =>
    metrics?.mix_30d.find((m) => m.kind === kind)?.count ?? 0;

  const fundsTjs = metrics?.customer_funds.find((c) => c.currency === "TJS")?.total_minor ?? 0;
  const fundsOther = metrics?.customer_funds.filter((c) => c.currency !== "TJS") ?? [];
  const newUsers7 = newUsers.slice(-7).reduce((a, p) => a + p.value, 0);

  return (
    <>
      <header className="page-head">
        <div>
          <h1>Dashboard</h1>
          <div className="sub">Money, risk and workload over the last 30 days.</div>
        </div>
        <div className="page-actions refresh-note">
          {updatedAt && (
            <span>
              updated <Ago ms={updatedAt} />
            </span>
          )}
          <button className="quiet" onClick={load} aria-label="Refresh now">
            <Icon name="refresh" size={15} />
            Refresh
          </button>
        </div>
      </header>

      {(error || statusError) && (
        <Alert kind="error" title="Dashboard data unavailable">
          {error ?? statusError}
        </Alert>
      )}

      {!metrics && !error ? (
        <>
          <div className="statgrid">
            {[0, 1, 2, 3, 4, 5].map((i) => (
              <div className="stat" key={i}>
                <Skeleton w={100} h={12} />
                <div style={{ height: 10 }} />
                <Skeleton w={70} h={24} />
              </div>
            ))}
          </div>
          <div className="grid-2">
            <div className="panel">
              <Skeleton w="100%" h={190} />
            </div>
            <div className="panel">
              <Skeleton w="100%" h={190} />
            </div>
          </div>
        </>
      ) : metrics ? (
        <>
          <div className="statgrid">
            <StatTile
              label="Customer funds"
              icon="bank"
              value={
                <>
                  {fmtCompact(fundsTjs / 100)} <span className="unit">TJS</span>
                </>
              }
              caption={
                fundsOther.length > 0
                  ? fundsOther
                      .map((c) => `+ ${formatMinor(c.total_minor, c.currency)}`)
                      .join(" · ")
                  : "held across all user wallets"
              }
            />
            <StatTile
              label="Money conservation"
              icon="shield"
              alarm={!conserved}
              tone={conserved ? "ok" : "bad"}
              value={
                conserved ? (
                  <>
                    <Icon name="check" size={18} /> Balanced
                  </>
                ) : (
                  <>
                    <Icon name="alert" size={18} /> BROKEN
                  </>
                )
              }
              caption={
                conserved ? "every currency nets to zero" : "page a human immediately"
              }
            />
            <StatTile
              label="Sealer backlog"
              icon="file"
              value={status ? status.unsealed_transactions.toLocaleString() : "—"}
              tone={(status?.unsealed_transactions ?? 0) > 5000 ? "warn" : undefined}
              caption={
                status?.latest_checkpoint ? (
                  <>
                    checkpoint #{status.latest_checkpoint.seq} ·{" "}
                    <Ago ms={status.latest_checkpoint.created_at_ms} />
                  </>
                ) : (
                  "no checkpoint yet"
                )
              }
            />
            <StatTile
              label="Users"
              icon="users"
              value={metrics.users.total.toLocaleString()}
              caption={`${newUsers7.toLocaleString()} new in 7 days`}
            >
              <Sparkline values={newUsers.map((p) => p.value)} color="var(--accent-strong)" />
            </StatTile>
            <StatTile
              label="AML blocks · 30d"
              icon="block"
              value={metrics.aml_blocked_30d.toLocaleString()}
              tone={metrics.aml_blocked_30d > 0 ? "warn" : undefined}
              caption={`${metrics.users.blocked.toLocaleString()} users on the blocklist`}
            />
            <StatTile
              label="Pending KYC"
              icon="shield"
              value={metrics.kyc.pending.toLocaleString()}
              tone={metrics.kyc.pending > 0 ? "warn" : undefined}
              caption={
                metrics.kyc.pending > 0 ? (
                  <Link to="/kyc">Review the queue →</Link>
                ) : (
                  "queue is clear"
                )
              }
            />
          </div>

          <div className="grid-2">
            <div className="panel">
              <div className="panel-title">
                <Icon name="chart" size={13} />
                Value moved per day · TJS · 30d
              </div>
              <TrendArea
                data={volume}
                color={SERIES.transfer}
                fmt={(v) => fmtCompact(v / 100)}
                tipValue={(p) => <>{formatMinor(p.value, "TJS")} moved</>}
              />
            </div>
            <div className="panel">
              <div className="panel-title">
                <Icon name="chart" size={13} />
                Transactions per day · 30d
              </div>
              <BarChart
                data={txns}
                color={SERIES.transfer}
                tipValue={(p) => <>{p.value.toLocaleString()} transactions</>}
              />
            </div>
          </div>

          <div className="grid-2">
            <div className="panel">
              <div className="panel-title">
                <Icon name="exchange" size={13} />
                Payment mix · 30d
              </div>
              <MixBar
                slices={[
                  { name: "Transfers", value: mixCount("transfer"), color: SERIES.transfer },
                  { name: "Deposits", value: mixCount("deposit"), color: SERIES.deposit },
                  { name: "FX", value: mixCount("fx"), color: SERIES.fx },
                ]}
              />
            </div>
            <div className="panel">
              <div className="panel-title">
                <Icon name="shield" size={13} />
                KYC decisions per day · 14d
              </div>
              <div className="row" style={{ marginBottom: "0.5rem" }}>
                <span className="badge warn">pending {metrics.kyc.pending}</span>
                <span className="badge ok">approved {metrics.kyc.approved}</span>
                <span className="badge bad">rejected {metrics.kyc.rejected}</span>
              </div>
              <StackedBars
                data={decisions}
                names={["Approved", "Rejected"]}
                colors={["var(--ok)", "var(--bad)"]}
              />
            </div>
          </div>

          <p className="refresh-note">
            <Icon name="info" size={14} />
            Auto-refreshes every 60s; integrity pill in the top bar updates every 30s. Deep
            metrics live in Grafana (<code>ssh -L 3000:localhost:3000</code>).
          </p>
        </>
      ) : null}
    </>
  );
}
