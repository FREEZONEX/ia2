import { useEffect, useState } from "react"
import { Check, ChevronDown, FolderOpen, Moon, PanelLeftOpen, Plus, Search, Settings, Sun, MonitorDot } from "@/components/ui/icons"
import { NewProjectDialog } from "@/components/dialogs/NewProjectDialog"
import { OpenProjectDialog } from "@/components/dialogs/OpenProjectDialog"
import { Button } from "@/components/ui/button"
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuLabel, DropdownMenuSeparator, DropdownMenuTrigger } from "@/components/ui/dropdown-menu"
import { fetchOpenProjects, fetchProjects } from "@/lib/api"
import { setTheme, useDarkMode } from "@/lib/dark-mode"
import { shortcut } from "@/lib/platform"
import { useRuntime } from "@/state/runtime"
import type { OpenProjectInfo } from "@/types/generated/OpenProjectInfo"
import type { ProjectListing } from "@/types/generated/ProjectListing"

export function Ia2Mark() {
  return <svg aria-hidden viewBox="0 0 24 24" className="size-6 shrink-0"><rect width="24" height="24" rx="4" fill="#050b14" /><path d="M4 15h5V7h6v10h5" fill="none" stroke="var(--agent)" strokeWidth="2" strokeLinejoin="round" /></svg>
}

/** App navigation is opaque on every platform. The OS owns its title bar. */
export function WindowTitleBar({ onSearch, onToggleSidebar, sidebarCollapsed, onToggleMonitor, monitorCollapsed }: {
  onSearch?: () => void
  onToggleSidebar?: () => void
  sidebarCollapsed?: boolean
  onToggleMonitor?: () => void
  monitorCollapsed?: boolean
}) {
  const { project } = useRuntime()
  const theme = useDarkMode()
  return (
    <header data-testid="workspace-header" className="flex h-12 min-h-12 shrink-0 items-center gap-3 border-b border-border bg-background px-3 text-foreground">
      {project && <Button variant="ghost" size="icon-sm" onClick={onToggleSidebar} aria-label={sidebarCollapsed ? "Show project explorer" : "Hide project explorer"} aria-pressed={!sidebarCollapsed}><PanelLeftOpen className="size-4" /></Button>}
      <div className="flex shrink-0 items-center gap-2"><Ia2Mark /><span className="text-[15px] font-medium">IA2</span></div>
      <span aria-hidden className="h-4 w-px bg-border" />
      <ProjectPicker />
      <div className="flex-1" />
      {project && <button type="button" onClick={onSearch} title={`Search (${shortcut("P")})`} aria-label="Search project" className="flex h-8 w-8 min-[1000px]:w-64 max-w-[24vw] items-center gap-2 rounded border border-border bg-secondary px-2 min-[1000px]:px-2.5 text-xs text-muted-foreground hover:border-input hover:text-foreground"><Search className="size-4 shrink-0" /><span className="hidden min-w-0 flex-1 truncate text-left min-[1000px]:block">Find in project</span><kbd className="ml-auto hidden shrink-0 font-sans min-[1000px]:block">{shortcut("P")}</kbd></button>}
      {project && <Button variant="ghost" size="sm" onClick={onToggleMonitor} aria-label={monitorCollapsed ? "Expand monitor" : "Collapse monitor"} aria-pressed={!monitorCollapsed}><MonitorDot className="size-4" /><span className="hidden min-[1000px]:inline">Monitor</span></Button>}
      <DropdownMenu>
        <DropdownMenuTrigger asChild><Button variant="ghost" size="icon-sm" aria-label="Appearance" title="Appearance"><Settings className="size-4" /></Button></DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-44">
          <DropdownMenuLabel>Appearance</DropdownMenuLabel>
          <DropdownMenuItem onSelect={() => setTheme("light")}><Sun className="size-4" />Light{theme === "light" && <Check className="ml-auto size-4" />}</DropdownMenuItem>
          <DropdownMenuItem onSelect={() => setTheme("dark")}><Moon className="size-4" />Dark{theme === "dark" && <Check className="ml-auto size-4" />}</DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </header>
  )
}

