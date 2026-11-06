import { Plus, RotateCw } from "@/components/ui/icons"
import { useEffect, useMemo, useState } from "react"
import { DiagnosticsBanner } from "@/components/editor/DiagnosticsBanner"
import { indexDiagnostics, type DiagnosticKey } from "@/components/editor/shared/diagnostics"
import { EditorHeader } from "@/components/editor/shared/EditorHeader"
import {
  ParseErrorView,
  useProgramEditor,
  type ParseResult,
} from "@/components/editor/shared/useProgramEditor"
import { Button } from "@/components/ui/button"
import { cn } from "@/lib/utils"
import type { CheckDiagnostic } from "@/types/generated/CheckDiagnostic"
import type { LdLocation } from "@/types/generated/LdLocation"
import {
  addRung,
  addVariable,
  parseProgram,
  removeVariable,
  serializeProgram,
  updateVariable,
  type LiveReadings,
} from "@/lib/ld-edit"
import { onlineBool, onlineNumber } from "@/lib/online-vars"
import type { LdProgram } from "@/types/generated/LdProgram"
import type { Selection } from "./ld/selection"
import { VariablePanel } from "./ld/VariablePanel"
import { RungEditor } from "./ld/RungEditor"

/** Controlled JSON editor. The document, diagnostics and online scope stay
 *  here; rung rendering and property drafts live in stable module components. */
export function LDEditor({
  value,
  onChange,
  readOnly = false,
  className,
  path,
}: {
  value: string
  onChange: (next: string) => void
  readOnly?: boolean
  className?: string
  /** Store slug this buffer came from — keeps the project-aware check
   *  from double-counting the on-disk copy. */
  path?: string
}) {
  const { parsed, diagnostics, online, commit } =
    useProgramEditor<LdProgram>({
      value,
      onChange,
      readOnly,
      path,
      language: "ld",
      parse: safeParse,
      serialize: serializeProgram,
    })
  const [sel, setSel] = useState<Selection>(null)

  // Live values for THIS program — drives online-mode coloring. `null`
  // when none are on the wire; a single unknown value reads as `null` too,
  // and renders like "no online data" rather than as a dead wire.
  const liveValues = useMemo<LiveReadings | null>(
    () =>
      online && {
        bool: (name) => onlineBool(online, name),
        number: (name) => onlineNumber(online, name),
      },
    [online],
  )

  // Drop selection when the source changes externally (revert, POU
  // switch). React's referential-equality check on `value` is what
  // saves us from infinite render loops on our own onChange.
  useEffect(() => {
    setSel(null)
  }, [value])

  // Every hook runs before the parse-error return below. Hooks after an
  // early return change the hook count whenever the source flips between
  // parseable and not — a fatal React error, and nothing above this pane
  // catches it, so the whole IDE unmounted.
  // Index diagnostics by ld_location so each renderer can look up
  // "are there errors on this element?" in O(1).
  const diagIndex = useMemo(
    () => indexDiagnostics(diagnostics, ldDiagnosticKey),
    [diagnostics],
  )

  if (parsed.kind === "error") {
    return (
      <ParseErrorView
        label="LD"
        message={parsed.message}
        source={value}
        className={className}
      />
    )
  }
  const prog = parsed.program

  return (
    <div className={cn("flex h-full min-h-0 flex-col", className)}>
      <EditorHeader
        name={prog.name}
        language="ld"
        pouType={prog.pou_type}
        summary={
          <>
            {prog.rungs.length} rung{prog.rungs.length === 1 ? "" : "s"} ·{" "}
            {prog.variables.length} var{prog.variables.length === 1 ? "" : "s"}
          </>
        }
      />

      {diagnostics.length > 0 && (
        <DiagnosticsBanner
          diagnostics={diagnostics}
          formatLocation={(d) => describeLocation(d.ld_location)}
          onJump={(d) => {
            // Jump to the rung containing the diagnostic. We don't dive
            // into the LD logic tree yet (that would require a node
            // path); landing on the rung is enough to show the user
            // where to look.
            const loc = d.ld_location
            if (!loc) return
            if (loc.kind === "rung") {
              const i = prog.rungs.findIndex((r) => r.id === loc.rung_id)
              if (i >= 0) setSel({ kind: "rung", rungIdx: i })
            } else if (loc.kind === "coil") {
              const i = prog.rungs.findIndex((r) => r.id === loc.rung_id)
              if (i >= 0)
                setSel({ kind: "coil", rungIdx: i, coilIdx: loc.coil_index })
            } else if (loc.kind === "fb_call") {
              const i = prog.rungs.findIndex((r) => r.id === loc.rung_id)
              if (i >= 0) setSel({ kind: "rung", rungIdx: i })
            } else if (loc.kind === "variable") {
              setSel({ kind: "variable", name: loc.name })
            }
          }}
        />
      )}

      <div className="flex-1 overflow-auto bg-background">
        <VariablePanel
          prog={prog}
          selection={sel}
          readOnly={readOnly}
          diagIndex={diagIndex}
          onSelect={(name) =>
            setSel({ kind: "variable", name })
          }
          onAdd={(v) => commit(addVariable(prog, v))}
          onRemove={(name) => {
            commit(removeVariable(prog, name))
            if (sel?.kind === "variable" && sel.name === name) setSel(null)
          }}
          onUpdate={(name, patch) => commit(updateVariable(prog, name, patch))}
        />

        <div className="space-y-3 px-4 py-3">
          {prog.rungs.map((rung, i) => (
            <RungEditor
              key={rung.id}
              prog={prog}
              rung={rung}
              rungIdx={i}
              totalRungs={prog.rungs.length}
              selection={sel}
              readOnly={readOnly}
              liveValues={liveValues}
              rungDiagnostics={diagIndex.byElement.get(rung.id) ?? []}
              onSelect={setSel}
              onCommit={commit}
            />
          ))}
          {!readOnly && (
            <div className="flex justify-center pt-2">
              <Button
                size="sm"
                variant="outline"
                onClick={() => commit(addRung(prog))}
              >
                <Plus className="mr-1 size-3" />
                Add rung
              </Button>
            </div>
          )}
        </div>
      </div>
    </div>
  )
}

