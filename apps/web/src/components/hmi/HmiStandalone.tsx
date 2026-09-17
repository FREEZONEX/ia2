/**
 * Standalone operator panel — the hmi.html entry's whole UI.
 *
 * Served by `ia2-runtime --static-dir` on the edge box: `/hmi` lists the
 * screens deployed with the project, `/hmi/<screen>` renders one live.
 * Everything talks to the runtime's own surface on this same origin:
 * `/api/hmi` (read-only documents), `/events` (bare VarSnapshot SSE),
 * `/write` (confirmed actions), `/status` (fault strip + project name).
 *
 * The canvas itself is the exact component the IDE uses — only the
 * HmiHost implementation differs, so operate-mode behaviour (confirm
 * flows, bindings, trends) can't drift between the two surfaces.
 */

import { useCallback, useEffect, useMemo, useState } from "react"
import { Moon, Sun } from "@/components/ui/icons"
import { EmptyState } from "@/components/ui/empty-state"
import { Button } from "@/components/ui/button"

import { HmiCanvas } from "@/components/hmi/HmiCanvas"
import {
  HmiHostProvider,
  type HmiHost,
  type HmiRuntimeState,
} from "@/components/hmi/host"
import {
  COMMS_LOST_POLLS,
  derivePanelHealth,
  edgeRuntimeState,
  type EdgeStatus,
  type PanelTone,
} from "@/components/hmi/panel-health"
import { panelFetch } from "@/components/hmi/panel-fetch"
import { useThemeToggle } from "@/lib/dark-mode"
import { cn } from "@/lib/utils"
import { encodeForWrite, undeliveredFrom } from "@/lib/write-encoding"
import { liveFeedStore, useConnected } from "@/state/live-feed"
import type { AlarmState } from "@/types/generated/AlarmState"
import type { HistoryResponse } from "@/types/generated/HistoryResponse"
import type { HmiListEntry } from "@/types/generated/HmiListEntry"

async function jget<T>(url: string): Promise<T> {
  const res = await panelFetch(url)
  if (!res.ok) {
    throw new Error(`${res.status}: ${(await res.text()) || url}`)
  }
  return (await res.json()) as T
}

/** `/hmi/<screen>` when served by the runtime; `hmi.html?screen=<slug>`
 *  when opened straight off a vite dev server. */
function slugFromLocation(): string | null {
  const m = window.location.pathname.match(/^\/hmi\/(.+)$/)
  if (m) return decodeURIComponent(m[1])
  return new URLSearchParams(window.location.search).get("screen")
}

function urlFor(slug: string | null): string {
  if (window.location.pathname.startsWith("/hmi")) {
    return slug ? `/hmi/${encodeURIComponent(slug)}` : "/hmi"
  }
  const q = slug ? `?screen=${encodeURIComponent(slug)}` : ""
  return `${window.location.pathname}${q}`
}

