import { useEffect, useRef, useState } from "react"

/** One plotted sample. `t` (seconds, snapshot time base) places the
 *  point on the time axis; without it the series falls back to uniform
 *  index spacing. `lo`/`hi` draw a min/max band under the line (history
 *  buckets carry them; live single samples leave them undefined). */
export type TrendPoint = { t?: number; v: number; lo?: number; hi?: number }

export type TrendSeries = {
  name: string
  points: TrendPoint[]
  color: string
  binary: boolean
}

type Props = {
  series: TrendSeries[]
  height?: number
  /** Fill an authored HMI node without allowing legend/axes to escape. */
  fit?: boolean
  /** X-axis span in seconds; the newest sample anywhere pins the right
   *  edge, `windowS` earlier pins the left. Only affects timed series. */
  windowS?: number
}

// viewBox width; SVG stretches to the plot rectangle (preserveAspectRatio
// = none). Gutters are real px around it so axis text stays crisp.
const W = 1000
const PAD_L = 52 // Y-axis label gutter
const PAD_TOP = 8
const PAD_BOTTOM = 22 // X-axis label strip
const Y_INSET = 2

/**
 * Hand-rolled multi-line trend for pinned / bound variables. A real
 * chart, not a sparkline: labelled Y axis, relative-time X ticks, a
 * min/max band from history buckets under the last-value line, and a
 * hover cursor that reads time + each series' value into the legend.
 *
 * Two Y-scale modes (toggle top-right): "per-series" normalises each
 * line to its own range so wildly different magnitudes coexist (the Y
 * axis then labels the single series, or defers to the legend when many
 * share the plot); "shared" puts everything on one labelled scale for
 * direct comparison.
 */
