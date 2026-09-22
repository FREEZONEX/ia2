import { ChevronDown, ChevronUp, Lock, Pause, Pin, Play, Search, StepForward, Unlock } from "@/components/ui/icons"
import { useCallback, useEffect, useMemo, useRef, useState } from "react"

import { Button } from "@/components/ui/button"
import { EmptyState } from "@/components/ui/empty-state"
import { PaneHeader } from "@/components/ui/pane-header"
import { canWriteMonitorType, currentForceValue, monitorWriteValue } from "./monitor-values"

import { Sparkline } from "@/components/charts/Sparkline"
import { TrendChart, type TrendSeries } from "@/components/charts/TrendChart"
import {
  alarmStanding,
  fmtAlarmClock,
  fmtAlarmValue,
  severityTone,
  standingCount,
  type AlarmTone,
} from "@/lib/alarms"
import { cn } from "@/lib/utils"
import {
  ackRuntimeAlarm,
  fetchRuntimeAlarms,
  fetchRuntimeHistory,
  fetchRuntimeStatus,
  forceVariable,
  pauseRuntime,
  resumeRuntime,
  stepRuntime,
  unforceVariable,
  writeVariable,
} from "@/lib/api"
import {
  classifyType,
  colorFor,
  historyToSamples,
  isBoolType,
  parseVarValue,
  prettyTime,
  pushHistory,
  pushTimedHistory,
  seedTimedBuffer,
  stripHexPrefix,
  windowSlice,
  type TimedSample,
  type VarCategory,
} from "@/lib/var-history"
import { useRuntime, type RunningInfo } from "@/state/runtime"
import { useLastSnapshot } from "@/state/live-feed"
import type { AlarmState } from "@/types/generated/AlarmState"
import type { VarValue } from "@/types/generated/VarValue"

/** Window the pinned trend shows (seconds). History backfill seeds this
 *  much on pin so a reload no longer starts the trace from empty. */
const MONITOR_TREND_WINDOW_S = 300

