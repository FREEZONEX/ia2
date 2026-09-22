/**
 * The built-in HMI symbol library — ISA-101 high-performance style on the
 * IA2 design tokens. The discipline every symbol follows: calm neutral
 * outlines in the normal state; colour appears only when it carries state
 * (running/open = --highlight, attention = --warn, fault/alarm =
 * --destructive). No gradients, no gloss, no decoration.
 *
 * Contract per symbol (mirrored by the server's /api/hmi-symbols catalog):
 * a `live` map with the resolved numeric value per bind key (null when
 * unresolved), and the node's static `props`.
 */

import { cn } from "@/lib/utils"
import { cssColor } from "@/lib/hmi-binding"
import { sampleSegments } from "@/lib/var-history"

export type SymbolLive = Record<string, number | null>

type Props = {
  symbol: string
  w: number
  h: number
  live: SymbolLive
  props: Record<string, unknown>
  /** Resolved color from the node's `color` bind (map output), when set.
   *  Wins over `props.color`; both fall back to the symbol default. */
  liveColor?: string | null
  /** Recent samples for history-drawing symbols (sparkline). */
  history?: (number | null)[]
}

const on = (v: number | null | undefined) => (v ?? 0) !== 0

function label(props: Record<string, unknown>): string | null {
  const l = props["label"]
  return typeof l === "string" && l.length > 0 ? l : null
}

function num(props: Record<string, unknown>, key: string, dflt: number): number {
  const v = props[key]
  return typeof v === "number" && Number.isFinite(v) ? v : dflt
}

function str(props: Record<string, unknown>, key: string): string | null {
  const v = props[key]
  return typeof v === "string" && v.length > 0 ? v : null
}

/** Effective accent color for a symbol: live map output, then the static
 *  prop, then the given default — always run through the token table. */
function accent(
  liveColor: string | null | undefined,
  props: Record<string, unknown>,
  dflt: string,
): string {
  return cssColor(liveColor ?? str(props, "color") ?? dflt)
}

export function HmiSymbol({ symbol, w, h, live, props, liveColor, history }: Props) {
  switch (symbol) {
    case "tank":
      return <Tank w={w} h={h} live={live} props={props} liveColor={liveColor} />
    case "valve":
      return <Valve w={w} h={h} live={live} props={props} />
    case "pump":
      return <RoundMachine w={w} h={h} live={live} props={props} kind="pump" />
    case "motor":
      return <RoundMachine w={w} h={h} live={live} props={props} kind="motor" />
    case "pipe_h":
      return <Pipe live={live} props={{ ...props, orientation: "h" }} liveColor={liveColor} />
    case "pipe_v":
      return <Pipe live={live} props={{ ...props, orientation: "v" }} liveColor={liveColor} />
    case "pipe":
      return <Pipe live={live} props={props} liveColor={liveColor} />
    case "gauge":
      return <Gauge w={w} h={h} live={live} props={props} />
    case "indicator":
      return <Indicator live={live} props={props} />
    case "setpoint":
      return <SetpointChip live={live} props={props} />
    case "analog":
      return <Analog w={w} h={h} live={live} props={props} />
    case "bar":
      return <Bar h={h} live={live} props={props} liveColor={liveColor} />
    case "led":
      return <Led live={live} props={props} liveColor={liveColor} />
    case "sparkline":
      return <Sparkline w={w} h={h} live={live} props={props} history={history ?? []} />
    case "fan":
      return <Fan w={w} h={h} live={live} props={props} />
    case "conveyor":
      return <Conveyor h={h} live={live} props={props} />
    case "switch":
      return <Switch live={live} props={props} />
    default:
      // Unknown symbol: visible placeholder, never a silent hole —
      // validate warns about it too.
      return (
        <div className="flex h-full w-full items-center justify-center rounded border border-dashed border-warn/60 font-mono text-[10px] text-warn">
          {symbol}?
        </div>
      )
  }
}

/** Vessel with a live fill (0-100) and an alarm ring. The fill level
 *  eases between samples; a `color` bind/prop recolors the liquid. */
