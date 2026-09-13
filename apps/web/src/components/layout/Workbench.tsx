import { X } from "@/components/ui/icons"
import { useEffect, useState } from "react"
import { Group, Panel, Separator, usePanelRef, type Layout } from "react-resizable-panels"
import { ProjectEmptyState } from "@/components/dialogs/ProjectEmptyState"
import { RuntimeProvider, useRuntime } from "@/state/runtime"
import { AgentStatusBar } from "./AgentStatusBar"
import { DevicePane } from "./DevicePane"
import { EdgePane } from "./EdgePane"
import { HmiPane } from "./HmiPane"
import { IoMapPane } from "./IoMapPane"
import { MonitorPane } from "./MonitorPane"
import { ProgramPane } from "./ProgramPane"
import { ProjectPane } from "./ProjectPane"
import { QuickOpen } from "./QuickOpen"
import { SystemIndication } from "./SystemIndication"
import { TasksPane } from "./TasksPane"
import { WindowTitleBar } from "./WindowTitleBar"

export function Workbench() {
  return <RuntimeProvider><Shell /><GlobalErrorToast /></RuntimeProvider>
}

function GlobalErrorToast() {
  const { error, clearError } = useRuntime()
  useEffect(() => {
    if (!error) return
    const t = window.setTimeout(clearError, 8000)
    return () => window.clearTimeout(t)
  }, [error, clearError])
  if (!error) return null
  return (
    <div
      role="alert"
      // z-[100] keeps the toast above modal dialog overlays (Radix uses
      // z-50) — a create dialog stays open on failure, so its error must
      // float over the dimming overlay, not behind it.
      className="fixed bottom-4 right-4 z-[100] flex max-w-md items-start gap-2 rounded-md border border-destructive/30 bg-popover px-4 py-3 text-[13px] text-destructive shadow-lg"
    >
      <span className="min-w-0 flex-1 whitespace-pre-wrap break-words">
        {error}
      </span>
      <button
        type="button"
        onClick={clearError}
        aria-label="Dismiss error"
        className="-mt-0.5 -mr-1 shrink-0 rounded p-0.5 text-destructive/70 hover:bg-destructive/10 hover:text-destructive"
      >
        <X className="size-3.5" />
      </button>
    </div>
  )
}


function usePersistedLayout(key: string, fallback: Layout): [Layout, (layout: Layout) => void] {
  const [layout, setLayout] = useState<Layout>(() => {
    try {
      const parsed = JSON.parse(localStorage.getItem(key) ?? "null")
      const keys = Object.keys(fallback)
      if (parsed && typeof parsed === "object" && Object.keys(parsed).length === keys.length && keys.every((id) => typeof parsed[id] === "number" && Number.isFinite(parsed[id]) && parsed[id] >= 0 && parsed[id] <= 100) && Math.abs(keys.reduce((sum, id) => sum + parsed[id], 0) - 100) < 0.1) return parsed
    } catch { /* An invalid saved layout must not hide the workspace. */ }
    return fallback
  })
  useEffect(() => { try { localStorage.setItem(key, JSON.stringify(layout)) } catch { /* optional preference */ } }, [key, layout])
  return [layout, setLayout]
}

function Shell() {
  const { project, projectLoading, view } = useRuntime()
  const [hLayout, setHLayout] = usePersistedLayout("ia2.shell.h.v4", { project: 21, center: 79 })
  const [vLayout, setVLayout] = usePersistedLayout("ia2.shell.v.v4", { editor: 65, monitor: 35 })
  const projectPanel = usePanelRef()
  const monitorPanel = usePanelRef()
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false)
  const [monitorCollapsed, setMonitorCollapsed] = useState(false)
  const [quickOpen, setQuickOpen] = useState(false)
  const toggleSidebar = () => { const p = projectPanel.current; if (p?.isCollapsed()) p.expand(); else p?.collapse() }
  const toggleMonitor = () => { const p = monitorPanel.current; if (p?.isCollapsed()) p.expand(); else p?.collapse() }
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && ["p", "k"].includes(e.key.toLowerCase())) { e.preventDefault(); setQuickOpen(true) }
    }
    window.addEventListener("keydown", key)
    return () => window.removeEventListener("keydown", key)
  }, [])
  useEffect(() => {
    if (!project) return
    const query = matchMedia("(max-width: 800px)")
    const fit = () => { if (query.matches) projectPanel.current?.collapse() }
    const frame = requestAnimationFrame(fit)
    query.addEventListener("change", fit)
    return () => { cancelAnimationFrame(frame); query.removeEventListener("change", fit) }
  }, [project?.name, projectPanel])
  if (projectLoading) return <div className="flex h-dvh flex-col bg-background"><WindowTitleBar /><div role="status" className="px-6 py-8 text-sm text-muted-foreground">Loading workspace…</div></div>
  if (!project) return <ProjectEmptyState />
  const center = view === "device" ? <DevicePane /> : view === "edge" ? <EdgePane /> : view === "hmi" ? <HmiPane /> : view === "iomap" ? <IoMapPane /> : view === "tasks" ? <TasksPane /> : <ProgramPane />
  return (
    <div data-testid="workbench" className="flex h-dvh w-full min-w-0 flex-col overflow-hidden bg-background text-foreground">
      <WindowTitleBar onSearch={() => setQuickOpen(true)} onToggleSidebar={toggleSidebar} sidebarCollapsed={sidebarCollapsed} onToggleMonitor={toggleMonitor} monitorCollapsed={monitorCollapsed} />
      <Group orientation="horizontal" defaultLayout={hLayout} onLayoutChange={setHLayout} className="min-h-0 min-w-0 flex-1">
        <Panel id="project" panelRef={projectPanel} minSize="208px" maxSize="340px" collapsible collapsedSize="0px" onResize={(size) => setSidebarCollapsed(size.inPixels < 40)}><div className="h-full" inert={sidebarCollapsed} aria-hidden={sidebarCollapsed}><ProjectPane /></div></Panel>
        <Gutter orientation="vertical" />
        <Panel id="center" minSize="360px">
          <Group orientation="vertical" defaultLayout={vLayout} onLayoutChange={setVLayout} className="h-full min-h-0 min-w-0">
            <Panel id="editor" minSize="160px">{center}</Panel>
            <Gutter orientation="horizontal" />
            <Panel id="monitor" panelRef={monitorPanel} minSize="210px" maxSize="65%" collapsible collapsedSize="40px" onResize={(size) => setMonitorCollapsed(size.inPixels < 60)}><MonitorPane collapsed={monitorCollapsed} onToggleCollapse={toggleMonitor} /></Panel>
          </Group>
        </Panel>
      </Group>
      <SystemIndication />
      <AgentStatusBar />
      <QuickOpen open={quickOpen} onClose={() => setQuickOpen(false)} />
    </div>
  )
}

function Gutter({ orientation }: { orientation: "vertical" | "horizontal" }) {
  const vertical = orientation === "vertical"
  return <Separator aria-label={vertical ? "Resize project explorer" : "Resize monitor"} className={`group relative shrink-0 bg-border/70 transition-colors hover:bg-ring focus-visible:bg-ring ${vertical ? "w-px cursor-col-resize" : "h-px cursor-row-resize"}`}><span aria-hidden className={`absolute z-10 ${vertical ? "-left-1 top-0 h-full w-2" : "-top-1 left-0 h-2 w-full"}`} /></Separator>
}