export function MonitorPane({ collapsed, onToggleCollapse }: { collapsed: boolean; onToggleCollapse: () => void }) {
  const { isRunning, currentPou, running, attached } = useRuntime()
  const lastSnapshot = useLastSnapshot()
  // Variable writes go through the local bridge. When attached to a
  // remote edge runtime we don't (yet) proxy writes — disable the
  // controls in that case to avoid silently losing the user's input.
  const canWrite = isRunning && !attached

  const [mode, setMode] = useState<"running" | "paused" | "step" | null>(null)
  const [forces, setForces] = useState<Set<string>>(new Set())
  const [pendingCommand, setPendingCommand] = useState<string | null>(null)
  const [commandError, setCommandError] = useState<string | null>(null)
  const [statusError, setStatusError] = useState<string | null>(null)
  const [runtimeFault, setRuntimeFault] = useState<string | null>(null)
  const [alarmCount, setAlarmCount] = useState(0)
  const [alarmStatusError, setAlarmStatusError] = useState<string | null>(null)
  const [query, setQuery] = useState("")
  const commandRef = useRef(false)
  const revisionRef = useRef(0)

  const refreshStatus = useCallback(async () => {
    const revision = revisionRef.current
    try {
      const data = await fetchRuntimeStatus()
      if (revision !== revisionRef.current) return
      const nextMode = data.mode?.kind
      setMode(nextMode === "running" || nextMode === "paused" || nextMode === "step" ? nextMode : null)
      setForces(new Set(data.forces.map(force => force.name)))
      const failedDevices = data.device_health.filter(device => !device.healthy).map(device => device.name)
      setRuntimeFault(data.watchdog_tripped
        ? "Watchdog tripped — outputs are locked"
        : data.last_error || (failedDevices.length ? `Device connection lost: ${failedDevices.join(", ")}` : null))
      setStatusError(null)
    } catch (error) {
      if (revision === revisionRef.current) setStatusError(`Runtime status unavailable: ${String(error)}`)
      throw error
    }
  }, [])

  useEffect(() => {
    if (!isRunning || attached) {
      setMode(null)
      setForces(new Set())
      return
    }
    const tick = () => {
      if (!commandRef.current) void refreshStatus().catch(() => {})
    }
    tick()
    const timer = setInterval(tick, 1000)
    return () => { clearInterval(timer); revisionRef.current++ }
  }, [isRunning, attached, refreshStatus])

  // A click is pending until the request succeeds and status is read back.
  // A failed command never changes the displayed scan mode or force state.
  // `action` may resolve to a notice: the runtime applied the write but the
  // device carrying that variable has a dead link, so it is not reaching the
  // field. Neither a failure nor a clean success — it rides the same channel
  // as "accepted; status unconfirmed" rather than being dropped.
  const command = useCallback(async (label: string, action: () => Promise<string | null | void>) => {
    if (commandRef.current) return false
    commandRef.current = true
    revisionRef.current++
    setPendingCommand(label)
    setCommandError(null)
    let accepted = false
    try {
      const notice = await action()
      accepted = true
      await refreshStatus()
      if (notice) setCommandError(`${label} accepted; ${notice}`)
      return true
    } catch (error) {
      setCommandError(accepted
        ? `${label} accepted; status unconfirmed: ${String(error)}`
        : `${label} failed: ${String(error)}`)
      return false
    } finally {
      commandRef.current = false
      setPendingCommand(null)
    }
  }, [refreshStatus])

  // History buffers (mutated in place; re-rendered via a tick counter).
  // `historyRef` is the untimed count-capped buffer the per-row
  // sparklines read; `timedRef` holds a timestamped buffer per PINNED
  // variable (seeded from stored history, fed by the live feed) that the
  // pinned TrendChart reads — only pinned vars carry the heavier buffer.
  const historyRef = useRef<Map<string, (number | null)[]>>(new Map())
  const timedRef = useRef<Map<string, TimedSample[]>>(new Map())
  const typeRef = useRef<Map<string, string>>(new Map())
  const [, setTick] = useState(0)
  const [pinned, setPinned] = useState<Set<string>>(new Set())

  // Drop history + pins when the user switches POU — old vars aren't
  // relevant to the new one.
  useEffect(() => {
    historyRef.current.clear()
    timedRef.current.clear()
    typeRef.current.clear()
    setPinned(new Set())
    setTick((t) => t + 1)
  }, [currentPou?.path])

  // Ingest every snapshot into the per-variable history. Timed buffers
  // ride the snapshot's own time base (scan-relative micros → seconds)
  // so seeded history lands on the same axis and the merge dedupes.
  useEffect(() => {
    if (!lastSnapshot) return
    const tsec = Number(lastSnapshot.timestamp_us) / 1e6
    const timeOk = lastSnapshot.timestamp_us > 0n
    for (const v of lastSnapshot.vars) {
      typeRef.current.set(v.name, v.type_name)
      let arr = historyRef.current.get(v.name)
      if (!arr) {
        arr = []
        historyRef.current.set(v.name, arr)
      }
      const n = v.input?.stale ? null : parseVarValue(v)
      pushHistory(arr, n)
      // timedRef only holds pinned vars (created by the seed effect).
      if (timeOk) {
        const tbuf = timedRef.current.get(v.name)
        if (tbuf) pushTimedHistory(tbuf, tsec, n, MONITOR_TREND_WINDOW_S)
      }
    }
    setTick((t) => t + 1)
  }, [lastSnapshot])

  // Backfill: seed a timed buffer from stored history the moment a
  // variable is pinned, and drop buffers for vars no longer pinned.
  const seedTrend = useCallback(async (name: string) => {
    try {
      const resp = await fetchRuntimeHistory([name], { stepMs: 1000 })
      const s = resp.series.find((x) => x.name === name)
      if (!s) return
      const existing = timedRef.current.get(name) ?? []
      timedRef.current.set(
        name,
        seedTimedBuffer(
          existing,
          historyToSamples(s.points),
          MONITOR_TREND_WINDOW_S,
        ),
      )
      setTick((t) => t + 1)
    } catch {
      /* history is best-effort; the live feed still fills forward */
    }
  }, [])

  useEffect(() => {
    for (const name of pinned) {
      if (!timedRef.current.has(name)) {
        // Claim the slot synchronously so live samples start landing and
        // a StrictMode double-invoke doesn't double-fetch.
        timedRef.current.set(name, [])
        void seedTrend(name)
      }
    }
    for (const name of [...timedRef.current.keys()]) {
      if (!pinned.has(name)) timedRef.current.delete(name)
    }
  }, [pinned, seedTrend])

  const togglePin = (name: string) => {
    setPinned((prev) => {
      const next = new Set(prev)
      if (next.has(name)) next.delete(name)
      else next.add(name)
      return next
    })
  }

  // Build series for the pinned trend chart from the timed buffers.
  const pinnedList = useMemo(() => Array.from(pinned), [pinned])
  const pinnedSeries: TrendSeries[] = pinnedList.map((name, idx) => {
    const buf = windowSlice(
      timedRef.current.get(name) ?? [],
      MONITOR_TREND_WINDOW_S,
    )
    return {
      name,
      points: buf.map((s) => ({ t: s.t, v: s.v, lo: s.lo, hi: s.hi })),
      color: colorFor(idx),
      binary: isBoolType(typeRef.current.get(name) ?? ""),
    }
  })
  const colorByName: Record<string, string> = Object.fromEntries(
    pinnedList.map((name, idx) => [name, colorFor(idx)]),
  )

  const vars = lastSnapshot?.vars ?? []
  const stale = !!lastSnapshot && !isRunning

  const visibleVars = vars.filter(variable => `${variable.name} ${variable.type_name}`.toLowerCase().includes(query.toLowerCase()))
  const onToggleForce = (variable: VarValue) => void command(
    `${forces.has(variable.name) ? "Unforce" : "Force"} ${variable.name}`,
    () => forces.has(variable.name)
      ? unforceVariable(variable.name)
      : forceVariable(variable.name, currentForceValue(variable), variable.type_name),
  )
  const error = commandError || statusError || runtimeFault || (collapsed ? alarmStatusError : null)

  return (
    <section aria-label="Monitor" className="flex h-full min-h-0 min-w-0 flex-col bg-background">
      <div className="shrink-0 [&_.ia2-pane-header]:h-10 [&_.ia2-pane-header]:min-h-10 [&_.ia2-pane-header]:flex-nowrap [&_.ia2-pane-header]:py-0 [&_.ia2-pane-header>div:first-child>div]:flex-nowrap [&_.ia2-pane-header>div:first-child>div>div]:flex-nowrap">
        <PaneHeader title="Monitor" meta={<>
          <RunningPill running={running} isRunning={isRunning} />
          {mode && <span className={cn("text-xs", mode !== "running" && "text-warn")}>{mode === "step" ? "Stepping" : mode === "paused" ? "Paused" : "Running"}</span>}
          {alarmCount > 0 && <span className="text-destructive">{alarmCount} standing alarms</span>}
          {collapsed && error && <span role="alert" title={error} className="max-w-72 truncate text-destructive">{error}</span>}
        </>} actions={<>
          {lastSnapshot && <span className="font-mono text-xs text-muted-foreground">{stale && "Last "}scan #{Number(lastSnapshot.scan_count)}</span>}
          <Button variant="ghost" size="icon-sm" title={collapsed ? "Expand Monitor" : "Collapse Monitor"} aria-label={collapsed ? "Expand Monitor" : "Collapse Monitor"} aria-expanded={!collapsed} aria-controls="monitor-content" onClick={onToggleCollapse}>
            {collapsed ? <ChevronUp /> : <ChevronDown />}
          </Button>
        </>} />
      </div>
      <div id="monitor-content" hidden={collapsed} className={cn("min-h-0 flex-1 flex-col", !collapsed && "flex")}>
        {(vars.length > 0 || (isRunning && !attached)) && <div className="flex shrink-0 flex-wrap items-center justify-between gap-3 px-4 py-2">
          {vars.length > 0 && <label className="flex h-8 min-w-40 max-w-72 flex-1 items-center gap-2 rounded border border-input bg-background px-2">
            <Search className="size-4 text-muted-foreground" />
            <input aria-label="Search variables" placeholder="Search variables" value={query} onChange={event => setQuery(event.target.value)} className="min-w-0 flex-1 bg-transparent text-[13px] outline-none" />
            <span className="text-xs text-muted-foreground">{visibleVars.length}</span>
          </label>}
          {isRunning && !attached && <DebugToolbar mode={mode} pending={pendingCommand} disabled={!!pendingCommand || !!statusError || !mode}
            onPause={() => void command("Pause", pauseRuntime)} onResume={() => void command("Resume", resumeRuntime)} onStep={() => void command("Step", () => stepRuntime(1))} />}
          {attached && <span className="text-xs text-muted-foreground">Remote values · read only</span>}
        </div>}
        {error && <div role="alert" className="shrink-0 border-y border-destructive/20 bg-destructive/5 px-4 py-2 text-xs text-destructive">{error}</div>}
        {pendingCommand && <div role="status" className="shrink-0 px-4 pb-2 text-xs text-muted-foreground">{pendingCommand}… waiting for confirmation</div>}
        <div className="min-h-0 flex-1 overflow-auto">
          {isRunning && <AlarmsSection onStandingChange={setAlarmCount} onErrorChange={setAlarmStatusError} />}
          {pinnedSeries.length > 0 && <div className="border-b border-border px-4 py-3"><TrendChart series={pinnedSeries} windowS={MONITOR_TREND_WINDOW_S} /></div>}
          {!lastSnapshot ? <EmptyState className="px-4 py-4 [&_h2+div]:mt-1" icon={<Play />} title={isRunning ? "Waiting for live values" : "No program running"} description={isRunning ? "The first scan snapshot will appear here." : "Run a program to inspect values and trace variables."} />
            : vars.length === 0 ? <EmptyState className="px-4 py-4 [&_h2+div]:mt-1" title="No variables in this snapshot" description="The monitor will update when variables become available." />
            : visibleVars.length === 0 ? <EmptyState className="px-4 py-4 [&_h2+div]:mt-1" title="No matching variables" description="Search by variable name or IEC type." />
            : <table className="w-full min-w-[520px] border-collapse text-[13px]">
              <thead className="sticky top-0 z-10 bg-muted text-left text-xs text-muted-foreground"><tr>
                <th className="w-10 px-2 py-2"><span className="sr-only">Trend</span></th><th className="px-2 py-2 font-medium">Variable</th><th className="px-2 py-2 font-medium">Type</th><th className="px-2 py-2 font-medium">Recent history</th><th className="px-2 py-2 text-right font-medium">Value</th><th className="w-16 px-2 py-2 text-center font-medium">Force</th>
              </tr></thead>
              <tbody>{visibleVars.map((variable, index) => <VarRow key={`${index}:${variable.name}`} v={variable}
                history={historyRef.current.get(variable.name) ?? []} isPinned={pinned.has(variable.name)} sparkColor={colorByName[variable.name]} onPin={togglePin}
                stale={stale} canWrite={canWrite && !pendingCommand} forced={forces.has(variable.name)} onToggleForce={onToggleForce}
                onWrite={(value) => command(`Write ${variable.name}`, () => writeVariable(variable.name, value, variable.type_name))} />)}</tbody>
            </table>}
        </div>
      </div>
    </section>
  )
}

// ============================================================
//   RunningPill — header chip that labels WHICH program(s) the
//   variables below belong to. Three variants:
//
//     - isolated  (ProgramPane Run): one PROGRAM name in FX Green
//     - scheduled (TasksPane Run):   list of PROGRAM names
//     - remote    (attached to edge): edge alias + "remote" tag
//
//   When `running` is null but `isRunning` is true (race window
//   between SSE `started` and our local state catching up), fall
//   back to a neutral "running" tag so the header doesn't lie.
// ============================================================

function RunningPill({
  running,
  isRunning,
}: {
  running: RunningInfo
  isRunning: boolean
}) {
  if (!running) {
    if (isRunning) {
      return <Tag color="highlight">running</Tag>
    }
    return null
  }
  if (running.kind === "isolated") {
    return (
      <Tag color="highlight" title={`Running ad-hoc from ${running.filePath}.st`}>
        <span className="font-mono">{running.program}</span>
        <span className="opacity-60">isolated</span>
      </Tag>
    )
  }
  if (running.kind === "scheduled") {
    const names = running.programs
    const label =
      names.length === 0
        ? "(empty schedule)"
        : names.length <= 3
          ? names.join(", ")
          : `${names.slice(0, 2).join(", ")} +${names.length - 2}`
    return (
      <Tag
        // Empty schedule = the scan loop is up but nothing executes; show it
        // muted, not highlighted, so it doesn't read as active program work.
        color={names.length === 0 ? "muted" : "highlight"}
        title={
          names.length > 0
            ? `Running ${names.length} PROGRAM instance${names.length > 1 ? "s" : ""}: ${names.join(", ")}`
            : "tasks.toml has no PROGRAM bindings — nothing is executing"
        }
      >
        <span className="font-mono">{label}</span>
        <span className="opacity-60">scheduled</span>
      </Tag>
    )
  }
  // remote
  return (
    <Tag color="muted" title={`Attached to edge ${running.edge}`}>
      <span className="font-mono">{running.edge}</span>
      <span className="opacity-60">remote</span>
    </Tag>
  )
}

/**
 * Inline pause / step / resume control + mode badge for the Monitor
 * header. Three icon buttons total — operators recognise the play /
 * pause / step pattern from every media UI they've ever used. The
 * mode badge ("PAUSED" / "STEP") shows up only when off the default
 * Running state, so the toolbar stays quiet during normal operation.
 */
function DebugToolbar({ mode, pending, disabled, onPause, onResume, onStep }: {
  mode: "running" | "paused" | "step" | null
  pending: string | null
  disabled: boolean
  onPause: () => void
  onResume: () => void
  onStep: () => void
}) {
  return <div className="flex items-center gap-2" aria-label="Scan controls" aria-busy={!!pending}>
    {mode === "paused" || mode === "step" ? <Button size="sm" disabled={disabled} onClick={onResume} title="Resume continuous scanning"><Play />Resume</Button>
      : <Button variant="outline" size="sm" disabled={disabled} onClick={onPause} title="Pause scan loop (freeze IO + program)"><Pause />Pause</Button>}
    <Button variant="outline" size="sm" disabled={disabled} onClick={onStep} title="Step one scan cycle (auto-pause after)"><StepForward />Step</Button>
  </div>
}

function Tag({
  color,
  title,
  children,
}: {
  color: "highlight" | "muted"
  title?: string
  children: React.ReactNode
}) {
  return (
    <span
      title={title}
      className={cn(
        "inline-flex items-center gap-1.5 rounded px-1.5 py-0.5 font-medium normal-case tracking-normal",
        color === "highlight"
          ? "bg-highlight/15 text-highlight"
          : "border border-border bg-muted/50 text-muted-foreground",
      )}
    >
      {children}
    </span>
  )
}

// ============================================================
//   Per-variable row — branches on the type category so each
//   IEC 61131-3 family gets a renderer that fits how an operator
//   actually reads it. Numerics trend, booleans flip, time scales
//   to seconds, bit masks render hex. FB instances (PID, etc.)
//   are scratch storage so we collapse them to a quiet "instance"
//   label rather than showing meaningless byte offsets.
// ============================================================

interface VarRowProps {
  v: VarValue
  history: (number | null)[]
  isPinned: boolean
  sparkColor: string | undefined
  onPin: (name: string) => void
  stale: boolean
  canWrite: boolean
  /** Whether this variable is currently forced (pinned across scans). */
  forced: boolean
  /** Toggle the force state for this variable. */
  onToggleForce: (v: VarValue) => void
  onWrite: (value: number) => Promise<boolean>
}

function VarRow({ v, history, isPinned, sparkColor, onPin, stale, canWrite, forced, onToggleForce, onWrite }: VarRowProps) {
  const category = classifyType(v.type_name)
  const trendable = category === "numeric" || category === "bool" || category === "bits"
  const inputStale = v.input?.stale === true
  const writable = canWrite && !inputStale && canWriteMonitorType(v.type_name)
  // Removing an existing override does not derive a new value from the
  // stale input. Keep this escape path even while new writes are disabled.
  const forceable = writable || (canWrite && forced && canWriteMonitorType(v.type_name))
  return <tr className={cn("h-9 border-b border-border/50 hover:bg-muted/50", (stale || inputStale) && "text-muted-foreground", isPinned && "bg-selection/50")}>
    <td className="px-2">{trendable && <Button variant="ghost" size="icon-xs" aria-label={`${isPinned ? "Unpin" : "Pin"} ${v.name} ${isPinned ? "from" : "to"} trend`} aria-pressed={isPinned} onClick={() => onPin(v.name)} title={isPinned ? "Unpin from trend" : "Pin to trend"}><Pin className={cn(isPinned ? "text-foreground" : "text-muted-foreground", isPinned && "fill-current rotate-45")} /></Button>}</td>
    <th scope="row" className="max-w-56 truncate px-2 text-left font-mono font-normal" title={v.name}>{v.name}
      {inputStale && <span className="ml-2 text-xs text-warn" title="Last-known input, not live feedback">Stale · {v.input!.device}/{v.input!.channel}</span>}
    </th>
    <td className="px-2 font-mono text-xs text-muted-foreground">{v.type_name}</td>
    <td className="w-1/3 min-w-24 px-2"><div className="h-5 max-w-72"><CategoryVisual cat={category} v={v} history={history} sparkColor={sparkColor} /></div></td>
    <td className="px-2 text-right"><ValueCell v={v} cat={category} canWrite={writable} onWrite={onWrite} /></td>
    <td className="px-2 text-center">{forceable ? <Button variant="ghost" size="icon-xs" aria-label={`${forced ? "Unforce" : "Force"} ${v.name}`} aria-pressed={forced} onClick={() => onToggleForce(v)} title={forced ? `Unforce ${v.name} (resume program control)` : `Force ${v.name} = current value`}>
      {forced ? <Lock className="text-destructive" /> : <Unlock className="text-muted-foreground" />}
    </Button> : forced ? <Lock aria-label={`${v.name} is forced`} className="mx-auto size-4 text-destructive" /> : <span className="text-muted-foreground">—</span>}</td>
  </tr>
}

function ValueCell({ v, cat, canWrite, onWrite }: { v: VarValue; cat: VarCategory; canWrite: boolean; onWrite: (value: number) => Promise<boolean> }) {
  if (canWrite && cat === "bool") {
    const on = v.value === "TRUE"
    return <button type="button" onClick={() => void onWrite(on ? 0 : 1)} className={cn("h-7 min-w-20 rounded px-2 text-right font-mono text-[13px]", on ? "bg-highlight/10 text-highlight" : "bg-muted text-foreground")} aria-label={`Toggle ${v.name}`} title="Click to toggle">{on ? "TRUE" : "FALSE"}</button>
  }
  if (canWrite && cat === "numeric") return <NumericEditor name={v.name} typeName={v.type_name} value={v.value} onWrite={onWrite} />
  return <span className="inline-block min-w-20 font-mono text-[13px] tabular-nums" title={!canWriteMonitorType(v.type_name) ? "Read only" : undefined}>{renderValue(cat, v)}</span>
}

/** Draft state stays local while snapshots continue. A ref consumes the draft
 * before blur fires, so Escape cannot submit and Enter cannot submit twice. */
export function NumericEditor({ name, typeName, value, onWrite }: {
  name: string; typeName: string; value: string; onWrite: (value: number) => Promise<boolean>
}) {
  const [draft, setDraft] = useState(value)
  const [editing, setEditing] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const dirty = useRef(false)
  useEffect(() => { if (!editing) setDraft(value) }, [value, editing])
  const commit = async () => {
    if (!dirty.current) return
    dirty.current = false
    setEditing(false)
    try {
      const parsed = monitorWriteValue(draft, typeName)
      if (!await onWrite(parsed)) setDraft(value)
      else setError(null)
    } catch (cause) { setDraft(value); setError(String(cause)) }
  }
  return <div className="inline-flex max-w-48 flex-col items-end">
    <input type="text" inputMode="decimal" aria-label={`Value for ${name}`} aria-invalid={!!error} value={draft}
      onFocus={() => setEditing(true)} onChange={event => { dirty.current = true; setDraft(event.target.value); setEditing(true); setError(null) }}
      onBlur={() => { setEditing(false); void commit() }} onKeyDown={event => {
        if (event.key === "Enter") { event.preventDefault(); void commit(); event.currentTarget.blur() }
        if (event.key === "Escape") { event.preventDefault(); dirty.current = false; setDraft(value); setEditing(false); setError(null); event.currentTarget.blur() }
      }}
      className={cn("h-7 w-24 rounded border border-transparent bg-transparent px-2 text-right font-mono text-[13px] tabular-nums hover:border-input focus:border-ring focus:bg-background focus:outline-none", error && "border-destructive")}
      title="Enter to write · Escape to cancel" />
    {error && <span role="alert" className="text-xs text-destructive">{error}</span>}
  </div>
}

/** The middle column — the visual that conveys "what's happening
 *  with this variable over time, at a glance". Different per category. */
function CategoryVisual({
  cat,
  v,
  history,
  sparkColor,
}: {
  cat: VarCategory
  v: VarValue
  history: (number | null)[]
  sparkColor: string | undefined
}) {
  switch (cat) {
    case "numeric": {
      const defaultColor = "text-trend"
      return (
        <span
          className={cn("block h-4 w-full", !sparkColor && defaultColor)}
          style={sparkColor ? { color: sparkColor } : undefined}
        >
          <Sparkline values={history} width={120} height={18} filled />
        </span>
      )
    }
    case "bool":
      return <BoolStrip history={history} sparkColor={sparkColor} />
    case "bits":
      return <BitsVisual hex={v.value} />
    case "time":
    case "text":
    case "fb":
    case "other":
      // Nothing to chart — leave the middle column empty so the value
      // column on the right does the talking.
      return <span className="block h-4 w-full" />
  }
}

/** Compact strip of segments showing the last ~80 BOOL transitions —
 *  green when true, muted when false. Faster to read at a glance than
 *  a 0/1 step-trace sparkline. */
function BoolStrip({
  history,
  sparkColor,
}: {
  history: (number | null)[]
  sparkColor: string | undefined
}) {
  // Take the last 80 ticks (the strip is 120 px wide → 1.5 px per cell).
  const last = history.slice(-80)
  return (
    <span className="flex h-4 w-full items-center gap-px overflow-hidden">
      {last.length === 0 ? (
        <span className="text-xs text-muted-foreground/60">—</span>
      ) : (
        last.map((v, i) => (
          <span
            key={i}
            className={cn(
              "h-2.5 flex-1 rounded-[1px]",
              v === null ? "bg-transparent" : v > 0.5
                ? sparkColor
                  ? ""
                  : "bg-highlight/80"
                : "bg-muted-foreground/20",
            )}
            title={v === null ? "Stale input" : undefined}
            style={v !== null && v > 0.5 && sparkColor ? { backgroundColor: sparkColor } : undefined}
          />
        ))
      )}
    </span>
  )
}

/** Bits — render the value as monospace hex pill so each nibble lines
 *  up; useful for visually spotting which bits are set in an alarm
 *  register (`16#0013` jumps out as different from `16#0010`). */
function BitsVisual({ hex }: { hex: string }) {
  const digits = stripHexPrefix(hex)
  return (
    <span className="flex h-4 items-center">
      <span className="rounded border border-border bg-muted/40 px-1.5 font-mono text-xs tracking-wider text-foreground">
        {digits}
      </span>
    </span>
  )
}

/** Right-hand value column: tweak per category so each looks "right"
 *  rather than uniformly using the raw bridge string. */
function renderValue(cat: VarCategory, v: VarValue): string {
  switch (cat) {
    case "time":
      return prettyTime(v.value)
    case "bool":
      return v.value === "TRUE" ? "on" : "off"
    case "fb":
      return "instance"
    case "text":
      return v.value
    default:
      return v.value
  }
}

// ============================================================
//   Alarms — a calm summary that only takes on colour when
//   something is standing (ISA-101). Polls GET /api/runtime/alarms
//   every 2 s while the Monitor is running; the backend pre-sorts
//   standing-first. Status colours retain their alarm meanings.
// ============================================================

const ALARM_TONE_CLS: Record<AlarmTone, { chip: string; text: string }> = {
  muted: {
    chip: "bg-muted/60 text-muted-foreground",
    text: "text-muted-foreground",
  },
  warn: { chip: "bg-warn/15 text-warn", text: "text-foreground" },
  alert: {
    chip: "bg-destructive/15 text-destructive",
    text: "text-foreground",
  },
}

function AlarmsSection({ onStandingChange, onErrorChange }: { onStandingChange: (count: number) => void; onErrorChange: (error: string | null) => void }) {
  const [alarms, setAlarms] = useState<AlarmState[]>([])
  const [pollError, setPollError] = useState<string | null>(null)
  const [ackError, setAckError] = useState<string | null>(null)
  const [acking, setAcking] = useState<string | null>(null)
  const [showAll, setShowAll] = useState(false)
  const alarmError = ackError || pollError
  useEffect(() => {
    let cancelled = false
    const tick = async () => {
      try {
        const a = await fetchRuntimeAlarms()
        if (!cancelled) { setAlarms(a); setPollError(null) }
      } catch (error) {
        if (!cancelled) setPollError(`Alarm status unavailable: ${String(error)}`)
      }
    }
    void tick()
    const id = setInterval(tick, 2000)
    return () => {
      cancelled = true
      clearInterval(id)
    }
  }, [])

  const standing = standingCount(alarms)
  useEffect(() => { onStandingChange(standing); return () => onStandingChange(0) }, [standing, onStandingChange])
  useEffect(() => { onErrorChange(alarmError); return () => onErrorChange(null) }, [alarmError, onErrorChange])
  const hasCritical = alarms.some(
    (a) => alarmStanding(a) && severityTone(a.severity, true) === "alert",
  )

  const ack = useCallback(async (id: string) => {
    setAcking(id)
    setAckError(null)
    try {
      const updated = await ackRuntimeAlarm(id)
      setAlarms(previous => previous.map(alarm => alarm.id === id ? updated : alarm))
    } catch (error) { setAckError(`Acknowledge failed: ${String(error)}`) }
    finally { setAcking(null) }
  }, [])

  return (
    <div className="shrink-0 border-b border-border bg-background/40">
      <div className="flex min-h-9 items-center justify-between gap-3 px-4 text-xs text-muted-foreground">
        <span className="flex items-center gap-3"><span className="font-medium">Alarms</span>
        {standing === 0 ? <span>No standing alarms</span> : (
          <span
            className={cn(
              "rounded px-1.5 py-0.5 font-mono normal-case tracking-normal",
              hasCritical
                ? "bg-destructive/15 text-destructive"
                : "bg-warn/15 text-warn",
            )}
          >
            {standing} standing
          </span>
        )}</span>
        {alarms.length > standing && <Button variant="ghost" size="sm" aria-expanded={showAll} onClick={() => setShowAll(value => !value)}>{showAll ? "Standing only" : `Show all ${alarms.length}`}</Button>}
      </div>
      {alarmError && <div role="alert" className="px-4 py-2 text-xs text-destructive">{alarmError}</div>}
      {(showAll || standing > 0) && (
        <ul className="max-h-40 divide-y divide-border/50 overflow-auto pb-1">
          {(showAll ? alarms : alarms.filter(alarmStanding)).map((a) => (
            <AlarmRow key={a.id} a={a} onAck={ack} disabled={acking !== null} />
          ))}
        </ul>
      )}
    </div>
  )
}

function AlarmRow({
  a,
  onAck,
  disabled,
}: {
  a: AlarmState
  onAck: (id: string) => void
  disabled: boolean
}) {
  const standing = alarmStanding(a)
  const tone = ALARM_TONE_CLS[severityTone(a.severity, standing)]
  return (
    <li
      className={cn(
        "flex min-h-9 items-center gap-3 px-4 py-1 text-xs",
        !standing && "opacity-60",
      )}
    >
      <span
        className={cn(
          "shrink-0 rounded px-1 py-px font-mono text-xs",
          tone.chip,
        )}
      >
        {a.severity}
      </span>
      <span
        className={cn("min-w-0 flex-1 truncate", tone.text)}
        title={`${a.variable} — ${a.message}`}
      >
        {a.message}
      </span>
      <span
        className="shrink-0 font-mono text-xs tabular-nums text-muted-foreground"
        title="raised at"
      >
        {a.count === 0 ? "Never raised" : fmtAlarmClock(a.raised_at_us)}
      </span>
      <span
        className="hidden shrink-0 font-mono text-xs tabular-nums text-muted-foreground sm:inline"
        title="value at raise"
      >
        {a.count === 0 ? "—" : fmtAlarmValue(a.value_at_raise)}
      </span>
      {a.count > 1 && (
        <span
          className="shrink-0 font-mono text-xs tabular-nums text-muted-foreground/70"
          title="times raised"
        >
          ×{a.count}
        </span>
      )}
      {!a.acked ? (
        <button
          type="button"
          onClick={() => onAck(a.id)}
          disabled={disabled}
          className="shrink-0 rounded border border-border bg-card px-1.5 py-px font-mono text-xs text-muted-foreground hover:text-foreground"
          title="Acknowledge"
        >
          ack
        </button>
      ) : (
        <span
          className="shrink-0 font-mono text-xs text-muted-foreground/40"
          title="acknowledged"
        >
          ackd
        </span>
      )}
    </li>
  )
}
