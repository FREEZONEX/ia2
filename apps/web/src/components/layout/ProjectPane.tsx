import { useRuntime } from "@/state/runtime"
import { ProjectTree } from "./ProjectTree"

export function ProjectPane() {
  const { project } = useRuntime()
  return (
    <aside aria-label="Project explorer" className="flex h-full min-w-0 flex-col bg-sidebar">
      <div className="flex h-12 shrink-0 items-center px-4 text-xs font-medium text-muted-foreground" title={project?.path}>Project explorer</div>
      <div className="min-h-0 flex-1 overflow-y-auto overflow-x-hidden pb-3"><ProjectTree /></div>
    </aside>
  )
}