export function HmiStandalone() {
  const [slug, setSlug] = useState<string | null>(slugFromLocation)
  const [screens, setScreens] = useState<HmiListEntry[] | null>(null)
  const [listError, setListError] = useState<string | null>(null)
  const [project, setProject] = useState("")
  const connected = useConnected()
  const { theme, toggle } = useThemeToggle()

  const navigate = useCallback((target: string | null) => {
    window.history.pushState(null, "", urlFor(target))
    setSlug(target)
  }, [])

  useEffect(() => {
    const onPop = () => setSlug(slugFromLocation())
    window.addEventListener("popstate", onPop)
    return () => window.removeEventListener("popstate", onPop)
  }, [])

  useEffect(() => {
    void jget<HmiListEntry[]>("/api/hmi")
      .then((rows) => {
        setScreens(rows)
        setListError(null)
      })
      .catch((e) => setListError(String(e)))
  }, [])

  // Shell-level /status poll: the header must distinguish running /
  // paused / fault / comms-lost on its own — the alarmbar is per-screen
  // and a screen may not include one. The same poll carries the standing
  // alarm count for the compact header chip.
  const [edgeState, setEdgeState] = useState<HmiRuntimeState | null>(null)
  const [failedPolls, setFailedPolls] = useState(0)
  const [alarmsStanding, setAlarmsStanding] = useState(0)
  useEffect(() => {
    let cancelled = false
    const tick = async () => {
      const generation = liveFeedStore.getGeneration()
      try {
        const s = await jget<EdgeStatus>("/status")
        if (cancelled || generation !== liveFeedStore.getGeneration()) return
        setProject(s.project ?? "")
        const state = edgeRuntimeState(s)
        setEdgeState(state)
        // This poll runs for the whole session, screen or no screen, so it is
        // the panel's reliable source for the write-freshness budget.
        liveFeedStore.setScanPeriodMs(state.scanPeriodMs, generation)
        setAlarmsStanding(s.alarms_standing ?? 0)
        setFailedPolls(0)
      } catch {
        if (!cancelled && generation === liveFeedStore.getGeneration()) setFailedPolls((n) => n + 1)
      }
    }
    void tick()
    const id = setInterval(tick, 2000)
    return () => {
      cancelled = true
      clearInterval(id)
    }
  }, [])

  // The runtime's /events is a bare VarSnapshot stream (no AppEvent
  // wrapper) — feed it straight into the same live store the canvas
  // already reads. EventSource reconnects on its own.
  useEffect(() => {
    const es = new EventSource("/events")
    es.onopen = () => liveFeedStore.setConnected(true)
    es.onerror = () => liveFeedStore.setConnected(false)
    es.onmessage = (e) => {
      try {
        liveFeedStore.setSnapshot(JSON.parse(e.data))
      } catch {
        /* malformed tick — skip */
      }
    }
    return () => {
      es.close()
      liveFeedStore.setConnected(false)
    }
  }, [])

  const host = useMemo<HmiHost>(
    () => ({
      fetchDoc: (p) => jget(`/api/hmi/${encodeURIComponent(p)}`),
      // no saveDoc: screens are edited in the IDE and arrive via deploy
      write: async (name, value, typeName, pulseMs) => {
        const body: Record<string, unknown> = {
          name,
          value: encodeForWrite(value, typeName),
        }
        if (pulseMs != null) body["pulse_ms"] = pulseMs
        // panelFetch stamps X-IA2-Origin: hmi — the edge audit ring
        // attributes this write to the operator panel, not an
        // anonymous script (ADR-0002).
        const res = await panelFetch("/write", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body),
        })
        if (!res.ok) {
          throw new Error(`${res.status}: ${await res.text()}`)
        }
        // Applied, but the runtime says this variable's own device cannot
        // carry it out right now. Same contract as the IDE host: report the
        // caveat, never retry.
        return undeliveredFrom(await res.json().catch(() => null))
      },
      nav: (target) => navigate(target),
      // mode + device_health ride along so the alarmbar can tell
      // paused / device-down apart from a healthy green run.
      runtimeState: async () =>
        edgeRuntimeState(await jget<EdgeStatus>("/status")),
      // Runtime-side surfaces on this same origin (root-relative), same
      // shapes as the IDE's /api/runtime/* endpoints.
      history: (vars, stepMs) =>
        jget<HistoryResponse>(
          `/history?vars=${encodeURIComponent(vars.join(","))}&from_us=0&to_us=0&step_ms=${stepMs}`,
        ),
      alarms: () => jget<AlarmState[]>("/alarms"),
      ackAlarm: async (id) => {
        // Same attribution header as /write, for parity. Acks are not
        // in the edge audit ring today (it covers writes/forces); the
        // header keeps the panel honest wherever origins ARE recorded.
        const res = await panelFetch(`/alarms/${encodeURIComponent(id)}/ack`, {
          method: "POST",
        })
        if (!res.ok) {
          throw new Error(`${res.status}: ${await res.text()}`)
        }
        return (await res.json()) as AlarmState
      },
    }),
    [navigate],
  )

  const title =
    (slug && screens?.find((s) => s.path === slug)?.title) || slug || ""

  const health = derivePanelHealth(edgeState, failedPolls)
  // No chip until a poll succeeded or comms are confirmed lost — a
  // one-frame STOPPED flash on load would be noise, not information.
  const showBadge =
    health.kind !== "running" &&
    (edgeState !== null || failedPolls >= COMMS_LOST_POLLS)

  return (
    <div className="flex h-dvh flex-col bg-background text-foreground">
      <header className="flex min-h-12 shrink-0 flex-wrap items-center justify-between gap-3 border-b border-border bg-background px-4 py-2">
        <div className="flex min-w-0 items-baseline gap-2">
          {slug ? (
            <button
              type="button"
              onClick={() => navigate(null)}
              className="shrink-0 font-mono text-xs text-muted-foreground hover:text-foreground"
            >
              ← Screens
            </button>
          ) : (
            <span className="font-mono text-xs text-muted-foreground">
              HMI
            </span>
          )}
          <span className="truncate text-[13px] font-medium">
            {slug ? title : project}
          </span>
          {slug && project && (
            <span className="truncate font-mono text-xs text-muted-foreground">
              {project}
            </span>
          )}
        </div>
        <div className="flex items-center gap-2">
          {alarmsStanding > 0 && (
            <button
              type="button"
              onClick={() =>
                document
                  .querySelector('[data-hmi-type="alarmlist"]')
                  ?.scrollIntoView({ behavior: "smooth", block: "center" })
              }
              title="Standing alarms — jump to the alarm list if this screen has one"
              className="rounded bg-warn/15 px-1.5 py-0.5 font-mono text-xs text-warn"
            >
              {alarmsStanding} {alarmsStanding === 1 ? "alarm" : "alarms"}
            </button>
          )}
          {showBadge && (
            <span
              className={cn(
                "rounded px-1.5 py-0.5 font-mono text-xs",
                HEADER_TONES[health.tone],
              )}
            >
              {health.badge}
            </span>
          )}
          <span
            title={connected ? "Live values connected" : "No live connection"}
            className={
              "size-2 rounded-full " +
              (connected ? "bg-highlight" : "bg-muted-foreground/40")
            }
          />
          <button
            type="button"
            onClick={toggle}
            title="Toggle theme"
            className="grid size-8 place-items-center rounded text-muted-foreground hover:bg-accent/50 hover:text-foreground"
          >
            {theme === "dark" ? <Sun className="size-3.5" /> : <Moon className="size-3.5" />}
          </button>
        </div>
      </header>

      {slug ? (
        <div className="min-h-0 flex-1">
          <HmiHostProvider value={host}>
            <HmiCanvas
              path={slug}
              mode="operate"
              selected={null}
              onSelect={() => {}}
            />
          </HmiHostProvider>
        </div>
      ) : (
        <ScreenIndex screens={screens} error={listError} onOpen={navigate} />
      )}
    </div>
  )
}

