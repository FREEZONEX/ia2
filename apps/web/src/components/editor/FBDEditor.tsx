import { useEffect, useMemo, useState } from "react"
import { DiagnosticsBanner } from "@/components/editor/DiagnosticsBanner"
import { indexDiagnostics, type DiagnosticKey } from "@/components/editor/shared/diagnostics"
import { EditorHeader } from "@/components/editor/shared/EditorHeader"
import { ReadonlyVariablePanel } from "@/components/editor/shared/ReadonlyVariablePanel"
import {
  ParseErrorView,
  useProgramEditor,
  type ParseResult,
} from "@/components/editor/shared/useProgramEditor"
import {
  addBlock,
  connectWire,
  parseProgram,
  removeBlock,
  serializeProgram,
  setBlockPosition,
} from "@/lib/fbd-edit"
import { onlineBool } from "@/lib/online-vars"
import { cn } from "@/lib/utils"
import type { CheckDiagnostic } from "@/types/generated/CheckDiagnostic"
import type { FbdBlock } from "@/types/generated/FbdBlock"
import type { FbdLocation } from "@/types/generated/FbdLocation"
import type { FbdProgram } from "@/types/generated/FbdProgram"
import { type LivePins, type WireDrag } from "./fbd/types"
import { BLOCK_W, layoutBlocks } from "./fbd/layout"
import { Toolbar } from "./fbd/Toolbar"
import { FbdCanvas } from "./fbd/FbdCanvas"
import { BlockDetail } from "./fbd/BlockDetail"
import { OutputsBar } from "./fbd/OutputsBar"

/** Layout input while the source does not parse (never rendered). */
const EMPTY_FBD: FbdProgram = { name: "", pou_type: "program", variables: [], blocks: [], outputs: [] }

/** Controlled JSON editor. Owns document commits, selection, wire gestures
 *  and the program-scoped online lookup; canvas and panels render that state. */
export function FBDEditor({
  value,
  onChange,
  className,
  readOnly = false,
  path,
}: {
  value: string
  onChange: (next: string) => void
  className?: string
  readOnly?: boolean
  /** Store slug this buffer came from — keeps the project-aware check
   *  from double-counting the on-disk copy. */
  path?: string
}) {
  const { parsed, diagnostics, online, commit } =
    useProgramEditor<FbdProgram>({
      value,
      onChange,
      readOnly,
      path,
      language: "fbd",
      parse: safeParse,
      serialize: serializeProgram,
    })

  // Live values for online-mode wire coloring, scoped to this program.
  // Every FBD wire leaves a block output pin, and the runtime publishes FB
  // instances, not their pins — so reading `instance.pin` left every wire
  // looking dead. A pin bound to an output variable IS readable: the
  // binding copies it at the end of the scan, and the snapshot is taken
  // after that. Anything else is unknown (`null`), drawn without live state.
  const liveValues = useMemo<LivePins | null>(() => {
    if (!online || parsed.kind !== "ok") return null
    const outputs = parsed.program.outputs
    return {
      pin: (block, pin) => {
        const bound = outputs.find((o) => o.from_block === block.id && o.from_pin === pin)
        return onlineBool(online, bound ? bound.variable : `${block.instance}.${pin}`)
      },
    }
  }, [online, parsed])

  // ---- Selection ----
  const [selectedBlockId, setSelectedBlockId] = useState<string | null>(null)

  // ---- Wire-drag (port-to-port) state ----
  // Set when the user pointer-downs on a block's output dot. While
  // active, the canvas paints a preview line from the source pin
  // following the cursor; on pointerup we look for a target input
  // pin near the release point and either connect or cancel.
  const [wireDrag, setWireDrag] = useState<WireDrag | null>(null)

  // Reset selection if the source changed externally.
  useEffect(() => {
    setSelectedBlockId(null)
    setWireDrag(null)
  }, [value])

  // Every hook runs before the parse-error return below. Hooks after an
  // early return change the hook count whenever the source flips between
  // parseable and not — a fatal React error, and nothing above this pane
  // catches it, so the whole IDE unmounted.
  const layout = useMemo(
    () => layoutBlocks(parsed.kind === "ok" ? parsed.program : EMPTY_FBD),
    [parsed],
  )
  const diagIndex = useMemo(
    () => indexDiagnostics(diagnostics, fbdDiagnosticKey),
    [diagnostics],
  )

  if (parsed.kind === "error") {
    return (
      <ParseErrorView
        label="FBD"
        message={parsed.message}
        source={value}
        className={className}
      />
    )
  }

  const prog = parsed.program

  const selectedBlock = selectedBlockId
    ? prog.blocks.find((b) => b.id === selectedBlockId) ?? null
    : null

  return (
    <div className={cn("flex h-full min-h-0 flex-col", className)}>
      <EditorHeader
        name={prog.name}
        language="fbd"
        pouType={prog.pou_type}
        summary={
          <>
            {prog.blocks.length} block{prog.blocks.length === 1 ? "" : "s"} ·{" "}
            {prog.outputs.length} output binding
            {prog.outputs.length === 1 ? "" : "s"}
          </>
        }
      />
      <Toolbar
        readOnly={readOnly}
        onAdd={(fbType) => {
          // Drop new blocks somewhere visible — middle of the canvas.
          const cx = layout.width / 2
          const cy = layout.height / 2
          const { prog: next, blockId } = addBlock(prog, fbType, {
            x: Math.round(cx - BLOCK_W / 2),
            y: Math.round(cy - 30),
          })
          commit(next)
          setSelectedBlockId(blockId)
        }}
      />
      {diagnostics.length > 0 && (
        <DiagnosticsBanner
          diagnostics={diagnostics}
          formatLocation={(d) => describeLocation(d.fbd_location, prog.blocks)}
        />
      )}
      <div className="flex-1 overflow-auto bg-background">
        <ReadonlyVariablePanel
          variables={prog.variables}
          byVariable={diagIndex.byVariable}
        />
        <div className="p-4">
          <FbdCanvas
            prog={prog}
            layout={layout}
            liveValues={liveValues}
            diagIndex={diagIndex}
            selectedBlockId={selectedBlockId}
            readOnly={readOnly}
            wireDrag={wireDrag}
            onSelectBlock={setSelectedBlockId}
            onMoveBlock={(id, pos) => commit(setBlockPosition(prog, id, pos))}
            onStartWireDrag={setWireDrag}
            onUpdateWireDrag={(pos) =>
              setWireDrag((d) => (d ? { ...d, currentX: pos.x, currentY: pos.y } : d))
            }
            onEndWireDrag={(target) => {
              if (wireDrag && target) {
                commit(
                  connectWire(
                    prog,
                    target.blockId,
                    target.pin,
                    wireDrag.fromBlockId,
                    wireDrag.fromPin,
                  ),
                )
              }
              setWireDrag(null)
            }}
          />
        </div>
      </div>
      {selectedBlock && !readOnly && (
        <BlockDetail
          prog={prog}
          block={selectedBlock}
          onCommit={commit}
          onClose={() => setSelectedBlockId(null)}
          onDelete={() => {
            commit(removeBlock(prog, selectedBlock.id))
            setSelectedBlockId(null)
          }}
        />
      )}
      {!selectedBlock && prog.outputs.length > 0 && !readOnly && (
        <OutputsBar
          prog={prog}
          onCommit={commit}
          onClose={() => {}}
        />
      )}
    </div>
  )
}

