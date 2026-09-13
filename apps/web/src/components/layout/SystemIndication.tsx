import { useState } from "react"
import { Square } from "@/components/ui/icons"
import { Button } from "@/components/ui/button"
import { useRuntime } from "@/state/runtime"
import { useConnected } from "@/state/live-feed"
import { cn } from "@/lib/utils"

/** Connection and stop remain visible when the editor or Monitor is hidden. */
export function SystemIndication() {
  const { isRunning, running, attached, isDirty, currentPou, selectPou, stop } = useRuntime()
  const connected = useConnected()
  const [stopping, setStopping] = useState(false)
  const label = !connected ? "Server unreachable" : isRunning ? "Controller active" : "Controller stopped"
  const program = running?.kind === "isolated" ? running.program : running?.kind === "scheduled" ? "Task schedule" : running?.kind === "remote" ? running.edge : null
  return (
    <footer aria-label="Controller status" className="flex h-9 min-h-9 shrink-0 items-center gap-3 border-t border-border bg-secondary px-4 text-xs">
      <span className={cn("size-1.5 shrink-0 rounded-full", !connected ? "bg-destructive" : isRunning ? "bg-highlight" : "bg-muted-foreground")} />
      <span className={cn("shrink-0", !connected && "text-destructive")}>{label}</span>
      {program && <span className="min-w-0 truncate font-mono text-muted-foreground">{program}</span>}
      <span className="hidden text-muted-foreground sm:inline">{attached ? "Remote runtime" : "Local runtime"}</span>
      <div className="flex-1" />
      {isDirty && currentPou && <button type="button" onClick={() => void selectPou(currentPou.path)} className="min-w-0 truncate text-warn hover:underline" title={`Unsaved changes in ${currentPou.path}`}>Unsaved changes</button>}
      {isRunning && <Button variant="ghost" size="xs" aria-label="Stop controller" disabled={stopping} onClick={() => { setStopping(true); void stop().finally(() => setStopping(false)) }} className="text-destructive hover:bg-destructive/10 hover:text-destructive"><Square className="size-3.5" />{stopping ? "Stopping…" : "Stop"}</Button>}
    </footer>
  )
}
