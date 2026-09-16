/**
 * The HMI canvas — renders one screen document live against the running
 * program, and renders it INCREMENTALLY: every `hmi` SSE mutation reloads
 * the document, and the node ids the mutation touched get a brief spawn
 * animation, so a watching human sees an agent assemble the screen
 * element by element (the Pencil workflow, pointed at a plant).
 *
 * Two explicit modes keep gestures unambiguous:
 *   - Operate: actions are live (tap a valve, commit a setpoint); nothing
 *     moves. The default — this is an operator surface first.
 *   - Arrange: drag-to-move with grid snap (saved on release); actions
 *     are inert so a mis-tap can't write to the plant while laying out.
 */

import { useCallback, useEffect, useRef, useState } from "react"
import { Button } from "@/components/ui/button"
import { EmptyState } from "@/components/ui/empty-state"
import { fitCanvasScale } from "./canvas-viewport"

import {
  clampNotice,
  confirmSummary,
  parseCommitText,
  resolveActionWrite,
  type ResolvedWrite,
} from "@/lib/hmi-action"
import { canHostAction } from "@/lib/hmi-actions"
import {
  bindingVariable,
  colorBinding,
  cssColor,
  displayBinding,
  lookupVar,
  resolveBinding,
  resolveOn,
} from "@/lib/hmi-binding"
import {
  historyToSamples,
  pushTimedHistory,
  seedTimedBuffer,
  windowSlice,
  type TimedSample,
} from "@/lib/var-history"
import {
  alarmStanding,
  fmtAlarmClock,
  severityTone,
  standingCount,
  type AlarmTone,
} from "@/lib/alarms"
import { cn } from "@/lib/utils"
import { useHmiMutation } from "@/state/hmi-live"
import { liveFeedStore, useConnected, useLastSnapshot } from "@/state/live-feed"
import { TrendChart, type TrendSeries } from "@/components/charts/TrendChart"
import type { AlarmState } from "@/types/generated/AlarmState"
import type { HmiAction } from "@/types/generated/HmiAction"
import type { HmiDoc } from "@/types/generated/HmiDoc"
import type { HmiNode } from "@/types/generated/HmiNode"

import { useHmiHost, type HmiHost, type HmiRuntimeState } from "./host"
import { derivePanelHealth, type PanelTone } from "./panel-health"
import { HmiSymbol, type SymbolLive } from "./symbols"

export type CanvasMode = "operate" | "arrange"

/** Per-element delay inside one spawn batch (the "wave"). */
const SPAWN_STAGGER_MS = 80

/** Does any of this node's actions reach the plant? `nav` does not — it stays
 *  usable when writes are blocked, the same way it stays usable offline. */
function nodeWrites(node: HmiNode): boolean {
  return Object.values(node.action).some((a) => a != null && a.kind !== "nav")
}

/** Refusal text for stale live data, naming the budget actually in force.
 *  That budget widens to cover a slow-cycle project's own scan cadence, so
 *  stating a bare "2 seconds" would be a lie on exactly the projects where
 *  the number matters. */
const staleFeedReason = (): string =>
  `No advancing live data within ${
    Math.round(liveFeedStore.getActionBudgetMs() / 100) / 10
  }s — action not sent`

type PendingConfirm = {
  nodeId: string
  action: HmiAction
  /** Resolved at request time — Confirm sends exactly this. */
  write: ResolvedWrite
  document: HmiDoc
  path: string
  host: HmiHost
  feedGeneration: number
  documentVersion: number
}

