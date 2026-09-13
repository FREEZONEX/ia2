import { useEffect, useState } from "react"
import { Bot } from "@/components/ui/icons"
import { apiFetch } from "@/lib/api"
import { shortcut } from "@/lib/platform"
import { activityLabel, agentActivityStore, useAgentActivity } from "@/state/agent-activity"

/** Agent work has one visible status area; it never frames or blocks the UI. */
export function AgentStatusBar() {
  const agent = useAgentActivity()
  const [error, setError] = useState<string | null>(null)
  const [pending, setPending] = useState(false)
  const active = agent.effectivelyActive
  const inSession = agent.sessionLabel != null
  const takeOver = async () => {
    if (pending) return
    setPending(true)
    setError(null)
    try {
      if (inSession) await apiFetch("/api/agent/session/end", { method: "POST", headers: { "Content-Type": "application/json" }, body: "{}" })
      else agentActivityStore.requestUserOverride()
    } catch (e) { setError(String(e)) } finally { setPending(false) }
  }
  useEffect(() => {
    if (!active) return
    const key = (e: KeyboardEvent) => { if (e.key === "." && (e.metaKey || e.ctrlKey)) { e.preventDefault(); void takeOver() } }
    window.addEventListener("keydown", key)
    return () => window.removeEventListener("keydown", key)
  }, [active, inSession, pending])
  if (!active) return null
  return (
    <div data-testid="agent-status-bar" className="relative z-[710] flex min-h-8 shrink-0 items-center gap-2 border-t border-border bg-selection px-4 text-xs text-selection-foreground">
      <Bot className="size-4 shrink-0" />
      <span className="shrink-0 font-medium">Agent working</span>
      <span className="min-w-0 flex-1 truncate">{activityLabel(agent)}</span>
      {error && <span role="alert" className="min-w-0 truncate text-destructive" title={error}>{error}</span>}
      <button type="button" disabled={pending} onClick={() => void takeOver()} title={`Take control back from the agent (${shortcut(".")})`} className="shrink-0 rounded px-2 py-1 font-medium hover:bg-button-highlight disabled:opacity-50">{pending ? "Ending session…" : "Take over"}</button>
    </div>
  )
}