function Tank({
  w,
  h,
  live,
  props,
  liveColor,
}: {
  w: number
  h: number
  live: SymbolLive
  props: Record<string, unknown>
  liveColor?: string | null
}) {
  const pct = Math.max(0, Math.min(100, live["value"] ?? 0))
  const alarm = on(live["alarm"])
  const unit = typeof props["unit"] === "string" ? (props["unit"] as string) : "%"
  const liquid = liveColor ?? str(props, "color")
  const bodyH = h - 18
  const fillH = (bodyH - 4) * (pct / 100)
  return (
    <div className="flex h-full w-full flex-col items-center">
      <svg width={w} height={bodyH} viewBox={`0 0 ${w} ${bodyH}`}>
        <rect
          x={1}
          y={1}
          width={w - 2}
          height={bodyH - 2}
          rx={8}
          className={cn(
            "fill-card stroke-muted-foreground/60",
            alarm && "stroke-destructive",
          )}
          strokeWidth={alarm ? 2 : 1.5}
        />
        <rect
          x={3}
          y={bodyH - 3 - fillH}
          width={w - 6}
          height={fillH}
          rx={6}
          className={cn(
            "hmi-ease-geom",
            !liquid && (alarm ? "fill-destructive/20" : "fill-trend/25"),
          )}
          style={liquid ? { fill: cssColor(liquid), opacity: 0.3 } : undefined}
        />
        <text
          x={w / 2}
          y={bodyH / 2 + 4}
          textAnchor="middle"
          className="fill-foreground font-mono text-[13px]"
        >
          {live["value"] == null ? "—" : `${pct.toFixed(0)}${unit}`}
        </text>
      </svg>
      {label(props) && (
        <div className="mt-0.5 font-mono text-[10px] text-muted-foreground">
          {label(props)}
        </div>
      )}
    </div>
  )
}

/** Bowtie block valve — filled when open, destructive ring on fault. */
function Valve({
  w,
  h,
  live,
  props,
}: {
  w: number
  h: number
  live: SymbolLive
  props: Record<string, unknown>
}) {
  const openState = on(live["open"])
  const fault = on(live["fault"])
  const s = Math.min(w, h - (label(props) ? 14 : 0))
  const half = s / 2
  return (
    <div className="flex h-full w-full flex-col items-center justify-center">
      <svg width={s} height={s} viewBox={`0 0 ${s} ${s}`}>
        <path
          d={`M2 4 L${half} ${half} L2 ${s - 4} Z M${s - 2} 4 L${half} ${half} L${s - 2} ${s - 4} Z`}
          className={cn(
            openState
              ? "fill-highlight/80 stroke-highlight"
              : "fill-card stroke-muted-foreground/70",
            fault && "stroke-destructive",
          )}
          strokeWidth={fault ? 2 : 1.5}
          strokeLinejoin="round"
        />
      </svg>
      {label(props) && (
        <div className="font-mono text-[10px] text-muted-foreground">
          {label(props)}
        </div>
      )}
    </div>
  )
}

/** Pump (impeller tick) / motor (M) — filled when running. */
function RoundMachine({
  w,
  h,
  live,
  props,
  kind,
}: {
  w: number
  h: number
  live: SymbolLive
  props: Record<string, unknown>
  kind: "pump" | "motor"
}) {
  const running = on(live["running"])
  const fault = on(live["fault"])
  const s = Math.min(w, h - (label(props) ? 14 : 0))
  const r = s / 2 - 2
  return (
    <div className="flex h-full w-full flex-col items-center justify-center">
      <svg width={s} height={s} viewBox={`0 0 ${s} ${s}`}>
        <circle
          cx={s / 2}
          cy={s / 2}
          r={r}
          className={cn(
            running
              ? "fill-highlight/80 stroke-highlight"
              : "fill-card stroke-muted-foreground/70",
            fault && "stroke-destructive",
          )}
          strokeWidth={fault ? 2 : 1.5}
        />
        {kind === "motor" ? (
          <text
            x={s / 2}
            y={s / 2 + 4}
            textAnchor="middle"
            className={cn(
              "font-mono text-[12px] font-bold",
              running ? "fill-highlight-foreground" : "fill-muted-foreground",
            )}
          >
            M
          </text>
        ) : (
          <path
            d={`M${s / 2} ${s / 2} L${s / 2 + r * 0.8} ${s / 2 - r * 0.45} M${s / 2} ${s / 2} L${s / 2 - r * 0.2} ${s / 2 - r * 0.85}`}
            className={cn(
              "stroke-[1.5]",
              running
                ? "hmi-spin stroke-highlight-foreground"
                : "stroke-muted-foreground",
            )}
            strokeLinecap="round"
          />
        )}
      </svg>
      {label(props) && (
        <div className="font-mono text-[10px] text-muted-foreground">
          {label(props)}
        </div>
      )}
    </div>
  )
}