function ProjectPicker() {
  const { project, openProject, closeProject } = useRuntime()
  const [open, setOpen] = useState(false)
  const [projectDialog, setProjectDialog] = useState<"new" | "open" | null>(null)
  const [projects, setProjects] = useState<OpenProjectInfo[] | null>(null)
  const [listings, setListings] = useState<ProjectListing[]>([])
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  useEffect(() => {
    if (!open) return
    let active = true
    setError(null)
    void Promise.all([fetchOpenProjects(), fetchProjects()]).then(([a, b]) => {
      if (active) { setProjects(a.projects); setListings(b) }
    }).catch((e) => { if (active) setError(String(e)) })
    return () => { active = false }
  }, [open])

  const select = async (name: string, path: string) => {
    if (busy) return
    if (name === project?.name) { setOpen(false); return }
    setBusy(name)
    try { if (await openProject(path)) setOpen(false) } finally { setBusy(null) }
  }
  const projectUrl = (name: string) => {
    const url = new URL(window.location.href)
    url.searchParams.set("project", name)
    return url.toString()
  }
  const other = listings.filter((p) => !(projects ?? []).some((o) => o.path === p.path))
  return (
    <>
    <DropdownMenu open={open} onOpenChange={setOpen}>
      <DropdownMenuTrigger asChild>
        <button type="button" title="Switch project / open another window" className="flex h-8 min-w-0 max-w-[34vw] items-center gap-2 rounded px-2 text-[13px] font-medium hover:bg-accent"><span className="truncate">{busy ? `Opening ${busy}…` : project?.name ?? "Projects"}</span><ChevronDown className="size-4 shrink-0 text-muted-foreground" /></button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-80 max-w-[calc(100vw-2rem)]">
        <DropdownMenuLabel>Open projects</DropdownMenuLabel>
        {error && <div role="alert" className="px-2 py-2 text-xs text-destructive">{error}</div>}
        {!projects && !error && <div className="px-2 py-2 text-xs text-muted-foreground">Loading projects…</div>}
        {projects?.map((p) => <div key={p.name} className="flex items-center gap-1">
          <DropdownMenuItem disabled={busy !== null} onSelect={(e) => { e.preventDefault(); void select(p.name, p.path) }} className="min-w-0 flex-1" title={p.path}><FolderOpen className="size-4 shrink-0" /><span className="truncate">{p.name}</span>{p.name === project?.name && <Check className="ml-auto size-4 shrink-0 text-selection-foreground" />}</DropdownMenuItem>
          <a href={projectUrl(p.name)} target="_blank" rel="noopener noreferrer" title="Open in a new window" aria-label={`Open ${p.name} in a new window`} className="grid size-8 shrink-0 place-items-center rounded text-muted-foreground hover:bg-accent hover:text-foreground"><Plus className="size-4" /></a>
        </div>)}
        {other.length > 0 && <><DropdownMenuSeparator /><DropdownMenuLabel>Recent projects</DropdownMenuLabel>{other.slice(0, 6).map((p) => <DropdownMenuItem key={p.path} disabled={busy !== null} title={p.path} onSelect={(e) => { e.preventDefault(); void select(p.name, p.path) }}><FolderOpen className="size-4" /><span className="truncate">{p.name}</span></DropdownMenuItem>)}</>}
        <DropdownMenuSeparator />
        <DropdownMenuItem onSelect={() => setProjectDialog("new")}>New project…</DropdownMenuItem>
        <DropdownMenuItem onSelect={() => setProjectDialog("open")}>Open project…</DropdownMenuItem>
        {project && <DropdownMenuItem onSelect={(e) => { e.preventDefault(); setOpen(false); void closeProject() }}>Close project</DropdownMenuItem>}
      </DropdownMenuContent>
    </DropdownMenu>
    <NewProjectDialog open={projectDialog === "new"} onOpenChange={(value) => setProjectDialog(value ? "new" : null)} />
    <OpenProjectDialog open={projectDialog === "open"} onOpenChange={(value) => setProjectDialog(value ? "open" : null)} />
    </>
  )
}
