// Dependency-free SVG charts for the dashboard. Follows the house dataviz
// rules: thin marks, recessive grid, one axis, tooltips on hover, text in ink
// tokens (never the series color), legends + direct labels for identity.

import { ReactNode, useRef, useState } from "react";

export type Point = { label: string; value: number };

const W = 640;
const PAD = { l: 46, r: 12, t: 10, b: 22 };

/** Round up to a "nice" 1/2/5 × 10^k ceiling so gridlines land on round numbers. */
function niceMax(v: number): number {
  if (v <= 0) return 1;
  const exp = Math.floor(Math.log10(v));
  const base = Math.pow(10, exp);
  for (const m of [1, 2, 5, 10]) {
    if (v <= m * base) return m * base;
  }
  return 10 * base;
}

/** Compact number: 950 · 12.3K · 4.5M. */
export function fmtCompact(n: number): string {
  const abs = Math.abs(n);
  if (abs >= 1e12) return `${(n / 1e12).toFixed(1)}T`;
  if (abs >= 1e9) return `${(n / 1e9).toFixed(1)}B`;
  if (abs >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (abs >= 1e3) return `${(n / 1e3).toFixed(1)}K`;
  return String(Math.round(n));
}

/** "2026-07-09" → "9 Jul". */
export function fmtDay(iso: string): string {
  const d = new Date(`${iso}T00:00:00Z`);
  return d.toLocaleDateString(undefined, { day: "numeric", month: "short", timeZone: "UTC" });
}

function xAt(i: number, n: number): number {
  if (n <= 1) return PAD.l + (W - PAD.l - PAD.r) / 2;
  return PAD.l + (i / (n - 1)) * (W - PAD.l - PAD.r);
}

/** Shared hover plumbing: maps mouse x → nearest data index. */
function useNearestIndex(n: number) {
  const wrap = useRef<HTMLDivElement>(null);
  const [idx, setIdx] = useState<number | null>(null);
  const onMove = (e: React.MouseEvent) => {
    const rect = wrap.current?.getBoundingClientRect();
    if (!rect || n === 0) return;
    const fx = ((e.clientX - rect.left) / rect.width) * W;
    const t = (fx - PAD.l) / (W - PAD.l - PAD.r);
    setIdx(Math.max(0, Math.min(n - 1, Math.round(t * (n - 1)))));
  };
  return { wrap, idx, onMove, onLeave: () => setIdx(null) };
}

function Frame({
  h,
  yMax,
  fmt,
  xLabels,
  children,
}: {
  h: number;
  yMax: number;
  fmt: (v: number) => string;
  xLabels: { i: number; n: number; text: string }[];
  children: ReactNode;
}) {
  const ticks = [0, yMax / 2, yMax];
  const yAt = (v: number) => h - PAD.b - (v / yMax) * (h - PAD.t - PAD.b);
  return (
    <svg viewBox={`0 0 ${W} ${h}`} role="img">
      {ticks.map((v) => (
        <g key={v}>
          <line
            x1={PAD.l}
            x2={W - PAD.r}
            y1={yAt(v)}
            y2={yAt(v)}
            stroke="var(--grid)"
            strokeWidth={1}
          />
          <text
            x={PAD.l - 7}
            y={yAt(v) + 3}
            textAnchor="end"
            fontSize={10}
            fill="var(--muted)"
          >
            {fmt(v)}
          </text>
        </g>
      ))}
      {xLabels.map(({ i, n, text }) => (
        <text
          key={`${i}-${text}`}
          x={xAt(i, n)}
          y={h - 6}
          textAnchor="middle"
          fontSize={10}
          fill="var(--muted)"
        >
          {text}
        </text>
      ))}
      {children}
    </svg>
  );
}

function pickXLabels(data: Point[]): { i: number; n: number; text: string }[] {
  const n = data.length;
  if (n === 0) return [];
  const count = Math.min(5, n);
  const out = [];
  for (let k = 0; k < count; k++) {
    const i = Math.round((k / Math.max(1, count - 1)) * (n - 1));
    out.push({ i, n, text: fmtDay(data[i].label) });
  }
  return out;
}

function Tip({ x, y, children }: { x: number; y: number; children: ReactNode }) {
  return (
    <div className="chart-tip" style={{ left: `${(x / W) * 100}%`, top: `${y}%` }}>
      {children}
    </div>
  );
}

/** Area trend for a single magnitude series (sequential: one hue). */
export function TrendArea({
  data,
  color,
  fmt,
  tipValue,
  height = 190,
}: {
  data: Point[];
  color: string;
  fmt?: (v: number) => string;
  /** Full-precision tooltip line for a point. */
  tipValue: (p: Point) => ReactNode;
  height?: number;
}) {
  const f = fmt ?? fmtCompact;
  const { wrap, idx, onMove, onLeave } = useNearestIndex(data.length);
  const yMax = niceMax(Math.max(...data.map((d) => d.value), 1));
  const yAt = (v: number) => height - PAD.b - (v / yMax) * (height - PAD.t - PAD.b);
  const n = data.length;

  const line = data.map((d, i) => `${i === 0 ? "M" : "L"}${xAt(i, n)},${yAt(d.value)}`).join(" ");
  const area = `${line} L${xAt(n - 1, n)},${yAt(0)} L${xAt(0, n)},${yAt(0)} Z`;

  return (
    <div className="chart-wrap" ref={wrap} onMouseMove={onMove} onMouseLeave={onLeave}>
      <Frame h={height} yMax={yMax} fmt={f} xLabels={pickXLabels(data)}>
        {n > 0 && (
          <>
            <path d={area} fill={color} opacity={0.14} />
            <path d={line} fill="none" stroke={color} strokeWidth={2} strokeLinejoin="round" />
          </>
        )}
        {idx !== null && n > 0 && (
          <>
            <line
              x1={xAt(idx, n)}
              x2={xAt(idx, n)}
              y1={PAD.t}
              y2={height - PAD.b}
              stroke="var(--border-strong)"
              strokeWidth={1}
            />
            <circle
              cx={xAt(idx, n)}
              cy={yAt(data[idx].value)}
              r={4}
              fill={color}
              stroke="var(--surface)"
              strokeWidth={2}
            />
          </>
        )}
      </Frame>
      {idx !== null && n > 0 && (
        <Tip x={xAt(idx, n)} y={(yAt(data[idx].value) / height) * 100}>
          <div className="tip-title">{fmtDay(data[idx].label)}</div>
          <div className="tip-row">
            <span className="swatch" style={{ background: color }} />
            {tipValue(data[idx])}
          </div>
        </Tip>
      )}
    </div>
  );
}

/** Daily bars for a count series. */
export function BarChart({
  data,
  color,
  fmt,
  tipValue,
  height = 190,
}: {
  data: Point[];
  color: string;
  fmt?: (v: number) => string;
  tipValue: (p: Point) => ReactNode;
  height?: number;
}) {
  const f = fmt ?? fmtCompact;
  const { wrap, idx, onMove, onLeave } = useNearestIndex(data.length);
  const yMax = niceMax(Math.max(...data.map((d) => d.value), 1));
  const yAt = (v: number) => height - PAD.b - (v / yMax) * (height - PAD.t - PAD.b);
  const n = data.length;
  const slot = (W - PAD.l - PAD.r) / Math.max(1, n);
  const bw = Math.max(2, Math.min(18, slot - 2)); // 2px surface gap between bars

  return (
    <div className="chart-wrap" ref={wrap} onMouseMove={onMove} onMouseLeave={onLeave}>
      <Frame h={height} yMax={yMax} fmt={f} xLabels={pickXLabels(data)}>
        {data.map((d, i) => {
          const x = xAt(i, n) - bw / 2;
          const y = yAt(d.value);
          const hgt = Math.max(0, yAt(0) - y);
          return (
            <rect
              key={d.label}
              x={x}
              y={y}
              width={bw}
              height={hgt}
              rx={Math.min(3, bw / 2)}
              fill={color}
              opacity={idx === null || idx === i ? 1 : 0.45}
            />
          );
        })}
      </Frame>
      {idx !== null && n > 0 && (
        <Tip x={xAt(idx, n)} y={(yAt(data[idx].value) / height) * 100}>
          <div className="tip-title">{fmtDay(data[idx].label)}</div>
          <div className="tip-row">
            <span className="swatch" style={{ background: color }} />
            {tipValue(data[idx])}
          </div>
        </Tip>
      )}
    </div>
  );
}

/** Stacked daily bars (e.g. approvals vs rejections). Status colors allowed —
 *  the series ARE statuses. */
export function StackedBars({
  data,
  names,
  colors,
  height = 170,
}: {
  data: { label: string; parts: number[] }[];
  names: string[];
  colors: string[];
  height?: number;
}) {
  const { wrap, idx, onMove, onLeave } = useNearestIndex(data.length);
  const totals = data.map((d) => d.parts.reduce((a, b) => a + b, 0));
  const yMax = niceMax(Math.max(...totals, 1));
  const yAt = (v: number) => height - PAD.b - (v / yMax) * (height - PAD.t - PAD.b);
  const n = data.length;
  const slot = (W - PAD.l - PAD.r) / Math.max(1, n);
  const bw = Math.max(3, Math.min(22, slot - 2));

  return (
    <div className="chart-wrap" ref={wrap} onMouseMove={onMove} onMouseLeave={onLeave}>
      <Frame
        h={height}
        yMax={yMax}
        fmt={fmtCompact}
        xLabels={pickXLabels(data.map((d) => ({ label: d.label, value: 0 })))}
      >
        {data.map((d, i) => {
          let acc = 0;
          return d.parts.map((v, s) => {
            const y0 = yAt(acc);
            acc += v;
            const y1 = yAt(acc);
            if (v === 0) return null;
            return (
              <rect
                key={`${d.label}-${s}`}
                x={xAt(i, n) - bw / 2}
                y={y1}
                width={bw}
                // 2px surface gap between stacked segments
                height={Math.max(0, y0 - y1 - (s < d.parts.length - 1 ? 2 : 0) + 0)}
                rx={2}
                fill={colors[s]}
                opacity={idx === null || idx === i ? 1 : 0.45}
              />
            );
          });
        })}
      </Frame>
      {idx !== null && n > 0 && (
        <Tip x={xAt(idx, n)} y={(yAt(totals[idx]) / height) * 100}>
          <div className="tip-title">{fmtDay(data[idx].label)}</div>
          {names.map((name, s) => (
            <div className="tip-row" key={name}>
              <span className="swatch" style={{ background: colors[s] }} />
              {name}: {data[idx].parts[s].toLocaleString()}
            </div>
          ))}
        </Tip>
      )}
      <div className="legend">
        {names.map((name, s) => (
          <span className="legend-item" key={name}>
            <span className="swatch" style={{ background: colors[s] }} />
            {name}
          </span>
        ))}
      </div>
    </div>
  );
}

/** One horizontal part-to-whole bar with a counted legend (payment mix). */
export function MixBar({
  slices,
}: {
  slices: { name: string; value: number; color: string }[];
}) {
  const total = slices.reduce((a, s) => a + s.value, 0);
  const [hover, setHover] = useState<number | null>(null);
  if (total === 0) {
    return <div className="muted small">No transactions in the last 30 days.</div>;
  }
  const h = 30;
  let acc = 0;
  return (
    <div className="chart-wrap">
      <svg viewBox={`0 0 ${W} ${h}`} role="img">
        {slices.map((s, i) => {
          const x = (acc / total) * W;
          acc += s.value;
          const w = (s.value / total) * W;
          if (s.value === 0) return null;
          return (
            <rect
              key={s.name}
              x={x}
              y={4}
              width={Math.max(0, w - 2)} /* 2px surface gap between segments */
              height={h - 8}
              rx={4}
              fill={s.color}
              opacity={hover === null || hover === i ? 1 : 0.45}
              onMouseEnter={() => setHover(i)}
              onMouseLeave={() => setHover(null)}
            />
          );
        })}
      </svg>
      <div className="legend">
        {slices.map((s) => (
          <span className="legend-item" key={s.name}>
            <span className="swatch" style={{ background: s.color }} />
            {s.name} · <strong>{s.value.toLocaleString()}</strong>{" "}
            <span className="muted">({total ? Math.round((s.value / total) * 100) : 0}%)</span>
          </span>
        ))}
      </div>
    </div>
  );
}

/** Tiny inline trend for a stat tile. */
export function Sparkline({
  values,
  color,
  w = 110,
  h = 30,
}: {
  values: number[];
  color: string;
  w?: number;
  h?: number;
}) {
  if (values.length < 2) return null;
  const max = Math.max(...values, 1);
  const pts = values
    .map(
      (v, i) =>
        `${(i / (values.length - 1)) * (w - 4) + 2},${h - 3 - (v / max) * (h - 6)}`,
    )
    .join(" ");
  return (
    <svg width={w} height={h} aria-hidden="true">
      <polyline points={pts} fill="none" stroke={color} strokeWidth={1.5} />
    </svg>
  );
}