/** Radial 0-100 gauge — ISA says use sparingly; we oblige with restraint. */
function Gauge({
  w,
  h,
  live,
  props,
}: {
  w: number
  h: number
  live: SymbolLive
  props: Record<string, unknown>
}) {
  const pct = Math.max(0, Math.min(100, live["value"] ?? 0))
  const s = Math.min(w, h - (label(props) ? 14 : 0))
  const r = s / 2 - 6
  const a0 = Math.PI * 0.75
  const a1 = Math.PI * 0.75 + Math.PI * 1.5 * (pct / 100)
  const arc = (a: number) => ({
    x: s / 2 + r * Math.cos(a),
    y: s / 2 + r * Math.sin(a),
  })
  const p0 = arc(a0)
  const p1 = arc(a1)
  const large = a1 - a0 > Math.PI ? 1 : 0
  return (
    <div className="flex h-full w-full flex-col items-center justify-center">
      <svg width={s} height={s} viewBox={`0 0 ${s} ${s}`}>
        <circle
          cx={s / 2}
          cy={s / 2}
          r={r}
          className="fill-none stroke-muted-foreground/25"
          strokeWidth={5}
        />
        {pct > 0 && (
          <path
            d={`M${p0.x} ${p0.y} A${r} ${r} 0 ${large} 1 ${p1.x} ${p1.y}`}
            className="fill-none stroke-trend"
            strokeWidth={5}
            strokeLinecap="round"
          />
        )}
        <text
          x={s / 2}
          y={s / 2 + 5}
          textAnchor="middle"
          className="fill-foreground font-mono text-[14px]"
        >
          {live["value"] == null ? "—" : pct.toFixed(0)}
        </text>
      </svg>
      {label(props) && (
        <div className="font-mono text-[10px] text-muted-foreground">
          {label(props)}
        </div>
      )}
    </div>
  )
}

/** Status dot + label row. Calm gray off; highlight on; destructive alarm. */
function Indicator({
  live,
  props,
}: {
  live: SymbolLive
  props: Record<string, unknown>
}) {
  const isOn = on(live["on"])
  const alarm = on(live["alarm"])
  return (
    <div className="flex h-full w-full items-center gap-2 overflow-hidden">
      <span
        className={cn(
          "size-[10px] shrink-0 rounded-full border",
          alarm
            ? "border-destructive bg-destructive"
            : isOn
              ? "border-highlight bg-highlight"
              : "border-muted-foreground/50 bg-transparent",
        )}
      />
      <span
        className={cn(
          "truncate font-mono text-[12px]",
          alarm ? "text-destructive" : "text-foreground",
        )}
      >
        {label(props) ?? "—"}
      </span>
    </div>
  )
}

/** Moving analog indicator — ISA-101's preferred analog display: a
 *  vertical scale with a shaded normal band, a live pointer, and an
 *  optional setpoint tick. Out-of-band values pull attention because the
 *  pointer leaves the band, not because anything flashes. */
function Analog({
  w,
  h,
  live,
  props,
}: {
  w: number
  h: number
  live: SymbolLive
  props: Record<string, unknown>
}) {
  const min = num(props, "min", 0)
  const max = num(props, "max", 100)
  const span = max - min || 1
  const lo = num(props, "lo", NaN)
  const hi = num(props, "hi", NaN)
  const unit = str(props, "unit") ?? ""
  const lbl = label(props)
  const svgH = h - (lbl ? 16 : 0)
  const top = 8
  const bottom = svgH - 8
  const railX = 18
  const yFor = (v: number) =>
    bottom - ((Math.max(min, Math.min(max, v)) - min) / span) * (bottom - top)
  const v = live["value"]
  const sp = live["sp"]
  const inBand =
    v == null || !Number.isFinite(lo) || !Number.isFinite(hi)
      ? true
      : v >= lo && v <= hi
  return (
    <div className="flex h-full w-full flex-col items-center">
      <svg width={w} height={svgH} viewBox={`0 0 ${w} ${svgH}`}>
        <line
          x1={railX}
          y1={top}
          x2={railX}
          y2={bottom}
          className="stroke-muted-foreground/40"
          strokeWidth={2}
        />
        {Number.isFinite(lo) && Number.isFinite(hi) && (
          <rect
            x={railX - 5}
            y={yFor(hi)}
            width={10}
            height={Math.max(0, yFor(lo) - yFor(hi))}
            className="fill-muted-foreground/20"
            rx={2}
          />
        )}
        {sp != null && (
          <line
            x1={railX - 8}
            y1={yFor(sp)}
            x2={railX + 8}
            y2={yFor(sp)}
            className="stroke-trend"
            strokeWidth={2}
          />
        )}
        {v != null && (
          <g className="hmi-ease-transform" transform={`translate(0 ${yFor(v)})`}>
            <path
              d={`M${railX - 12} 0 L${railX - 4} -5 L${railX - 4} 5 Z`}
              className={cn(inBand ? "fill-foreground" : "fill-destructive")}
            />
            <text
              x={railX + 14}
              y={4}
              className={cn(
                "font-mono text-[12px]",
                inBand ? "fill-foreground" : "fill-destructive",
              )}
            >
              {v.toFixed(Number.isInteger(v) ? 0 : 1)}
              {unit}
            </text>
          </g>
        )}
        {v == null && (
          <text x={railX + 14} y={(top + bottom) / 2} className="fill-muted-foreground font-mono text-[12px]">
            —
          </text>
        )}
      </svg>
      {lbl && (
        <div className="font-mono text-[10px] text-muted-foreground">{lbl}</div>
      )}
    </div>
  )
}

