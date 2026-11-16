import { Plus, X } from "@/components/ui/icons"
import { useState } from "react"
import type { DiagnosticIndex } from "@/components/editor/shared/diagnostics"
import { Input } from "@/components/ui/input"
import { cn } from "@/lib/utils"
import type { LdProgram } from "@/types/generated/LdProgram"
import type { LdVarSection } from "@/types/generated/LdVarSection"
import type { Selection } from "./selection"

// (DiagnosticsBanner moved to its own module — same component is
//  shared with FBDEditor and SFCEditor. See ./DiagnosticsBanner.tsx.
//  The top-of-pane Header is now the shared <EditorHeader>.)


export function VariablePanel({
  prog,
  selection,
  readOnly,
  diagIndex,
  onSelect,
  onAdd,
  onRemove,
  onUpdate,
}: {
  prog: LdProgram
  selection: Selection
  readOnly: boolean
  diagIndex: DiagnosticIndex
  onSelect: (name: string) => void
  onAdd: (v: LdProgram["variables"][number]) => void
  onRemove: (name: string) => void
  onUpdate: (name: string, patch: Partial<LdProgram["variables"][number]>) => void
}) {
  const groups: Array<{ label: string; section: LdVarSection }> = [
    { label: "VAR_INPUT", section: "input" },
    { label: "VAR_OUTPUT", section: "output" },
    { label: "VAR", section: "internal" },
  ]
  const [drafting, setDrafting] = useState<{
    section: LdVarSection
    name: string
    type: string
  } | null>(null)

  return (
    <div className="grid grid-cols-3 gap-3 border-b border-border bg-muted/10 px-4 py-2 text-[13px]">
      {groups.map((g) => {
        const vs = prog.variables.filter((v) => v.section === g.section)
        return (
          <div key={g.section}>
            <div className="mb-1 flex items-center justify-between font-mono text-xs text-muted-foreground">
              <span>{g.label}</span>
              {!readOnly && (
                <button
                  type="button"
                  className="rounded p-0.5 hover:bg-accent/40 hover:text-foreground"
                  onClick={() =>
                    setDrafting({ section: g.section, name: "", type: "BOOL" })
                  }
                  title={`Add ${g.label}`}
                >
                  <Plus className="size-3" />
                </button>
              )}
            </div>
            <ul className="space-y-0.5">
              {vs.map((v) => (
                <li
                  key={v.name}
                  className={cn(
                    "group flex items-center gap-1 rounded px-1 font-mono cursor-pointer",
                    selection?.kind === "variable" && selection.name === v.name
                      ? "bg-highlight/15"
                      : "hover:bg-accent/30",
                    // Red outline when a diagnostic mentions this var.
                    diagIndex.byVariable.has(v.name) &&
                      "ring-1 ring-destructive/60",
                  )}
                  onClick={() => onSelect(v.name)}
                  title={
                    diagIndex.byVariable.get(v.name)?.[0]?.message ?? undefined
                  }
                >
                  <span className="text-foreground">{v.name}</span>
                  <span className="text-muted-foreground">{v.type}</span>
                  {v.init !== null && v.init !== undefined && (
                    <span className="text-muted-foreground">:= {v.init}</span>
                  )}
                  {!readOnly && (
                    <button
                      type="button"
                      className="ml-auto rounded p-0.5 opacity-0 transition-opacity hover:bg-destructive/15 hover:text-destructive group-hover:opacity-100"
                      onClick={(e) => {
                        e.stopPropagation()
                        onRemove(v.name)
                      }}
                      title={`Remove ${v.name}`}
                    >
                      <X className="size-3" />
                    </button>
                  )}
                </li>
              ))}
              {drafting && drafting.section === g.section && (
                <li className="flex gap-1 rounded bg-highlight/10 px-1 py-0.5">
                  <Input
                    autoFocus
                    placeholder="name"
                    value={drafting.name}
                    onChange={(e) =>
                      setDrafting({ ...drafting, name: e.target.value })
                    }
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && drafting.name.trim()) {
                        onAdd({
                          name: drafting.name.trim(),
                          type: drafting.type,
                          section: g.section,
                          init: null,
                        })
                        setDrafting(null)
                      } else if (e.key === "Escape") {
                        setDrafting(null)
                      }
                    }}
                    className="h-6 w-20 font-mono text-[13px]"
                  />
                  <Input
                    placeholder="type"
                    value={drafting.type}
                    onChange={(e) =>
                      setDrafting({ ...drafting, type: e.target.value })
                    }
                    className="h-6 w-16 font-mono text-[13px]"
                  />
                </li>
              )}
            </ul>
            {/* Editable details for the selected variable in this section. */}
            {selection?.kind === "variable" &&
              vs.some((v) => v.name === selection.name) && (
                <VariableDetail
                  prog={prog}
                  name={selection.name}
                  onUpdate={onUpdate}
                />
              )}
          </div>
        )
      })}
    </div>
  )
}

function VariableDetail({
  prog,
  name,
  onUpdate,
}: {
  prog: LdProgram
  name: string
  onUpdate: (name: string, patch: Partial<LdProgram["variables"][number]>) => void
}) {
  const v = prog.variables.find((x) => x.name === name)
  if (!v) return null
  return (
    <div className="mt-2 space-y-1 rounded border border-highlight/30 bg-highlight/5 p-1.5 text-xs">
      <Row label="type">
        <Input
          value={v.type}
          onChange={(e) => onUpdate(name, { type: e.target.value })}
          className="h-6 font-mono"
        />
      </Row>
      <Row label="init">
        <Input
          placeholder="(none)"
          value={v.init ?? ""}
          onChange={(e) =>
            onUpdate(name, {
              init: e.target.value.trim() ? e.target.value : null,
            })
          }
          className="h-6 font-mono"
        />
      </Row>
    </div>
  )
}

function Row({
  label,
  children,
}: {
  label: string
  children: React.ReactNode
}) {
  return (
    <div className="flex items-center gap-1.5">
      <span className="w-9 shrink-0 font-mono text-xs text-muted-foreground">
        {label}
      </span>
      <div className="flex-1">{children}</div>
    </div>
  )
}