/** Classify an FBD diagnostic for the shared `indexDiagnostics`: block
 *  errors bucket under the block id, variable errors under the variable
 *  name. `output` locations aren't surfaced inline, so they're dropped. */
function fbdDiagnosticKey(d: CheckDiagnostic): DiagnosticKey {
  const loc = d.fbd_location
  if (!loc) return null
  if (loc.kind === "block") return { element: loc.block_id }
  if (loc.kind === "variable") return { variable: loc.name }
  return null
}

function describeLocation(
  loc: FbdLocation | null | undefined,
  blocks: FbdBlock[],
): string {
  if (!loc) return "—"
  switch (loc.kind) {
    case "variable":
      return `var ${loc.name}`
    case "block": {
      // Show the instance name the user actually reads on the canvas
      // (e.g. `flt`), not the internal block_id (`b_lag`).
      const b = blocks.find((bb) => bb.id === loc.block_id)
      return b ? `block ${b.instance} (${b.fb_type})` : `block ${loc.block_id}`
    }
    case "output":
      return `output ${loc.variable}`
  }
}

function safeParse(source: string): ParseResult<FbdProgram> {
  try {
    const obj = JSON.parse(source)
    if (!obj || typeof obj !== "object" || !Array.isArray(obj.blocks)) {
      return { kind: "error", message: "missing `blocks` array" }
    }
    // Normalise optional-but-required-on-frontend fields so the rest
    // of this file can dereference `prog.outputs.length`, `block.inputs`,
    // etc. without `undefined.length` crashes. Older project files on
    // disk pre-date the "always serialize empty arrays" backend fix
    // and may omit these fields entirely.
    if (!Array.isArray(obj.outputs)) obj.outputs = []
    if (Array.isArray(obj.blocks)) {
      for (const b of obj.blocks) {
        if (b && typeof b === "object" && !Array.isArray(b.inputs)) {
          b.inputs = []
        }
      }
    }
    if (!Array.isArray(obj.variables)) obj.variables = []
    return { kind: "ok", program: parseProgram(JSON.stringify(obj)) }
  } catch (e) {
    return { kind: "error", message: String(e) }
  }
}
