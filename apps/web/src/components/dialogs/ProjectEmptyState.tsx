import { useState } from "react"
import { FolderPlus, FolderOpen } from "@/components/ui/icons"
import { Button } from "@/components/ui/button"
import { WindowTitleBar } from "@/components/layout/WindowTitleBar"
import { useRuntime } from "@/state/runtime"
import { NewProjectDialog } from "./NewProjectDialog"
import { OpenProjectDialog } from "./OpenProjectDialog"

export function ProjectEmptyState() {
  const { availableProjects, openProject } = useRuntime()
  const [opening, setOpening] = useState<string | null>(null)
  return (
    <div className="flex h-dvh flex-col bg-background text-foreground">
      <WindowTitleBar />
      <main className="min-h-0 flex-1 overflow-auto">
        <div className="mx-auto w-full max-w-4xl px-8 py-12">
          <div className="flex flex-wrap items-start justify-between gap-6">
            <div className="max-w-md"><h1 className="text-xl font-medium">Your projects</h1><p className="mt-2 text-[13px] leading-relaxed text-muted-foreground">Open a project to edit programs, configure devices and run your controller.</p></div>
            <div className="flex items-center gap-2"><OpenProjectDialog trigger={<Button variant="outline"><FolderOpen className="size-4" />Open project</Button>} /><NewProjectDialog trigger={<Button><FolderPlus className="size-4" />New project</Button>} /></div>
          </div>
          <section className="mt-10" aria-label="Recent projects">
            <h2 className="mb-3 text-xs font-medium text-muted-foreground">Recent projects</h2>
            {availableProjects.length > 0 ? <ul className="divide-y divide-border border-y border-border">{availableProjects.map((p) => <li key={p.path}><button type="button" disabled={opening !== null} onClick={() => { setOpening(p.path); void openProject(p.path).finally(() => setOpening(null)) }} className="flex w-full items-center gap-4 px-2 py-4 text-left hover:bg-secondary disabled:opacity-50"><FolderOpen className="size-5 shrink-0 text-muted-foreground" /><span className="min-w-0 flex-1"><span className="block truncate text-sm font-medium">{opening === p.path ? `Opening ${p.name}…` : p.name}</span><span className="mt-1 block truncate font-mono text-xs text-muted-foreground" title={p.path}>{p.path}</span></span></button></li>)}</ul> : <div className="border-y border-border py-8 text-[13px] text-muted-foreground">No recent projects. Open an existing folder or create a project.</div>}
          </section>
        </div>
      </main>
    </div>
  )
}