export function HmiCanvas({
  path,
  mode,
  selected,
  onSelect,
  onDocLoaded,
}: {
  path: string
  mode: CanvasMode
  selected: string | null
  onSelect: (id: string | null) => void
  onDocLoaded?: (doc: HmiDoc) => void
}) {
  const host = useHmiHost()
  const [doc, setDoc] = useState<HmiDoc | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const snapshot = useLastSnapshot()
  const connected = useConnected()
  const mutation = useHmiMutation()
  const loadRevision = useRef(0)

  // ---- document load + live reload --------------------------------
  const documentVersion = useRef(0)
  const load = useCallback(async () => {
    const revision = ++loadRevision.current
    try {
      const d = await host.fetchDoc(path)
      if (revision !== loadRevision.current) return
      documentVersion.current = revision
      setDoc(d)
      setLoadError(null)
      onDocLoaded?.(d)
    } catch (e) {
      if (revision !== loadRevision.current) return
      setLoadError(String(e))
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path, host])

  useEffect(() => {
    setDoc(null)
    setLoadError(null)
    void load()
    return () => { loadRevision.current++ }
  }, [load])

  // Spawn animation bookkeeping: ids touched by the latest mutation get
  // the class until their timer expires. A ref map keeps timers out of
  // render; the `anim` state just triggers the re-render.
  const spawnRef = useRef<Map<string, { until: number; order: number }>>(new Map())
  const [, bumpAnim] = useState(0)
  useEffect(() => {
    if (!mutation || mutation.path !== path) return
    if (mutation.deleted) {
      loadRevision.current++
      setDoc(null)
      setLoadError("screen was deleted")
      return
    }
    void load()
    if (mutation.touched.length > 0) {
      // Per-batch stagger: each touched element's frame/pop/glow chain
      // starts SPAWN_STAGGER_MS after the previous one, so a batch
      // reads as a wave of drawing. The expiry covers the last
      // element's full chain.
      const life = 1400 + mutation.touched.length * SPAWN_STAGGER_MS
      const until = Date.now() + life
      mutation.touched.forEach((id, order) => {
        spawnRef.current.set(id, { until, order })
      })
      bumpAnim((n) => n + 1)
      const t = setTimeout(() => {
        const now = Date.now()
        for (const [id, s] of spawnRef.current) {
          if (s.until <= now) spawnRef.current.delete(id)
        }
        bumpAnim((n) => n + 1)
      }, life + 50)
      return () => clearTimeout(t)
    }
  }, [mutation, path, load])

  // The scaled footprint owns scrolling; the transformed document is
  // positioned inside it so its unscaled box cannot create phantom overflow.
  const wrapRef = useRef<HTMLDivElement | null>(null)
  const [zoom, setZoom] = useState<"fit" | number>("fit")
  const [viewport, setViewport] = useState({ width: 0, height: 0 })
  useEffect(() => {
    const element = wrapRef.current
    if (!element || !doc) return
    const measure = () => setViewport({ width: Math.max(0, element.clientWidth - 24), height: Math.max(0, element.clientHeight - 24) })
    measure()
    const observer = new ResizeObserver(measure)
    observer.observe(element)
    return () => observer.disconnect()
  }, [doc?.grid.w, doc?.grid.h])
  useEffect(() => { setZoom("fit") }, [path])
  const scale = zoom === "fit" && doc
    ? fitCanvasScale(viewport.width, viewport.height, doc.grid.w, doc.grid.h)
    : zoom === "fit" ? 1 : zoom

  // ---- trend history (one timed ring buffer per referenced variable,
  // retained for the widest window_s among the nodes referencing it;
  // each node slices its own window at render) ----
  const historyRef = useRef<Map<string, TimedSample[]>>(new Map())
  const [, bumpSeed] = useState(0)
  useEffect(() => {
    if (!snapshot || !doc) return
    // Samples ride the snapshot's own time base (scan-relative micros →
    // seconds) so backfilled history lands on the same axis and merges.
    if (snapshot.timestamp_us <= 0n) return
    const t = Number(snapshot.timestamp_us) / 1e6
    for (const [name, windowS] of trendWindows(doc)) {
      const found = lookupVar(snapshot, name)
      if (!found) continue
      const n = Number.isNaN(Number(found.raw))
        ? (/^true$/i.test(found.raw.trim()) ? 1 : 0)
        : Number(found.raw)
      let buf = historyRef.current.get(name)
      if (!buf) {
        buf = []
        historyRef.current.set(name, buf)
      }
      pushTimedHistory(buf, t, n, windowS)
    }
  }, [snapshot, doc])

  // Backfill: when the screen's trend/sparkline variables change, seed
  // their ring buffers from stored history so a reload keeps the trace
  // instead of starting empty. Best-effort — the live feed fills forward
  // regardless. Re-runs on every doc reload; already-buffered live
  // samples are preserved by the merge.
  useEffect(() => {
    if (!doc) return
    const windows = trendWindows(doc)
    const vars = [...windows.keys()]
    if (vars.length === 0) return
    let cancelled = false
    void host
      .history(vars, 1000)
      .then((resp) => {
        if (cancelled) return
        for (const s of resp.series) {
          const windowS = windows.get(s.name) ?? SPARKLINE_WINDOW_S
          const existing = historyRef.current.get(s.name) ?? []
          historyRef.current.set(
            s.name,
            seedTimedBuffer(existing, historyToSamples(s.points), windowS),
          )
        }
        bumpSeed((n) => n + 1)
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [doc, host])

  // ---- drag-to-move (Arrange mode) --------------------------------
  // Gesture data lives in the ref (single source of truth — pointerup may
  // fire in the same task as the last move, before any re-render); the
  // state mirror only drives the visual position during the drag.
  const dragRef = useRef<{
    id: string
    startX: number
    startY: number
    origX: number
    origY: number
    curX: number
    curY: number
    moved: boolean
  } | null>(null)
  const [dragPos, setDragPos] = useState<{ id: string; x: number; y: number } | null>(null)

  const onNodePointerDown = (n: HmiNode, e: React.PointerEvent) => {
    // Selection is an Arrange-mode concept. In Operate a tap is an
    // ACTION — selecting here used to pop the inspector, reflow the
    // canvas mid-click, and swallow the click's tap under the moved
    // layout (real mice lost actions to it, not just automation).
    if (mode !== "arrange") return
    onSelect(n.id)
    e.preventDefault()
    try {
      ;(e.target as Element).setPointerCapture?.(e.pointerId)
    } catch {
      /* synthetic events (tests) have no active pointer — capture is
       * an optimisation, not a requirement */
    }
    dragRef.current = {
      id: n.id,
      startX: e.clientX,
      startY: e.clientY,
      origX: n.x,
      origY: n.y,
      curX: n.x,
      curY: n.y,
      moved: false,
    }
  }
  const onPointerMove = (e: React.PointerEvent) => {
    const d = dragRef.current
    if (mode !== "arrange" || !d || !doc) return
    const snap = Math.max(1, doc.grid.snap)
    const nx =
      Math.round((d.origX + (e.clientX - d.startX) / scale) / snap) * snap
    const ny =
      Math.round((d.origY + (e.clientY - d.startY) / scale) / snap) * snap
    if (nx !== d.origX || ny !== d.origY) d.moved = true
    d.curX = nx
    d.curY = ny
    setDragPos({ id: d.id, x: nx, y: ny })
  }
  const onPointerUp = async () => {
    const d = dragRef.current
    dragRef.current = null
    setDragPos(null)
    if (mode !== "arrange" || !d || !doc || !d.moved || !host.saveDoc) return
    const next = structuredClone(doc)
    const target = findNode(next.root, d.id)
    if (target) {
      target.x = d.curX
      target.y = d.curY
      setDoc(next)
      try {
        await host.saveDoc(path, next)
      } catch (error) {
        setActionError(`Layout was not saved: ${String(error)}`)
        void load() // server rejected — resync to truth
      }
    }
  }

  // ---- actions (Operate mode) -------------------------------------
  const [pending, setPending] = useState<PendingConfirm | null>(null)
  const [actionError, setActionError] = useState<string | null>(null)
  useEffect(() => {
    setPending(null)
    dragRef.current = null
    setDragPos(null)
  }, [mode, path])

  // Refs cover changes that happen while a fresh status request is in flight.
  const actionContext = useRef({ host, path, mode, doc, loadError })
  actionContext.current = { host, path, mode, doc, loadError }
  const mounted = useRef(false)
  const writing = useRef(false)
  useEffect(() => {
    mounted.current = true
    return () => { mounted.current = false }
  }, [])

  const checkWrite = useCallback((request: PendingConfirm) => {
    // First: a host that cannot deliver a write at all. Checked ahead of the
    // live-state gates so the operator gets the real reason rather than a
    // freshness message about a runtime this write would never reach.
    if (request.host.writesBlocked) throw new Error(request.host.writesBlocked)
    const current = actionContext.current
    if (!mounted.current || current.mode !== "operate" || current.host !== request.host ||
        current.path !== request.path || current.doc !== request.document || current.loadError ||
        request.documentVersion !== documentVersion.current ||
        request.documentVersion !== loadRevision.current) {
      throw new Error("Screen changed — request the action again")
    }
    if (request.feedGeneration !== liveFeedStore.getGeneration()) {
      throw new Error("Live connection or run changed — request the action again")
    }
    const fresh = liveFeedStore.getFreshSnapshot()
    if (!fresh) throw new Error(staleFeedReason())
    const node = findNode(request.document.root, request.nodeId)
    if (!node || !canHostAction(node.type)) throw new Error("Control unavailable — action not sent")
    const enabled = node.bind["enable"]
    if (enabled !== undefined && !resolveOn(fresh, enabled)) {
      throw new Error("Control is no longer enabled — action not sent")
    }
    const variable = lookupVar(fresh, request.write.variable)
    if (!variable || variable.type_name !== request.write.typeName) {
      throw new Error("Live variable or type changed — request the action again")
    }
  }, [])

  // Keep the value/type shown in the card; recheck permission to dispatch it.
  // Pulse reset remains one runtime-side request, never a browser timer.
  const performWrite = useCallback(async (request: PendingConfirm) => {
    if (writing.current) {
      setActionError("Another action is awaiting a response — action not sent")
      return
    }
    writing.current = true
    let timer: ReturnType<typeof setTimeout> | undefined
    let status: HmiRuntimeState | undefined
    try {
      checkWrite(request)
      status = await Promise.race([
        request.host.runtimeState(),
        new Promise<never>((_, reject) => {
          timer = setTimeout(() => reject(new Error("Runtime status timed out — action not sent")), 2000)
        }),
      ])
      // Device health is deliberately NOT part of this gate. It is not a
      // property of the runtime, it is a property of ONE device, and the
      // panel cannot tell which device carries this variable — the edge
      // panel has no iomap at all. Blanket-refusing on any unhealthy device
      // took away every control, Stop included, because some unrelated
      // island dropped. The runtime scopes it to the mapping and reports
      // the caveat below; everything else here is genuinely runtime-wide.
      const state = status
      const health = derivePanelHealth({ ...state, unhealthyDevices: [] }, 0)
      if (health.kind !== "running") throw new Error(`${health.text} — action not sent`)
      checkWrite(request)
      const undelivered = await request.host.write(
        request.write.variable,
        request.write.value,
        request.write.typeName,
        request.action.kind === "pulse" ? request.action.ms : undefined,
      )
      // A write that landed in the program but not on the bus is not a
      // failure and not a success — say exactly that, and never retry.
      if (undelivered && mounted.current) setActionError(undelivered)
    } catch (e) {
      if (mounted.current) setActionError(String(e))
    } finally {
      clearTimeout(timer)
      writing.current = false
      // Learn the cadence from this read — an IDE screen without an alarmbar
      // has no other poll — but only AFTER the decision. Moving the budget
      // between the two checkWrite calls would judge one action on two
      // different windows.
      if (status) liveFeedStore.setScanPeriodMs(status.scanPeriodMs)
    }
  }, [checkWrite])

  const requestAction = useCallback(
    (nodeId: string, action: HmiAction, value?: number) => {
      if (mode !== "operate") return
      setActionError(null)
      if (action.kind === "nav") {
        try {
          host.nav(action.target)
        } catch (e) {
          setActionError(String(e))
        }
        return
      }
      if (host.writesBlocked) {
        setActionError(host.writesBlocked)
        return
      }
      const fresh = liveFeedStore.getFreshSnapshot()
      if (!fresh) {
        setActionError(staleFeedReason())
        return
      }
      const res = resolveActionWrite(fresh, action, value)
      if (!res.ok) {
        setActionError(res.reason)
        return
      }
      if (!doc) return
      const request: PendingConfirm = {
        nodeId, action, write: res.write, document: doc, path, host,
        feedGeneration: liveFeedStore.getGeneration(), documentVersion: documentVersion.current,
      }
      try {
        checkWrite(request)
      } catch (e) {
        setActionError(String(e))
        return
      }
      const needsConfirm =
        "confirm" in action ? action.confirm : true
      if (needsConfirm) {
        setPending(request)
      } else {
        // A clamped no-confirm entry still writes — but never silently.
        setActionError(clampNotice(res.write))
        void performWrite(request)
      }
    },
    [mode, host, doc, path, checkWrite, performWrite],
  )

  // ---- render ------------------------------------------------------
  if (loadError) {
    return (
      <EmptyState title="Screen unavailable" description={loadError} actions={<Button variant="outline" onClick={() => void load()}>Retry</Button>} />
    )
  }
  if (!doc) {
    return (
      <EmptyState title="Loading screen…" />
    )
  }

  const rootChildren =
    doc.root.type === "group" ? doc.root.children : []

  // SSE gone while a snapshot is still on screen = every readout is
  // frozen at its last value. Dim the surface (the alarmbar carries the
  // words) so stale numbers can't pass for live ones.
  const stale = !connected && snapshot != null

  return (
    <div className="relative flex h-full min-h-0 min-w-0 flex-col bg-muted">
      <div className="flex min-h-10 shrink-0 flex-wrap items-center justify-between gap-2 border-b border-border bg-background px-3 py-1" aria-label="Canvas view controls">
        <span className="text-xs text-muted-foreground">{mode === "operate" ? "Live controls · layout locked" : "Edit layout · controls disabled"}{stale && <span className="ml-2 text-warn">Live connection lost · values frozen</span>}</span>
        <div className="flex items-center gap-1">
          <Button size="sm" variant={zoom === "fit" ? "highlight" : "ghost"} aria-pressed={zoom === "fit"} onClick={() => setZoom("fit")} title="Fit the whole screen">Fit</Button>
          <Button size="sm" variant={zoom === 1 ? "highlight" : "ghost"} aria-pressed={zoom === 1} onClick={() => setZoom(1)} title="Actual size · scroll to pan">100%</Button>
          <Button size="icon-sm" variant="ghost" aria-label="Zoom out" disabled={scale <= 0.1} onClick={() => setZoom(Math.max(0.1, Math.round((scale - 0.1) * 10) / 10))}>−</Button>
          <span className="min-w-10 text-center font-mono text-xs" aria-label="Canvas zoom">{Math.round(scale * 100)}%</span>
          <Button size="icon-sm" variant="ghost" aria-label="Zoom in" disabled={scale >= 2} onClick={() => setZoom(Math.min(2, Math.round((scale + 0.1) * 10) / 10))}>+</Button>
        </div>
      </div>
      <div ref={wrapRef} data-testid="hmi-viewport" className="relative min-h-0 min-w-0 flex-1 overflow-auto p-3" onPointerMove={onPointerMove} onPointerUp={onPointerUp}
        onClick={event => { if (event.target === event.currentTarget) onSelect(null) }}>
        <div data-testid="hmi-footprint" className="relative mx-auto overflow-hidden" style={{ width: doc.grid.w * scale, height: doc.grid.h * scale }}>
          <div data-testid="hmi-screen" className={cn("absolute left-0 top-0 origin-top-left bg-background", stale && "opacity-60")}
            style={{ width: doc.grid.w, height: doc.grid.h, transform: `scale(${scale})` }}
            onClick={event => { if (event.target === event.currentTarget) onSelect(null) }}>
        {rootChildren.map((n) => (
          <CanvasNode
            key={n.id}
            node={n}
            doc={doc}
            snapshotTick={snapshot?.scan_count}
            spawn={spawnRef.current}
            selected={selected}
            mode={mode}
            dragPos={dragPos}
            historyRef={historyRef}
            onPointerDown={onNodePointerDown}
            onAction={requestAction}
          />
        ))}
        {/* Spawn overlays — the dashed sketch frame + glow live outside
            the element so they're visible while it is still fading in.
            Coordinates are accumulated to canvas space: a node inside a
            nested group carries group-relative x/y. */}
        {[...spawnRef.current.entries()].map(([id, sp]) => {
          const hit = findNodeAbs(doc.root, id, 0, 0)
          if (!hit) return null
          const { node: n, x, y } = hit
          return (
            <div
              key={`spawn-${id}`}
              className="hmi-spawn-overlay"
              style={{
                left: x,
                top: y,
                width: n.w > 0 ? n.w : 120,
                height: n.h > 0 ? n.h : 32,
                ...({ "--spawn-delay": `${sp.order * SPAWN_STAGGER_MS}ms` } as React.CSSProperties),
              }}
            />
          )
        })}
          </div>
        </div>
      </div>

      {pending && mode === "operate" && (
        <ConfirmCard
          pending={pending}
          onCancel={() => setPending(null)}
          onConfirm={() => {
            if (mode !== "operate") return
            const p = pending
            setPending(null)
            void performWrite(p)
          }}
        />
      )}
      {actionError && (
        <div role="alert" className="shrink-0 border-t border-destructive/30 bg-background px-4 py-2 text-xs text-destructive">
          {actionError}
        </div>
      )}
    </div>
  )
}

// One node. Split out so a snapshot tick re-renders the tree cheaply —
// the components are small and the tree is tens of nodes, so plain
// rendering is fine at 10 Hz; memoization can come with evidence.
function CanvasNode({
  node,
  doc,
  snapshotTick,
  spawn,
  selected,
  mode,
  dragPos,
  historyRef,
  onPointerDown,
  onAction,
}: {
  node: HmiNode
  doc: HmiDoc
  snapshotTick: unknown
  spawn: Map<string, { until: number; order: number }>
  selected: string | null
  mode: CanvasMode
  dragPos: { id: string; x: number; y: number } | null
  historyRef: React.MutableRefObject<Map<string, TimedSample[]>>
  onPointerDown: (n: HmiNode, e: React.PointerEvent) => void
  onAction: (nodeId: string, action: HmiAction, value?: number) => void
}) {
  const snapshot = useLastSnapshot()
  const host = useHmiHost()
  const pos =
    dragPos && dragPos.id === node.id
      ? { x: dragPos.x, y: dragPos.y }
      : { x: node.x, y: node.y }
  const spawning = spawn.get(node.id)
  // Only control-surface node types fire gestures (mirrors validate_hmi's
  // action-host rule) — a tap on a text label must never reach the plant.
  const tapAction = canHostAction(node.type) ? node.action["tap"] : undefined

  // `visible` bind: 0 hides the element in Operate; Arrange keeps it
  // ghosted so it can still be selected and edited.
  const visBind = node.bind["visible"]
  const hidden =
    visBind !== undefined && (resolveBinding(snapshot, visBind) ?? 1) === 0
  if (hidden && mode === "operate") return null

  const body = renderKind(node, snapshot, historyRef, onAction, host, mode)

  // In Operate mode a tap-actionable node IS a control: give it a real
  // role and a keyboard path (Enter/Space through the same gated
  // handler as the click). Button nodes already render a native
  // <button> inside, so the wrapper stays inert for them.
  const fireTap = () => {
    if (mode !== "operate" || !tapAction) return
    const enBind = node.bind["enable"]
    if (enBind !== undefined && !resolveOn(snapshot, enBind)) return
    onAction(node.id, tapAction)
  }
  const keyboardActionable =
    mode === "operate" && tapAction !== undefined && node.type !== "button"

  return (
    <div
      data-hmi-id={node.id}
      data-hmi-type={node.type}
      role={keyboardActionable ? "button" : undefined}
      tabIndex={keyboardActionable ? 0 : undefined}
      onKeyDown={
        keyboardActionable
          ? (e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault()
                fireTap()
              }
            }
          : undefined
      }
      className={cn(
        "absolute",
        spawning !== undefined && "hmi-spawn",
        mode === "arrange" && "cursor-grab active:cursor-grabbing",
        mode === "operate" && tapAction && "cursor-pointer",
        selected === node.id &&
          "outline outline-1 outline-offset-2 outline-ring",
      )}
      style={{
        left: pos.x,
        top: pos.y,
        width: node.w > 0 ? node.w : undefined,
        height: node.h > 0 ? node.h : undefined,
        opacity: hidden ? 0.35 : undefined,
        ...(spawning !== undefined
          ? ({ "--spawn-delay": `${spawning.order * SPAWN_STAGGER_MS}ms` } as React.CSSProperties)
          : {}),
      }}
      onPointerDown={(e) => {
        e.stopPropagation()
        onPointerDown(node, e)
      }}
      onClick={(e) => {
        e.stopPropagation()
        // `bind.enable` gates the gesture itself, not just the visual —
        // a click on the wrapper around a disabled control must not
        // reach the plant either (fireTap applies the same gate).
        fireTap()
      }}
    >
      {body}
      {node.type === "group" &&
        node.children.map((c) => (
          <CanvasNode
            key={c.id}
            node={c}
            doc={doc}
            snapshotTick={snapshotTick}
            spawn={spawn}
            selected={selected}
            mode={mode}
            dragPos={dragPos}
            historyRef={historyRef}
            onPointerDown={onPointerDown}
            onAction={onAction}
          />
        ))}
    </div>
  )
}

function renderKind(
  node: HmiNode,
  snapshot: ReturnType<typeof useLastSnapshot>,
  historyRef: React.MutableRefObject<Map<string, TimedSample[]>>,
  onAction: (nodeId: string, action: HmiAction, value?: number) => void,
  host: HmiHost,
  mode: CanvasMode,
) {
  switch (node.type) {
    case "group":
      return null
    case "text": {
      const cls =
        node.style === "title"
          ? "text-[16px] font-semibold text-foreground"
          : node.style === "section"
            ? "text-xs font-medium text-muted-foreground"
            : node.style === "caption"
              ? "text-xs text-muted-foreground"
              : "text-[12px] text-foreground"
      // Live text (a mapped state label) wins over the static string;
      // a live color (map output) wins over the prop color.
      const textBind = node.bind["text"]
      const liveText =
        textBind !== undefined ? displayBinding(snapshot, textBind) : null
      const colorB = node.bind["color"]
      const liveColor =
        colorB !== undefined ? colorBinding(snapshot, colorB) : null
      const p = node.props
      const color = liveColor ?? (typeof p["color"] === "string" ? (p["color"] as string) : null)
      const style: React.CSSProperties = {}
      if (color) style.color = cssColor(color)
      if (typeof p["size"] === "number") style.fontSize = p["size"] as number
      if (typeof p["align"] === "string")
        style.textAlign = p["align"] as React.CSSProperties["textAlign"]
      if (typeof p["weight"] === "number" || typeof p["weight"] === "string")
        style.fontWeight = p["weight"] as React.CSSProperties["fontWeight"]
      return (
        <div className={cn("truncate", cls)} style={style}>
          {liveText ?? node.text}
        </div>
      )
    }
    case "value": {
      const b = node.bind["value"]
      const display = b !== undefined ? displayBinding(snapshot, b) : null
      const colorB = node.bind["color"]
      const liveColor =
        colorB !== undefined ? colorBinding(snapshot, colorB) : null
      return (
        <div className="flex h-full w-full items-baseline justify-between gap-2 overflow-hidden">
          {node.label && (
            <span className="truncate font-mono text-xs text-muted-foreground">
              {node.label}
            </span>
          )}
          <span
            className="font-mono text-[13px] text-foreground"
            style={liveColor ? { color: cssColor(liveColor) } : undefined}
          >
            {display ?? "—"}
            {node.unit && (
              <span className="ml-0.5 text-xs text-muted-foreground">
                {node.unit}
              </span>
            )}
          </span>
        </div>
      )
    }
    case "symbol": {
      const live: SymbolLive = {}
      for (const [k, b] of Object.entries(node.bind)) {
        live[k] = b === undefined ? null : resolveBinding(snapshot, b)
      }
      const colorB = node.bind["color"]
      const liveColor =
        colorB !== undefined ? colorBinding(snapshot, colorB) : null
      const valueBind = node.bind["value"]
      const history =
        node.symbol === "sparkline" && valueBind !== undefined
          ? windowSlice(
              historyRef.current.get(bindingVariable(valueBind)) ?? [],
              SPARKLINE_WINDOW_S,
            ).map((p) => p.v)
          : undefined
      return (
        <HmiSymbol
          symbol={node.symbol}
          w={node.w || 48}
          h={node.h || 48}
          live={live}
          props={node.props}
          liveColor={liveColor}
          history={history}
        />
      )
    }
    case "trend": {
      const series: TrendSeries[] = node.series.map((s, i) => {
        const buf = windowSlice(
          historyRef.current.get(s.variable) ?? [],
          node.window_s,
        )
        return {
          name: s.label ?? s.variable,
          points: buf.map((p) => ({ t: p.t, v: p.v, lo: p.lo, hi: p.hi })),
          color: TREND_COLORS[i % TREND_COLORS.length],
          binary: false,
        }
      })
      return (
        <div className="h-full w-full rounded border border-border bg-card/60 p-2">
          <TrendChart
            series={series}
            fit
            windowS={node.window_s}
          />
        </div>
      )
    }
    case "alarmbar":
      return <AlarmBar host={host} />
    case "alarmlist":
      return (
        <AlarmList host={host} maxRows={node.max_rows} operate={mode === "operate"} />
      )
    case "button": {
      // Optional state feedback: with `bind.on` the button lights while
      // the bound value is truthy (the indicator's lit treatment), so a
      // toggle shows the state it controls. `bind.enable` gates the
      // control: 0 (or no live data — same refusal posture as the write
      // resolver) renders it disabled, the context-dependent-controls
      // lever from RFC #33-C4.
      const onBind = node.bind["on"]
      const lit = onBind !== undefined && resolveOn(snapshot, onBind)
      const enBind = node.bind["enable"]
      const disabled =
        (enBind !== undefined && !resolveOn(snapshot, enBind)) ||
        (host.writesBlocked != null && nodeWrites(node))
      return (
        <button
          type="button"
          disabled={disabled}
          className={cn(
            "h-full w-full rounded-md border px-3 font-mono text-[12px]",
            lit
              ? "border-highlight bg-highlight/80 text-highlight-foreground hover:bg-highlight/70"
              : "border-border bg-card text-foreground hover:bg-accent/50",
            disabled && "cursor-not-allowed opacity-40 hover:bg-card",
          )}
        >
          {node.label}
        </button>
      )
    }
    case "input":
      return <InputNode node={node} snapshot={snapshot} onAction={onAction} />
    case "nav":
      return (
        <div className="flex h-full w-full items-center justify-center rounded-md border border-border bg-secondary px-3 font-mono text-[12px] text-foreground hover:bg-accent/50">
          {node.label} →
        </div>
      )
    case "shape": {
      // Style: props give the static look, `fill`/`stroke` binds (map
      // outputs) override it live — free-form P&ID pieces that carry
      // state color without a dedicated symbol.
      const p = node.props
      const fillB = node.bind["fill"]
      const strokeB = node.bind["stroke"]
      const liveFill = fillB !== undefined ? colorBinding(snapshot, fillB) : null
      const liveStroke =
        strokeB !== undefined ? colorBinding(snapshot, strokeB) : null
      const fill =
        liveFill ?? (typeof p["fill"] === "string" ? (p["fill"] as string) : null)
      const stroke =
        liveStroke ??
        (typeof p["stroke"] === "string" ? (p["stroke"] as string) : null)
      const strokeW =
        typeof p["stroke_width"] === "number" ? (p["stroke_width"] as number) : null
      const dash = typeof p["dash"] === "string" ? (p["dash"] as string) : null

      if (node.shape === "rect" || node.shape === "ellipse") {
        const rx = typeof p["rx"] === "number" ? (p["rx"] as number) : 4
        return (
          <div
            className={cn(
              "h-full w-full border",
              !stroke && "border-muted-foreground/40",
            )}
            style={{
              borderRadius: node.shape === "ellipse" ? "50%" : rx,
              background: fill ? cssColor(fill) : undefined,
              borderColor: stroke ? cssColor(stroke) : undefined,
              borderWidth: strokeW ?? undefined,
              borderStyle: dash ? "dashed" : undefined,
            }}
          />
        )
      }
      // line / polyline: draw through the points within the node box.
      const pts =
        node.points.length >= 2
          ? node.points
          : ([[0, 0], [node.w || 100, node.h || 0]] as [number, number][])
      const path = pts.map(([x, y], i) => `${i === 0 ? "M" : "L"}${x} ${y}`).join(" ")
      return (
        <svg
          width={node.w || 100}
          height={Math.max(node.h, 8)}
          className="overflow-visible"
        >
          <path
            d={path}
            className={cn("fill-none", !stroke && "stroke-muted-foreground/40")}
            style={stroke ? { stroke: cssColor(stroke) } : undefined}
            strokeWidth={strokeW ?? 3}
            strokeDasharray={dash ?? undefined}
            strokeLinecap="round"
          />
        </svg>
      )
    }
  }
}

const TREND_COLORS = ["var(--trend)", "var(--highlight)", "var(--warn)"]

function InputNode({
  node,
  snapshot,
  onAction,
}: {
  node: Extract<HmiNode, { type: "input" }>
  snapshot: ReturnType<typeof useLastSnapshot>
  onAction: (nodeId: string, action: HmiAction, value?: number) => void
}) {
  const host = useHmiHost()
  const [text, setText] = useState("")
  const commit = node.action["commit"]
  const b = node.bind["value"]
  // Display resolution (not resolveBinding): the placeholder echoes the
  // current value, which may legitimately be text (STRING var).
  const current = b !== undefined ? displayBinding(snapshot, b) : null
  const enBind = node.bind["enable"]
  const disabled =
    (enBind !== undefined && !resolveOn(snapshot, enBind)) ||
    (host.writesBlocked != null && nodeWrites(node))
  return (
    <div className="flex h-full w-full items-center gap-1.5 overflow-hidden">
      {node.label && (
        <span className="truncate font-mono text-xs text-muted-foreground">
          {node.label}
        </span>
      )}
      <input
        value={text}
        disabled={disabled}
        onChange={(e) => setText(e.target.value)}
        onClick={(e) => e.stopPropagation()}
        onPointerDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Enter" && commit && !disabled) {
            // Number("") is 0, not NaN — an empty field must not commit.
            const v = parseCommitText(text)
            if (v !== null) onAction(node.id, commit, v)
            setText("")
          }
        }}
        placeholder={current == null ? "—" : String(current)}
        className={cn(
          "h-full min-w-0 flex-1 rounded border border-input bg-background px-1.5 font-mono text-[12px] text-foreground outline-none focus:border-ring",
          disabled && "cursor-not-allowed opacity-40",
        )}
      />
      {node.unit && (
        <span className="font-mono text-xs text-muted-foreground">
          {node.unit}
        </span>
      )}
    </div>
  )
}

