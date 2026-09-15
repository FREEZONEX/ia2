/**
 * HMI view: toolbar (title, ISA level, operate/arrange switch, check) +
 * the live canvas, plus the human editing surface — Arrange mode shows the
 * palette strip and an editable inspector (geometry, props, bindings,
 * actions), Operate mode a read-only one. Both agents (via `cs hmi op`)
 * and humans edit through the same /ops endpoint, so either side's
 * changes land live on the other's canvas.
 */

import { useCallback, useEffect, useMemo, useState } from "react"
import { Hand, MousePointerClick, ShieldAlert } from "@/components/ui/icons"

import { findNode, HmiCanvas, type CanvasMode } from "@/components/hmi/HmiCanvas"
import { HmiInspector, HmiPalette } from "@/components/hmi/HmiEditorPanel"
import { HmiHostProvider, type HmiHost } from "@/components/hmi/host"
import {
  ackRuntimeAlarm,
  checkHmi,
  fetchHmi,
  fetchProjectVariables,
  fetchRuntimeAlarms,
  fetchRuntimeHistory,
  fetchRuntimeStatus,
  saveHmi,
  writeVariable,
} from "@/lib/api"
import { PaneHeader } from "@/components/ui/pane-header"
import { EmptyState } from "@/components/ui/empty-state"
import { Button } from "@/components/ui/button"
import { cn } from "@/lib/utils"
import { useHmiMutation } from "@/state/hmi-live"
import { useRuntime } from "@/state/runtime"
import type { HmiDoc } from "@/types/generated/HmiDoc"
import type { HmiIssue } from "@/types/generated/HmiIssue"

export function HmiPane() {
  const { currentHmi, selectHmi } = useRuntime()
  const [mode, setMode] = useState<CanvasMode>("operate")
  const [selected, setSelected] = useState<string | null>(null)
  const [doc, setDoc] = useState<HmiDoc | null>(null)
  const [issues, setIssues] = useState<HmiIssue[]>([])
  const [variables, setVariables] = useState<string[]>([])
  const [checkError, setCheckError] = useState<string | null>(null)
  const mutation = useHmiMutation()

  // The IDE-side host: documents and writes go through the project
  // server; nav switches the workbench's active screen. The standalone
  // edge panel provides its own implementation of this seam.
  const host = useMemo<HmiHost>(
    () => ({
      fetchDoc: fetchHmi,
      saveDoc: saveHmi,
      write: writeVariable,
      nav: (target) => void selectHmi(target),
      runtimeState: async () => {
        const s = await fetchRuntimeStatus()
        // mode rides along so a paused scan loop doesn't show as a
        // green "Running" in the canvas alarmbar. unhealthyDevices does
        // the same job for a dead fieldbus: the scan loop keeps running
        // and every value keeps updating, so without this the IDE's
        // alarmbar shows a calm green plant while the bus is down —
        // the standalone edge panel has always shown it.
        return {
          running: s.running,
          alarm: s.watchdog_tripped ? "Watchdog tripped — outputs are locked" : s.last_error ?? null,
          mode: s.mode?.kind,
          scanPeriodMs: s.scan_period_ms,
          unhealthyDevices: s.device_health
            .filter((d) => !d.healthy)
            .map((d) => d.name),
        }
      },
      history: (vars, stepMs) => fetchRuntimeHistory(vars, { stepMs }),
      alarms: fetchRuntimeAlarms,
      ackAlarm: ackRuntimeAlarm,
    }),
    [selectHmi],
  )

  useEffect(() => {
    void fetchProjectVariables()
      .then((r) => setVariables([...new Set(r.variables.map((v) => v.name))]))
      .catch(() => {})
  }, [currentHmi])

  const refreshIssues = useCallback(async () => {
    if (!currentHmi) return
    try {
      setIssues(await checkHmi(currentHmi))
      setCheckError(null)
    } catch (error) { setCheckError(`Screen validation unavailable: ${String(error)}`) }
  }, [currentHmi])

  useEffect(() => {
    setSelected(null)
    setDoc(null)
    void refreshIssues()
  }, [refreshIssues])

  useEffect(() => {
    if (mutation && mutation.path === currentHmi) void refreshIssues()
  }, [mutation, currentHmi, refreshIssues])

  if (!currentHmi) {
    return (
      <EmptyState title="No screen selected" description="Choose a screen in the HMI section to operate it or edit its layout." />
    )
  }

  const errors = issues.filter((i) => i.severity === "error").length
  const warnings = issues.length - errors
  const selectedNode = doc && selected ? findNode(doc.root, selected) : null

  return (
    <main className="flex h-full min-h-0 min-w-0 flex-col">
      <PaneHeader title={doc?.title || currentHmi} meta={<>
        <span className="font-mono">{currentHmi}</span>
        {doc && <span>Level {doc.level}</span>}
        {issues.length > 0 && <span title={issues.map(issue => issue.message).join("\n")} className={cn("flex items-center gap-1", errors > 0 ? "text-destructive" : "text-warn")}><ShieldAlert className="size-4" />{errors > 0 ? `${errors} errors` : `${warnings} warnings`}</span>}
      </>} actions={<ModeSwitch mode={mode} onChange={next => { setMode(next); setSelected(null) }} />} />
      {checkError && <div role="alert" className="border-b border-border px-4 py-2 text-xs text-destructive">{checkError}</div>}

      {mode === "arrange" && <HmiPalette path={currentHmi} doc={doc} />}
      <div className="flex min-h-0 flex-1">
        <div className="min-w-0 flex-1">
          <HmiHostProvider value={host}>
            <HmiCanvas
              path={currentHmi}
              mode={mode}
              selected={selected}
              onSelect={setSelected}
              onDocLoaded={setDoc}
            />
          </HmiHostProvider>
        </div>
        {selectedNode && mode === "arrange" && (
          <HmiInspector
            path={currentHmi}
            node={selectedNode}
            variables={variables}
            onClose={() => setSelected(null)}
          />
        )}
      </div>
    </main>
  )
}

function ModeSwitch({
  mode,
  onChange,
}: {
  mode: CanvasMode
  onChange: (m: CanvasMode) => void
}) {
  const btn = (m: CanvasMode, icon: React.ReactNode, label: string) => (
    <Button
      size="sm"
      variant={mode === m ? "highlight" : "ghost"}
      aria-pressed={mode === m}
      type="button"
      onClick={() => onChange(m)}
      title={
        m === "operate"
          ? "Operate: actions are live; layout is locked"
          : "Arrange: drag elements (snap to grid); actions are inert"
      }
    >
      {icon}
      {label}
    </Button>
  )
  return (
    <div className="flex items-center gap-1" role="group" aria-label="HMI mode">
      {btn("operate", <MousePointerClick className="size-3" />, "Operate")}
      {btn("arrange", <Hand className="size-3" />, "Arrange")}
    </div>
  )
}