/** Linear fill bar, horizontal or vertical. */
function Bar({
  h,
  live,
  props,
  liveColor,
}: {
  h: number
  live: SymbolLive
  props: Record<string, unknown>
  liveColor?: string | null
}) {
  const min = num(props, "min", 0)
  const max = num(props, "max", 100)
  const span = max - min || 1
  const vertical = str(props, "orientation") === "v"
  const v = live["value"]
  const frac =
    v == null ? 0 : Math.max(0, Math.min(1, (v - min) / span))
  const fill = accent(liveColor, props, "info")
  const unit = str(props, "unit") ?? ""
  const lbl = label(props)
  const text = v == null ? "—" : `${v.toFixed(Number.isInteger(v) ? 0 : 1)}${unit}`
  const barH = vertical ? h - (lbl ? 16 : 0) - 16 : undefined
  return (
    <div
      className={cn(
        "flex h-full w-full gap-1.5",
        vertical ? "flex-col items-center" : "flex-col justify-center",
      )}
    >
      {!vertical && (lbl || true) && (
        <div className="flex items-baseline justify-between">
          {lbl && (
            <span className="font-mono text-[10px] text-muted-foreground">{lbl}</span>
          )}
          <span className="font-mono text-[11px] text-foreground">{text}</span>
        </div>
      )}
      {vertical ? (
        <>
          <span className="font-mono text-[11px] text-foreground">{text}</span>
          <div
            className="w-[14px] overflow-hidden rounded-full bg-muted-foreground/15"
            style={{ height: barH }}
          >
            <div
              className="hmi-ease-size w-full rounded-full"
              style={{
                height: `${frac * 100}%`,
                marginTop: `${(1 - frac) * 100}%`,
                background: fill,
              }}
            />
          </div>
          {lbl && (
            <span className="font-mono text-[10px] text-muted-foreground">{lbl}</span>
          )}
        </>
      ) : (
        <div className="h-[10px] w-full overflow-hidden rounded-full bg-muted-foreground/15">
          <div
            className="hmi-ease-size h-full rounded-full"
            style={{ width: `${frac * 100}%`, background: fill }}
          />
        </div>
      )}
    </div>
  )
}

/** Headline numeric readout — dark plate, large mono digits. Bind
 *  `color` (with a map) to state-color the digits. */
function Led({
  live,
  props,
  liveColor,
}: {
  live: SymbolLive
  props: Record<string, unknown>
  liveColor?: string | null
}) {
  const v = live["value"]
  const unit = str(props, "unit") ?? ""
  const lbl = label(props)
  const color = liveColor ?? str(props, "color")
  return (
    <div className="flex h-full w-full flex-col justify-center overflow-hidden rounded-md border border-border bg-[#14181c] px-3 py-1.5">
      {lbl && (
        <div className="font-mono text-[9px] uppercase tracking-wider text-[#9aa39b]">
          {lbl}
        </div>
      )}
      <div className="flex items-baseline gap-1.5">
        <span
          className="font-mono text-[26px] font-semibold leading-tight"
          style={{ color: color ? cssColor(color) : "#f4f6f3" }}
        >
          {v == null ? "—" : v.toFixed(Number.isInteger(v) ? 0 : 1)}
        </span>
        {unit && <span className="font-mono text-[11px] text-[#9aa39b]">{unit}</span>}
      </div>
    </div>
  )
}