/** Fault + run-state strip. Calm when nothing is wrong (ISA-101: color
 *  only when it means something). Polls the host — the IDE answers from
 *  its runtime status, the edge panel from the runtime's /status. A
 *  failing poll counts toward an explicit COMMS LOST state instead of
 *  keeping the last green state over frozen values. */
const ALARM_TONES: Record<PanelTone, { bar: string; dot: string }> = {
  ok: {
    bar: "border-border bg-card/50 text-xs text-muted-foreground",
    dot: "bg-highlight",
  },
  idle: {
    bar: "border-border bg-card/50 text-xs text-muted-foreground",
    dot: "bg-muted-foreground/40",
  },
  warn: {
    bar: "border-warn/50 bg-warn/10 text-[12px] text-warn",
    dot: "bg-warn",
  },
  alert: {
    bar: "border-destructive/50 bg-destructive/10 text-[12px] text-destructive",
    dot: "bg-destructive",
  },
}

function AlarmBar({ host }: { host: HmiHost }) {
  const [state, setState] = useState<HmiRuntimeState | null>(null)
  const [failedPolls, setFailedPolls] = useState(0)
  useEffect(() => {
    let cancelled = false
    const tick = async () => {
      try {
        const s = await host.runtimeState()
        if (!cancelled) {
          setState(s)
          setFailedPolls(0)
          liveFeedStore.setScanPeriodMs(s.scanPeriodMs)
        }
      } catch {
        if (!cancelled) setFailedPolls((n) => n + 1)
      }
    }
    void tick()
    const id = setInterval(tick, 2000)
    return () => {
      cancelled = true
      clearInterval(id)
    }
  }, [host])
  const health = derivePanelHealth(state, failedPolls)
  const tone = ALARM_TONES[health.tone]
  return (
    <div
      className={cn(
        "flex h-full w-full items-center gap-2 rounded border px-3 font-mono",
        tone.bar,
      )}
    >
      <span className={cn("size-2 shrink-0 rounded-full", tone.dot)} />
      <span className="truncate">{health.text}</span>
    </div>
  )
}