/** Header chip styling per health tone; `ok` never renders a chip. */
const HEADER_TONES: Record<PanelTone, string> = {
  ok: "",
  idle: "bg-muted/60 text-muted-foreground",
  warn: "bg-warn/15 text-warn",
  alert: "bg-destructive/15 text-destructive",
}

function ScreenIndex({
  screens,
  error,
  onOpen,
}: {
  screens: HmiListEntry[] | null
  error: string | null
  onOpen: (slug: string) => void
}) {
  if (error) {
    return (
      <EmptyState title="Screens unavailable" description={error} actions={<Button variant="outline" onClick={() => window.location.reload()}>Retry</Button>} />
    )
  }
  if (!screens) {
    return (
      <EmptyState title="Loading screens…" />
    )
  }
  if (screens.length === 0) {
    return (
      <EmptyState title="No screens deployed" description="Add an HMI screen to the project and deploy it to this runtime." />
    )
  }
  return (
    <div className="flex-1 overflow-auto p-6">
      <div className="mx-auto flex max-w-4xl flex-col divide-y divide-border">
        {screens.map((s) => (
          <button
            key={s.path}
            type="button"
            onClick={() => onOpen(s.path)}
            className="flex min-h-16 items-center justify-between gap-4 rounded px-4 py-3 text-left hover:bg-selection"
          >
            <div className="flex items-center gap-3">
              <span className="font-mono text-[12px] text-muted-foreground">
                {s.path}
              </span>
              <span className="rounded bg-muted/60 px-1.5 py-0.5 font-mono text-xs text-muted-foreground">
                L{s.level}
              </span>
            </div>
            <div className="text-[14px] font-medium text-foreground">
              {s.title}
            </div>
          </button>
        ))}
      </div>
    </div>
  )
}