/** Inline mini-trend: the recent history of one bound variable, no axes. */
function Sparkline({
  w,
  h,
  live,
  props,
  history,
}: {
  w: number
  h: number
  live: SymbolLive
  props: Record<string, unknown>
  history: (number | null)[]
}) {
  const lbl = label(props)
  const svgH = h - (lbl ? 14 : 0)
  const segments = sampleSegments(history)
  const readings = segments.flatMap(run => run.map(p => p.value))
  let lo = readings.length ? Math.min(...readings) : 0
  let hi = readings.length ? Math.max(...readings) : 1
  if (hi - lo < 1e-9) {
    lo -= 1
    hi += 1
  }
  const step = w / Math.max(1, history.length - 1)
  const d = segments.map(run => run
    .map(
      ({ index, value }, i) =>
        `${i === 0 ? "M" : "L"}${(index * step).toFixed(1)} ${(
          svgH - 3 - ((value - lo) / (hi - lo)) * (svgH - 6)
        ).toFixed(1)}`,
    )
    .join(" ")).join(" ")
  const v = live["value"]
  return (
    <div className="flex h-full w-full flex-col">
      <svg width={w} height={svgH} viewBox={`0 0 ${w} ${svgH}`}>
        <path d={d} className="fill-none stroke-trend" strokeWidth={1.5} />
      </svg>
      {lbl && (
        <div className="flex items-baseline justify-between font-mono text-[10px] text-muted-foreground">
          <span>{lbl}</span>
          <span className="text-foreground">
            {v == null ? "—" : v.toFixed(Number.isInteger(v) ? 0 : 1)}
          </span>
        </div>
      )}
    </div>
  )
}

/** Process line. Bind `flow`: nonzero animates travel (sign sets the
 *  direction); zero/unbound renders the familiar static line. */
function Pipe({
  live,
  props,
  liveColor,
}: {
  live: SymbolLive
  props: Record<string, unknown>
  liveColor?: string | null
}) {
  const vertical = str(props, "orientation") === "v"
  const flow = live["flow"] ?? 0
  const color = accent(liveColor, props, "muted")
  const moving = flow !== 0
  const base = vertical ? "h-full w-[6px]" : "h-[6px] w-full"
  if (!moving) {
    return (
      <div
        className={cn(base, "rounded-full")}
        style={{ background: color === cssColor("muted") ? "color-mix(in oklab, var(--muted-foreground) 30%, transparent)" : color }}
      />
    )
  }
  const grad = vertical
    ? `repeating-linear-gradient(180deg, ${color} 0 6px, transparent 6px 14px)`
    : `repeating-linear-gradient(90deg, ${color} 0 6px, transparent 6px 14px)`
  return (
    <div className={cn(base, "relative overflow-hidden rounded-full")}>
      <div
        className="absolute inset-0 rounded-full"
        style={{ background: "color-mix(in oklab, var(--muted-foreground) 18%, transparent)" }}
      />
      <div
        className={cn(
          "absolute inset-0",
          vertical
            ? flow < 0
              ? "hmi-flow-v-rev"
              : "hmi-flow-v"
            : flow < 0
              ? "hmi-flow-rev"
              : "hmi-flow",
        )}
        style={{ background: grad, backgroundSize: vertical ? "6px 14px" : "14px 6px" }}
      />
    </div>
  )
}

/** Fan / blower — three blades, spinning while running. */
function Fan({
  w,
  h,
  live,
  props,
}: {
  w: number
  h: number
  live: SymbolLive
  props: Record<string, unknown>
}) {
  const running = on(live["running"])
  const fault = on(live["fault"])
  const s = Math.min(w, h - (label(props) ? 14 : 0))
  const c = s / 2
  const r = s / 2 - 2
  const blade = (angle: number) =>
    `rotate(${angle} ${c} ${c})`
  return (
    <div className="flex h-full w-full flex-col items-center justify-center">
      <svg width={s} height={s} viewBox={`0 0 ${s} ${s}`}>
        <circle
          cx={c}
          cy={c}
          r={r}
          className={cn(
            "fill-card",
            fault
              ? "stroke-destructive"
              : running
                ? "stroke-highlight"
                : "stroke-muted-foreground/70",
          )}
          strokeWidth={fault ? 2 : 1.5}
        />
        <g className={cn(running && "hmi-spin")}>
          {[0, 120, 240].map((a) => (
            <ellipse
              key={a}
              cx={c}
              cy={c - r * 0.45}
              rx={r * 0.22}
              ry={r * 0.42}
              transform={blade(a)}
              className={cn(
                running ? "fill-highlight/80" : "fill-muted-foreground/40",
              )}
            />
          ))}
          <circle cx={c} cy={c} r={r * 0.16} className="fill-muted-foreground" />
        </g>
      </svg>
      {label(props) && (
        <div className="font-mono text-[10px] text-muted-foreground">
          {label(props)}
        </div>
      )}
    </div>
  )
}

