import type { HistoryPoint } from "@/types/generated/HistoryPoint"
import type { VarValue } from "@/types/generated/VarValue"

export const MAX_HISTORY = 256

const HEX_RE = /^16#[0-9A-F]+$/i
const TIME_RE = /^L?TIME?#-?[\d.]+/i

/** Coerce a ironplc-formatted value string into a number for charting.
 *  TRUE/FALSE → 1/0, hex/time literals strip prefixes, everything else
 *  goes through parseFloat. NaN/Infinity fall back to 0. */
export function parseVarValue(v: VarValue): number {
  const s = v.value
  if (s === "TRUE") return 1
  if (s === "FALSE") return 0
  if (HEX_RE.test(s)) {
    const n = parseInt(s.slice(3), 16)
    return Number.isFinite(n) ? n : 0
  }
  if (TIME_RE.test(s)) {
    const m = s.match(/-?[\d.]+/)
    return m ? parseFloat(m[0]) : 0
  }
  const n = parseFloat(s)
  return Number.isFinite(n) ? n : 0
}

export function isBoolType(typeName: string): boolean {
  return typeName.toUpperCase() === "BOOL"
}

/** Category a variable's type falls into. Drives how Monitor renders
 *  it — numeric gets a sparkline, bool a chip, bits a hex pill, time a
 *  pretty duration, text plain text, and FB instances are skipped from
 *  the trace because they're internal byte-offset bookkeeping.
 *
 *  Categorisation is by the type-name string the bridge reports
 *  (`VariableRenderer` already preserves these via ironplc's
 *  debug section), so this is the one place to extend when new IEC
 *  types come online. */
export type VarCategory =
  | "bool"
  | "numeric"
  | "bits"
  | "time"
  | "text"
  | "fb"
  | "other"

const NUMERIC_TYPES = new Set([
  "REAL",
  "LREAL",
  "SINT",
  "INT",
  "DINT",
  "LINT",
  "USINT",
  "UINT",
  "UDINT",
  "ULINT",
])
const BIT_TYPES = new Set(["BYTE", "WORD", "DWORD", "LWORD"])
const TIME_TYPES = new Set([
  "TIME",
  "LTIME",
  "DATE",
  "LDATE",
  "TIME_OF_DAY",
  "TOD",
  "DATE_AND_TIME",
  "DT",
])
const TEXT_TYPES = new Set(["STRING", "WSTRING"])

export function classifyType(typeName: string): VarCategory {
  const t = typeName.toUpperCase()
  if (t === "BOOL") return "bool"
  if (NUMERIC_TYPES.has(t)) return "numeric"
  if (BIT_TYPES.has(t)) return "bits"
  if (TIME_TYPES.has(t)) return "time"
  if (TEXT_TYPES.has(t)) return "text"
  // User-defined FB instance types come through with the FB name as the
  // type (e.g. "PID", "ARRHENIUS"). They're scratch storage and have no
  // meaningful single-value display.
  if (/^[A-Z][A-Z0-9_]*$/.test(t)) return "fb"
  return "other"
}

/** Pretty-format a `TIME` / `LTIME` value the bridge serialised as
 *  `T#NNNms` into something a process operator would read. Sub-second:
 *  "750 ms". Seconds: "12.3 s". Minutes: "1m 23s". Hours similar. */
export function prettyTime(raw: string): string {
  const m = raw.match(/-?\d+(?:\.\d+)?/)
  if (!m) return raw
  const ms = parseFloat(m[0])
  if (!Number.isFinite(ms)) return raw
  const abs = Math.abs(ms)
  const sign = ms < 0 ? "-" : ""
  if (abs < 1000) return `${sign}${abs.toFixed(0)} ms`
  if (abs < 60_000) return `${sign}${(abs / 1000).toFixed(abs < 10_000 ? 2 : 1)} s`
  const mins = Math.floor(abs / 60_000)
  const secs = Math.round((abs % 60_000) / 1000)
  if (abs < 3_600_000) return `${sign}${mins}m ${secs}s`
  const hours = Math.floor(abs / 3_600_000)
  const restMin = Math.floor((abs % 3_600_000) / 60_000)
  return `${sign}${hours}h ${restMin}m`
}

/** Strip the `16#` prefix some bridge-formatted hex values carry so
 *  the row can render the digits with its own styling. */
export function stripHexPrefix(raw: string): string {
  return raw.startsWith("16#") ? raw.slice(3) : raw
}

/** Push `next` onto `buf` in place, trimming from the head once we hit
 *  `MAX_HISTORY`. Returns the same array for chaining / spread. */
export function pushHistory(buf: (number | null)[], next: number | null): (number | null)[] {
  buf.push(next)
  if (buf.length > MAX_HISTORY) {
    // Drop ~10% at a time so we're not shift()ing every tick once full.
    buf.splice(0, buf.length - MAX_HISTORY)
  }
  return buf
}

