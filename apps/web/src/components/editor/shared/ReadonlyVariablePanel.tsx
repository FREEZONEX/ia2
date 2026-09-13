/**
 * Read-only three-column variable panel — shared by the FBD and SFC
 * editors, which carried a byte-identical copy each.
 *
 * Lists the POU's VAR_INPUT / VAR_OUTPUT / VAR declarations with a red
 * ring on any variable a diagnostic mentions. (LDEditor keeps its own
 * richer, inline-editable panel — this is only the read-only variant.)
 */

import { cn } from "@/lib/utils"
import type { CheckDiagnostic } from "@/types/generated/CheckDiagnostic"
import type { LdVarSection } from "@/types/generated/LdVarSection"
import type { LdVariable } from "@/types/generated/LdVariable"

export function ReadonlyVariablePanel({
  variables,
  byVariable,
}: {
  variables: LdVariable[]
  /** Diagnostics keyed by variable name (from `indexDiagnostics`). */
  byVariable: Map<string, CheckDiagnostic[]>
}) {
  const groups: Array<{ label: string; section: LdVarSection }> = [
    { label: "VAR_INPUT", section: "input" },
    { label: "VAR_OUTPUT", section: "output" },
    { label: "VAR", section: "internal" },
  ]
  return (
    <div className="grid grid-cols-3 gap-3 border-b border-border bg-muted/10 px-4 py-2 text-[13px]">
      {groups.map((g) => {
        const vs = variables.filter((v) => v.section === g.section)
        return (
          <div key={g.section}>
            <div className="mb-1 font-mono text-xs text-muted-foreground">
              {g.label}
            </div>
            <ul className="space-y-0.5">
              {vs.length === 0 && (
                <li className="text-muted-foreground italic">—</li>
              )}
              {vs.map((v) => (
                <li
                  key={v.name}
                  className={cn(
                    "flex items-center gap-1 rounded px-1 font-mono",
                    byVariable.has(v.name) && "ring-1 ring-destructive/60",
                  )}
                  title={byVariable.get(v.name)?.[0]?.message ?? undefined}
                >
                  <span className="text-foreground">{v.name}</span>
                  <span className="text-muted-foreground">{v.type}</span>
                  {v.init !== null && v.init !== undefined && (
                    <span className="text-muted-foreground">:= {v.init}</span>
                  )}
                </li>
              ))}
            </ul>
          </div>
        )
      })}
    </div>
  )
}