/** Alarm summary table (the `alarmlist` node) — the operator-surface
 *  cousin of the Monitor's alarm section. Polls the host every 2 s; the
 *  backend pre-sorts standing-first. Calm until something stands
 *  (ISA-101): muted rows, severity colour only on standing ones, and ACK
 *  buttons only in Operate. `maxRows` caps visible rows (0 = 8). */
const CANVAS_ALARM_TONE: Record<AlarmTone, { chip: string; text: string }> = {
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

function AlarmList({
  host,
  maxRows,
  operate,
}: {
  host: HmiHost
  maxRows: number
  operate: boolean
}) {
  const [alarms, setAlarms] = useState<AlarmState[]>([])
  const [failed, setFailed] = useState(false)
  const [acking, setAcking] = useState<string | null>(null)
  const [ackError, setAckError] = useState<string | null>(null)
  const [showAll, setShowAll] = useState(false)
  useEffect(() => {
    let cancelled = false
    const tick = async () => {
      try {
        const a = await host.alarms()
        if (!cancelled) {
          setAlarms(a)
          setFailed(false)
        }
      } catch {
        if (!cancelled) setFailed(true)
      }
    }
    void tick()
    const id = setInterval(tick, 2000)
    return () => {
      cancelled = true
      clearInterval(id)
    }
  }, [host])

  const ack = async (id: string) => {
    if (!operate || acking) return
    setAcking(id)
    setAckError(null)
    try {
      const updated = await host.ackAlarm(id)
      setAlarms(previous => previous.map(alarm => alarm.id === id ? updated : alarm))
    } catch (error) { setAckError(`Acknowledge failed: ${String(error)}`) }
    finally { setAcking(null) }
  }

  const cap = maxRows > 0 ? maxRows : 8
  const standing = standingCount(alarms)
  const displayed = showAll ? alarms : alarms.filter(alarmStanding)
  const rows = displayed.slice(0, cap)
  const hidden = displayed.length - rows.length

  return (
    <div className="flex h-full w-full flex-col overflow-hidden rounded border border-border bg-card/60">
      <div className="flex shrink-0 items-center justify-between border-b border-border/70 px-2 py-1 text-xs font-medium text-muted-foreground">
        <span>Alarms{standing > 0 && <span className="ml-2 font-mono">{standing} standing</span>}</span>
        {alarms.length > standing && <Button variant="ghost" size="sm" aria-expanded={showAll} onClick={() => setShowAll(value => !value)}>{showAll ? "Standing only" : `Show all ${alarms.length}`}</Button>}
      </div>
      {(ackError || failed) && <div role="alert" className="shrink-0 border-b border-border px-2 py-1 text-xs text-destructive">{ackError || "Alarm status unavailable · showing last known values"}</div>}
      {rows.length === 0 ? (
        <div className="flex flex-1 items-center px-2 py-1 text-xs text-muted-foreground/60">
          {failed ? "Alarms unavailable" : "No standing alarms"}
        </div>
      ) : (
        <ul className="min-h-0 flex-1 divide-y divide-border/50 overflow-auto">
          {rows.map((a) => {
            const standing = alarmStanding(a)
            const tone = CANVAS_ALARM_TONE[severityTone(a.severity, standing)]
            return (
              <li
                key={a.id}
                className={cn(
                  "flex items-center gap-2 px-2 py-1 text-xs",
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
                <span className="shrink-0 font-mono text-xs tabular-nums text-muted-foreground">
                  {a.count === 0 ? "Never raised" : fmtAlarmClock(a.raised_at_us)}
                </span>
                {operate && !a.acked && (
                  <button
                    type="button"
                    onClick={() => void ack(a.id)}
                    disabled={acking !== null}
                    className="shrink-0 rounded border border-border bg-card px-1.5 py-px font-mono text-xs text-muted-foreground hover:text-foreground"
                    title="Acknowledge"
                  >
                    ack
                  </button>
                )}
              </li>
            )
          })}
        </ul>
      )}
      {hidden > 0 && (
        <div className="shrink-0 border-t border-border/60 px-2 py-0.5 text-xs text-muted-foreground/60">
          +{hidden} more
        </div>
      )}
    </div>
  )
}

function ConfirmCard({
  pending,
  onCancel,
  onConfirm,
}: {
  pending: PendingConfirm
  onCancel: () => void
  onConfirm: () => void
}) {
  const summary = confirmSummary(pending.action, pending.write)
  // The card interrupts the operator's flow, so their keyboard must
  // land IN it: focus starts on Cancel (the safe answer — Enter never
  // fires a plant write by accident) and Escape dismisses.
  const cancelRef = useRef<HTMLButtonElement | null>(null)
  useEffect(() => {
    cancelRef.current?.focus()
  }, [])
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={`Confirm action: ${summary}`}
      className="absolute inset-0 z-20 grid place-items-center bg-black/20"
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.stopPropagation()
          onCancel()
        }
      }}
    >
      <div className="w-[360px] max-w-[calc(100%_-_32px)] rounded border border-border bg-popover p-5 shadow-lg">
        <div className="text-xs font-medium text-muted-foreground">
          Confirm action
        </div>
        <div className="mt-2 font-mono text-[13px] text-foreground">
          {summary}
        </div>
        <div className="mt-4 flex justify-end gap-2">
          <button
            ref={cancelRef}
            type="button"
            onClick={onCancel}
            className="rounded-md border border-border bg-card px-3 py-1 text-[12px] text-muted-foreground hover:text-foreground"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={onConfirm}
            className="rounded-md bg-primary px-3 py-1 text-[12px] font-medium text-primary-foreground hover:bg-primary/90"
          >
            Confirm
          </button>
        </div>
      </div>
    </div>
  )
}