/** Hard cap on a timed buffer regardless of its window — `window_s` is
 *  user-editable screen JSON, so a wild value (86400 s at 10 Hz would
 *  be ~864k points) must not grow memory unbounded. 4096 points cover
 *  the generator's default 300 s window up to ~13 Hz snapshot rate;
 *  past the cap the oldest samples fall out and the chart shows what
 *  fits. */
export const MAX_TIMED_HISTORY = 4096

/** One trend sample: seconds (snapshot time base) + charted value.
 *  Timestamped because retention is by age (`window_s`), not by count —
 *  a count cap makes the visible window silently vary with snapshot
 *  rate. `lo`/`hi` carry a history bucket's min/max envelope; live
 *  single samples leave them undefined (the band collapses to `v`). */
export type TimedSample = { t: number; v: number | null; lo?: number; hi?: number }

/** Push one timestamped sample in place, trimming samples older than
 *  `windowS` seconds behind the newest one, then enforcing the hard
 *  count cap. Age is relative to the newest sample, so a hidden-tab
 *  gap trims exactly what the window no longer covers. */
export function pushTimedHistory(
  buf: TimedSample[],
  t: number,
  v: number | null,
  windowS: number,
): TimedSample[] {
  buf.push({ t, v })
  const cutoff = t - Math.max(1, windowS)
  let drop = 0
  while (drop < buf.length - 1 && buf[drop].t < cutoff) drop++
  const over = buf.length - MAX_TIMED_HISTORY
  if (over > drop) drop = over
  if (drop > 0) buf.splice(0, drop)
  return buf
}

/** The suffix of `buf` within `windowS` seconds of its newest sample —
 *  a trend node's per-render view when it shares the buffer with a
 *  wider-windowed node (retention keeps the max window across nodes
 *  referencing the variable). */
export function windowSlice(
  buf: TimedSample[],
  windowS: number,
): TimedSample[] {
  if (buf.length === 0) return buf
  const cutoff = buf[buf.length - 1].t - Math.max(1, windowS)
  let start = 0
  while (start < buf.length - 1 && buf[start].t < cutoff) start++
  return start === 0 ? buf : buf.slice(start)
}

/** A small fixed palette for pinned series; cycles if more than 8. */
export const SERIES_COLORS = [
  "#0ea5e9", // sky
  "#a855f7", // violet
  "#10b981", // emerald
  "#f97316", // orange
  "#ec4899", // pink
  "#eab308", // yellow
  "#06b6d4", // cyan
  "#ef4444", // red
] as const

export function colorFor(index: number): string {
  return SERIES_COLORS[index % SERIES_COLORS.length]
}

// ============================================================
//   History backfill — seeding a live ring buffer from the
//   runtime's stored history so a page reload doesn't wipe trends.
// ============================================================

/** Convert one downsampled history series into ring-buffer samples.
 *  History timestamps share the snapshot time base the live feed uses
 *  (scan-relative micros), so `t_us / 1e6` lands on the same seconds
 *  axis the live samples are pushed on — that shared base is what makes
 *  the merge dedupe cleanly. `min`/`max` ride along as the band. */
export function historyToSamples(points: HistoryPoint[]): TimedSample[] {
  return points.map((p) => ({
    t: Number(p.t_us) / 1e6,
    v: p.stale ? null : p.v,
    lo: p.stale ? undefined : p.min,
    hi: p.stale ? undefined : p.max,
  }))
}

/** Contiguous finite readings, retaining original indices so a gap
 * occupies its real time/position instead of joining across stale data. */
export function sampleSegments(values: (number | null)[]): { index: number; value: number }[][] {
  const segments: { index: number; value: number }[][] = []
  let current: { index: number; value: number }[] = []
  values.forEach((value, index) => {
    if (value === null || !Number.isFinite(value)) {
      current = []
    } else {
      if (current.length === 0) segments.push(current)
      current.push({ index, value })
    }
  })
  return segments
}

/** Merge stored history (older, ~1 Hz) with the live ring buffer
 *  (newer, higher rate). History fills the span before the live
 *  buffer's earliest sample; any history point at or after that
 *  boundary is dropped so the overlap is never double-plotted (the
 *  live feed is authoritative once it picks up). Inputs are assumed
 *  ascending in `t`; the result stays ascending. */
export function mergeHistory(
  history: TimedSample[],
  live: TimedSample[],
): TimedSample[] {
  if (live.length === 0) return history.slice()
  if (history.length === 0) return live.slice()
  const boundary = live[0].t
  const older = history.filter((p) => p.t < boundary)
  return older.concat(live)
}

/** Seed a live buffer with fetched history, keeping only the newest
 *  `windowS` seconds (the window the chart shows) — the endpoint may
 *  return far more than a trend node displays. */
export function seedTimedBuffer(
  live: TimedSample[],
  history: TimedSample[],
  windowS: number,
): TimedSample[] {
  return windowSlice(mergeHistory(history, live), windowS)
}