export function TrendChart({ series, height = 130, fit = false, windowS }: Props) {
  const [shared, setShared] = useState(false)
  const [hoverFrac, setHoverFrac] = useState<number | null>(null)
  const plotRef = useRef<HTMLDivElement | null>(null)
  const containerRef = useRef<HTMLDivElement | null>(null)
  const legendRef = useRef<HTMLDivElement | null>(null)
  const [fittedHeight, setFittedHeight] = useState(height)
  useEffect(() => {
    if (!fit || !containerRef.current || !legendRef.current) return
    const container = containerRef.current
    const legend = legendRef.current
    const measure = () => setFittedHeight(Math.max(1, container.clientHeight - legend.offsetHeight))
    measure()
    const observer = new ResizeObserver(measure)
    observer.observe(container)
    observer.observe(legend)
    return () => observer.disconnect()
  }, [fit, series.length])
  const chartHeight = fit ? fittedHeight : height

  if (series.length === 0) {
    return (
      <div
        className="flex min-h-0 items-center justify-center overflow-hidden text-xs text-muted-foreground"
        style={{ height: fit ? "100%" : height }}
      >
        No variables to trend.
      </div>
    )
  }

  const plotH = Math.max(10, chartHeight - PAD_TOP - PAD_BOTTOM)

  // ---- X domain (shared time axis across all series) ----------------
  let anyTimed = false
  let tEnd = -Infinity
  let tMinData = Infinity
  for (const s of series) {
    for (const p of s.points) {
      if (p.t !== undefined) {
        anyTimed = true
        if (p.t > tEnd) tEnd = p.t
        if (p.t < tMinData) tMinData = p.t
      }
    }
  }
  const timed = anyTimed && Number.isFinite(tEnd)
  const span =
    windowS != null && windowS > 0
      ? windowS
      : timed
        ? Math.max(1, tEnd - tMinData)
        : null
  const tStart = timed ? (span != null ? tEnd - span : tMinData) : null
  const tRange = timed && tStart != null ? Math.max(1e-9, tEnd - tStart) : 1

  const xFracOf = (s: TrendSeries, i: number): number => {
    const p = s.points[i]
    if (timed && tStart != null && p.t !== undefined) {
      return clamp((p.t - tStart) / tRange, 0, 1)
    }
    return s.points.length > 1 ? i / (s.points.length - 1) : 1
  }

  // ---- Y scale ------------------------------------------------------
  const ranges = series.map(seriesRange)
  const sharedRange = ranges.reduce<[number, number]>(
    (acc, r) => [Math.min(acc[0], r[0]), Math.max(acc[1], r[1])],
    [Infinity, -Infinity],
  )
  if (!Number.isFinite(sharedRange[0])) {
    sharedRange[0] = 0
    sharedRange[1] = 1
  }
  if (sharedRange[0] === sharedRange[1]) sharedRange[1] = sharedRange[0] + 1
  const scaleOf = (idx: number): [number, number] =>
    shared ? sharedRange : ranges[idx]
  const toY = ([mn, mx]: [number, number], v: number): number =>
    Y_INSET + (1 - (v - mn) / (mx - mn)) * (plotH - 2 * Y_INSET)

  // Which scale (if any) gets numeric Y labels: shared always does; in
  // per-series mode a lone series labels its own axis, but a crowd of
  // them can't share one — the legend carries their ranges instead.
  const labeledRange: [number, number] | null = shared
    ? sharedRange
    : series.length === 1
      ? ranges[0]
      : null

  const nearestIndex = (s: TrendSeries, frac: number): number => {
    const n = s.points.length
    if (n <= 1) return n - 1
    let best = 0
    let bestD = Infinity
    for (let i = 0; i < n; i++) {
      const d = Math.abs(xFracOf(s, i) - frac)
      if (d < bestD) {
        bestD = d
        best = i
      }
    }
    return best
  }

  const hoverTimeLabel =
    hoverFrac != null && timed && tStart != null
      ? relTime(tStart + hoverFrac * tRange - tEnd)
      : null

  // ---- render -------------------------------------------------------
  const yTicks = labeledRange ? axisTicks(labeledRange) : []

  return (
    <div ref={containerRef} className={fit ? "h-full min-h-0 min-w-0 select-none overflow-hidden" : "min-w-0 select-none"}>
      {/* Legend + hover readout + scale toggle */}
      <div ref={legendRef} className="flex flex-wrap items-center gap-x-4 gap-y-1 px-1 pb-2 text-xs">
        {series.map((s, i) => {
          const idx = hoverFrac != null ? nearestIndex(s, hoverFrac) : s.points.length - 1
          const val = idx >= 0 ? s.points[idx]?.v : undefined
          const [mn, mx] = ranges[i]
          return (
            <div key={s.name} className="flex items-center gap-1.5">
              <span
                className="inline-block size-2 rounded-full"
                style={{ background: s.color }}
              />
              <span className="font-mono text-foreground">{s.name}</span>
              <span className="font-mono tabular-nums text-muted-foreground">
                {val === undefined ? "—" : fmtVal(val)}
              </span>
              {!shared && series.length > 1 && (
                <span className="font-mono tabular-nums text-muted-foreground/50">
                  ({fmtVal(mn)}–{fmtVal(mx)})
                </span>
              )}
            </div>
          )
        })}
        <div className="ml-auto flex items-center gap-2">
          {hoverTimeLabel && (
            <span className="font-mono tabular-nums text-muted-foreground/70">
              @ {hoverTimeLabel}
            </span>
          )}
          <button
            type="button"
            onClick={() => setShared((v) => !v)}
            title={
              shared
                ? "Shared Y scale — click for per-series auto-scaling"
                : "Per-series Y scale — click to share one scale"
            }
            className="rounded border border-border bg-card px-1 font-mono text-xs text-muted-foreground hover:text-foreground"
          >
            {shared ? "shared" : "per-series"}
          </button>
        </div>
      </div>

      {/* Chart area: Y gutter + plot; X labels live in the bottom strip. */}
      <div className="flex" style={{ height: chartHeight }}>
        <div className="relative shrink-0" style={{ width: PAD_L }}>
          {yTicks.map((tick) => (
            <span
              key={tick}
              className="absolute right-1 -translate-y-1/2 font-mono text-xs tabular-nums text-muted-foreground/70"
              style={{ top: PAD_TOP + toY(labeledRange!, tick) }}
            >
              {fmtVal(tick)}
            </span>
          ))}
          {!labeledRange && (
            <span
              className="absolute right-1 font-mono text-xs text-muted-foreground/40"
              style={{ top: PAD_TOP }}
            >
              auto
            </span>
          )}
        </div>

        <div
          ref={plotRef}
          className="relative min-w-0 flex-1"
          onMouseMove={(e) => {
            const el = plotRef.current
            if (!el) return
            const r = el.getBoundingClientRect()
            if (r.width === 0) return
            setHoverFrac(clamp((e.clientX - r.left) / r.width, 0, 1))
          }}
          onMouseLeave={() => setHoverFrac(null)}
        >
          <svg
            viewBox={`0 0 ${W} ${plotH}`}
            preserveAspectRatio="none"
            className="absolute inset-x-0 block w-full"
            style={{ top: PAD_TOP, height: plotH }}
          >
            {/* Baseline + Y gridlines */}
            {(yTicks.length > 0 ? yTicks : [0.5]).map((tick, gi) => {
              const y = labeledRange
                ? toY(labeledRange, tick)
                : plotH * (1 - (tick as number))
              return (
                <line
                  key={gi}
                  x1={0}
                  y1={y}
                  x2={W}
                  y2={y}
                  stroke="currentColor"
                  strokeOpacity={0.08}
                  strokeDasharray="2 4"
                />
              )
            })}

            {/* Min/max bands first, so lines sit on top of them. */}
            {series.map((s, i) => {
              if (!s.binary && hasBand(s)) {
                return (
                  <path
                    key={`band-${s.name}`}
                    d={bandPath(s, i, scaleOf(i), toY, xFracOf)}
                    fill={s.color}
                    fillOpacity={0.12}
                    stroke="none"
                  />
                )
              }
              return null
            })}

            {/* Last-value lines. */}
            {series.map((s, i) => renderLine(s, i, scaleOf(i), toY, xFracOf))}

            {/* Hover cursor. */}
            {hoverFrac != null && (
              <line
                x1={hoverFrac * W}
                y1={0}
                x2={hoverFrac * W}
                y2={plotH}
                stroke="currentColor"
                strokeOpacity={0.35}
                vectorEffect="non-scaling-stroke"
              />
            )}
          </svg>

          {/* X-axis relative-time ticks. */}
          {timed && (
            <div
              className="absolute inset-x-0 flex justify-between font-mono text-xs tabular-nums text-muted-foreground/60"
              style={{ bottom: 0, height: PAD_BOTTOM }}
            >
              {[0, 0.5, 1].map((f) => (
                <span key={f}>
                  {f === 1 ? "now" : relTime(tStart! + f * tRange - tEnd)}
                </span>
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  )
}

// ---- pure drawing helpers -----------------------------------------

function seriesRange(s: TrendSeries): [number, number] {
  if (s.binary) return [0, 1]
  let mn = Infinity
  let mx = -Infinity
  for (const p of s.points) {
    const lo = p.lo ?? p.v
    const hi = p.hi ?? p.v
    if (lo < mn) mn = lo
    if (hi > mx) mx = hi
  }
  if (!Number.isFinite(mn) || !Number.isFinite(mx)) return [0, 1]
  if (mn === mx) return [mn, mn + 1]
  return [mn, mx]
}

function hasBand(s: TrendSeries): boolean {
  return s.points.some(
    (p) => p.lo !== undefined && p.hi !== undefined && p.hi > p.lo,
  )
}

function bandPath(
  s: TrendSeries,
  _i: number,
  scale: [number, number],
  toY: (scale: [number, number], v: number) => number,
  xFracOf: (s: TrendSeries, i: number) => number,
): string {
  const upper: string[] = []
  const lower: string[] = []
  for (let i = 0; i < s.points.length; i++) {
    const p = s.points[i]
    const x = (xFracOf(s, i) * W).toFixed(1)
    upper.push(`${x},${toY(scale, p.hi ?? p.v).toFixed(1)}`)
    lower.push(`${x},${toY(scale, p.lo ?? p.v).toFixed(1)}`)
  }
  lower.reverse()
  return `M${upper.join(" L")} L${lower.join(" L")} Z`
}

function renderLine(
  s: TrendSeries,
  i: number,
  scale: [number, number],
  toY: (scale: [number, number], v: number) => number,
  xFracOf: (s: TrendSeries, i: number) => number,
) {
  if (s.points.length < 2) return null
  const pts: string[] = []
  for (let j = 0; j < s.points.length; j++) {
    const x = xFracOf(s, j) * W
    const y = toY(scale, s.points[j].v)
    if (s.binary && j > 0) {
      // Stair-step: hold the previous level to the new x before stepping.
      pts.push(`${x.toFixed(1)},${toY(scale, s.points[j - 1].v).toFixed(1)}`)
    }
    pts.push(`${x.toFixed(1)},${y.toFixed(1)}`)
  }
  return (
    <polyline
      key={`line-${s.name}-${i}`}
      points={pts.join(" ")}
      fill="none"
      stroke={s.color}
      strokeWidth={1}
      vectorEffect="non-scaling-stroke"
      strokeLinejoin={s.binary ? "miter" : "round"}
      strokeLinecap="round"
      opacity={0.95}
    />
  )
}

function clamp(v: number, lo: number, hi: number): number {
  return v < lo ? lo : v > hi ? hi : v
}

/** Three axis ticks (min, mid, max) for a labelled scale. */
function axisTicks([mn, mx]: [number, number]): number[] {
  return [mx, (mn + mx) / 2, mn]
}

/** Compact numeric label — integers verbatim, small floats to 2–3 dp,
 *  extremes to exponential, so a 40-px gutter never overflows. */
function fmtVal(n: number): string {
  if (!Number.isFinite(n)) return "—"
  const a = Math.abs(n)
  if (a !== 0 && (a >= 1e5 || a < 1e-3)) return n.toExponential(1)
  if (Number.isInteger(n)) return String(n)
  return n.toFixed(a < 1 ? 3 : a < 100 ? 2 : 1)
}

/** Relative-time X label from a (negative) seconds offset behind now. */
function relTime(dtS: number): string {
  const r = Math.round(dtS)
  if (r === 0) return "0s"
  if (Math.abs(r) >= 600) return `${Math.round(r / 60)}m`
  return `${r}s`
}