// ---- small pure helpers -------------------------------------------

export function findNode(root: HmiNode, id: string): HmiNode | null {
  if (root.id === id) return root
  if (root.type === "group") {
    for (const c of root.children) {
      const hit = findNode(c, id)
      if (hit) return hit
    }
  }
  return null
}

/** Same lookup as findNode, but accumulating group offsets into canvas
 *  coordinates (spawn overlays render at the root layer). */
function findNodeAbs(
  n: HmiNode,
  id: string,
  baseX: number,
  baseY: number,
): { node: HmiNode; x: number; y: number } | null {
  const ax = baseX + n.x
  const ay = baseY + n.y
  if (n.id === id) return { node: n, x: ax, y: ay }
  if (n.type === "group") {
    for (const c of n.children) {
      const hit = findNodeAbs(c, id, ax, ay)
      if (hit) return hit
    }
  }
  return null
}

/** Retention window a sparkline keeps for its bound variable. */
const SPARKLINE_WINDOW_S = 120

/** Per-variable retention window: the max `window_s` among the trend
 *  nodes referencing it, plus SPARKLINE_WINDOW_S for every sparkline's
 *  `value` bind (one shared buffer serves them all). */
function trendWindows(doc: HmiDoc): Map<string, number> {
  const out = new Map<string, number>()
  const bump = (v: string, w: number) =>
    out.set(v, Math.max(out.get(v) ?? 0, w))
  const walk = (n: HmiNode) => {
    if (n.type === "trend") {
      for (const s of n.series) bump(s.variable, n.window_s)
    }
    if (n.type === "symbol" && n.symbol === "sparkline") {
      const b = n.bind["value"]
      if (b !== undefined) bump(bindingVariable(b), SPARKLINE_WINDOW_S)
    }
    if (n.type === "group") n.children.forEach(walk)
  }
  walk(doc.root)
  return out
}