/** Belt conveyor, side view — stripes travel while running. */
function Conveyor({
  h,
  live,
  props,
}: {
  h: number
  live: SymbolLive
  props: Record<string, unknown>
}) {
  const running = on(live["running"])
  const fault = on(live["fault"])
  const reverse = props["reverse"] === true
  const lbl = label(props)
  const beltH = Math.min(h - (lbl ? 14 : 0), 28)
  const stripes = `repeating-linear-gradient(90deg, color-mix(in oklab, var(--muted-foreground) 35%, transparent) 0 3px, transparent 3px 12px)`
  return (
    <div className="flex h-full w-full flex-col items-center justify-center">
      <div
        className={cn(
          "relative w-full overflow-hidden rounded-full border",
          fault
            ? "border-destructive"
            : running
              ? "border-highlight"
              : "border-muted-foreground/50",
        )}
        style={{ height: beltH }}
      >
        <div
          className={cn(
            "absolute inset-0",
            running && (reverse ? "hmi-flow-rev" : "hmi-flow"),
          )}
          style={{ background: stripes, backgroundSize: "12px 100%" }}
        />
        <div className="absolute left-[6px] top-1/2 size-[10px] -translate-y-1/2 rounded-full border border-muted-foreground/60" />
        <div className="absolute right-[6px] top-1/2 size-[10px] -translate-y-1/2 rounded-full border border-muted-foreground/60" />
      </div>
      {lbl && (
        <div className="mt-0.5 font-mono text-[10px] text-muted-foreground">{lbl}</div>
      )}
    </div>
  )
}

/** Rocker switch — a STATE toggle, visually distinct from a momentary
 *  button (RFC #33-C1): pill track, sliding thumb, ON/OFF captions.
 *  Pair with an `action.tap` toggle; bind `on` to the state it shows. */
function Switch({
  live,
  props,
}: {
  live: SymbolLive
  props: Record<string, unknown>
}) {
  const isOn = on(live["on"])
  const onText = str(props, "on_text") ?? "ON"
  const offText = str(props, "off_text") ?? "OFF"
  const lbl = label(props)
  return (
    <div className="flex h-full w-full items-center gap-2 overflow-hidden">
      <div
        className={cn(
          "relative h-[22px] w-[44px] shrink-0 rounded-full border transition-colors",
          isOn ? "border-highlight bg-highlight/80" : "border-border bg-muted",
        )}
      >
        <div
          className="absolute top-[2px] size-[16px] rounded-full bg-background shadow"
          style={{ left: isOn ? 24 : 2, transition: "left 0.2s ease" }}
        />
      </div>
      <span
        className={cn(
          "font-mono text-[11px]",
          isOn ? "text-foreground" : "text-muted-foreground",
        )}
      >
        {isOn ? onText : offText}
      </span>
      {lbl && (
        <span className="ml-auto truncate font-mono text-[10px] text-muted-foreground">
          {lbl}
        </span>
      )}
    </div>
  )
}

/** Read-only setpoint chip. */
function SetpointChip({
  live,
  props,
}: {
  live: SymbolLive
  props: Record<string, unknown>
}) {
  const unit = typeof props["unit"] === "string" ? (props["unit"] as string) : ""
  const v = live["value"]
  return (
    <div className="flex h-full w-full items-center justify-between gap-2 rounded border border-border bg-card px-2">
      <span className="truncate font-mono text-[11px] text-muted-foreground">
        {label(props) ?? "SP"}
      </span>
      <span className="font-mono text-[12px] text-foreground">
        {v == null ? "—" : v}
        {unit && <span className="ml-0.5 text-muted-foreground">{unit}</span>}
      </span>
    </div>
  )
}