/** Classify an LD diagnostic for the shared `indexDiagnostics`: rung,
 *  coil and fb_call errors all bucket under the owning rung id; variable
 *  errors under the variable name; everything else is dropped. */
function ldDiagnosticKey(d: CheckDiagnostic): DiagnosticKey {
  const loc = d.ld_location
  if (!loc) return null
  if (loc.kind === "rung" || loc.kind === "coil" || loc.kind === "fb_call") {
    return { element: loc.rung_id }
  }
  if (loc.kind === "variable") return { variable: loc.name }
  return null
}

/** Format an LdLocation for human display. Used in the banner. */
function describeLocation(loc: LdLocation | null | undefined): string {
  if (!loc) return "—"
  switch (loc.kind) {
    case "rung":
      return `rung ${loc.rung_id}`
    case "coil":
      return `rung ${loc.rung_id} · coil ${loc.coil_index}`
    case "fb_call":
      return `rung ${loc.rung_id} · ${loc.instance}(…)`
    case "variable":
      return `var ${loc.name}`
  }
}

function safeParse(source: string): ParseResult<LdProgram> {
  try {
    const program = parseProgram(source)
    if (!program || typeof program !== "object") {
      return { kind: "error", message: "top-level value is not an object" }
    }
    if (!Array.isArray(program.rungs)) {
      return { kind: "error", message: "missing `rungs` array" }
    }
    return { kind: "ok", program }
  } catch (e) {
    return { kind: "error", message: String(e) }
  }
}

/** Mark `RotateCw` as referenced so its tree-shake doesn't whine —
 *  we ship the icon for a future "rotate / swap" action. */
void RotateCw
