import { ShieldCheck } from "@/components/ui/icons"
import { useAgentActivity } from "@/state/agent-activity"

/** AgentStatusBar is the single agent activity surface. */
export function TakeoverOverlay() { return null }

export function UserControlIndicator() {
  const agent = useAgentActivity()
  if (!agent.overrideActive) return null
  return <div role="status" className="pointer-events-none fixed right-4 bottom-12 z-[650] flex items-center gap-2 rounded border border-border bg-popover px-3 py-2 text-xs text-highlight shadow-sm"><ShieldCheck className="size-4" />You're in control</div>
}
