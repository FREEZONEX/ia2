import {
  ArrowDown,
  ArrowUp,
  Plus,
  RotateCw,
  Trash2,
  X,
} from "@/components/ui/icons"
import { useEffect, useMemo, useRef, useState } from "react"

import { DiagnosticsBanner } from "@/components/editor/DiagnosticsBanner"
import { ActionBtn, DetailLabel, Separator } from "@/components/editor/shared/DetailBar"
import {
  indexDiagnostics,
  type DiagnosticIndex,
  type DiagnosticKey,
} from "@/components/editor/shared/diagnostics"
import { EditorHeader } from "@/components/editor/shared/EditorHeader"
import { powerClass } from "@/components/editor/shared/power"
import {
  ParseErrorView,
  useProgramEditor,
  type ParseResult,
} from "@/components/editor/shared/useProgramEditor"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { cn } from "@/lib/utils"
import type { CheckDiagnostic } from "@/types/generated/CheckDiagnostic"
import type { LdLocation } from "@/types/generated/LdLocation"
import {
  addCoil,
  addInParallel,
  addInSeries,
  addRung,
  addVariable,
  comparatorSymbol,
  deleteCoil,
  deleteNode,
  deleteRung,
  evaluateNode,
  moveRung,
  newCompare,
  newContact,
  newFbCall,
  operandText,
  parseProgram,
  removeVariable,
  serializeProgram,
  setCoilKind,
  setCoilVar,
  setContactVar,
  setFbInputValue,
  setFbType,
  setRungLabel,
  toggleNegated,
  updateCompare,
  updateFbCall,
  updateVariable,
  type LiveReadings,
  type NodePath,
} from "@/lib/ld-edit"
import { onlineBool, onlineNumber } from "@/lib/online-vars"
import { fbByType, fbInputs, fbOutputs, fbPinHint, groupedFbs } from "@/lib/ld-fbs"
import type { LdCoilKind } from "@/types/generated/LdCoilKind"
import type { LdNode } from "@/types/generated/LdNode"
import type { LdProgram } from "@/types/generated/LdProgram"
import type { LdVarSection } from "@/types/generated/LdVarSection"

// =================================================================
//   Controlled LD editor.
//
//   Props:
//     value     — pretty-JSON source string (what's on disk).
//     onChange  — called whenever the user mutates the program; the
//                 string passed is the new pretty-JSON to save.
//
//   The editor parses `value` into an `LdProgram` on every render. The
//   JSON IS the source of truth — we never hold "the program" in
//   internal state separately, so external edits (revert, agent push,
//   git pull) round-trip without diverging.
//
//   Selection lives in local state (ephemeral; lost on POU switch /
//   page reload — fine, matches editor convention).
// =================================================================

type Selection =
  | { kind: "node"; rungIdx: number; path: NodePath }
  | { kind: "coil"; rungIdx: number; coilIdx: number }
  | { kind: "rung"; rungIdx: number }
  | { kind: "variable"; name: string }
  | null

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

// =================================================================
//   Diagnostics indexing + banner
// =================================================================

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

// (DiagnosticsBanner moved to its own module — same component is
//  shared with FBDEditor and SFCEditor. See ./DiagnosticsBanner.tsx.
//  The top-of-pane Header is now the shared <EditorHeader>.)

// =================================================================
//   Variable panel — three columns, inline-editable
// =================================================================

function VariablePanel({
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

// =================================================================
//   Rung — toolbar + SVG canvas + inline detail overlays
// =================================================================

const CELL_W = 90
const CELL_H = 44
const RAIL_PAD = 16
const COIL_W = 100
const TOP_PAD = 12

interface LayoutBox {
  cols: number
  rows: number
}

function measure(node: LdNode): LayoutBox {
  switch (node.op) {
    case "contact":
    case "const":
      return { cols: 1, rows: 1 }
    case "compare":
      // Compare blocks render as a rectangle wide enough to fit
      // "left CMP right" (e.g. `temperature < 50.0`). 2 cells fits
      // most typical labels comfortably; longer labels just clip.
      return { cols: 2, rows: 1 }
    case "fb_call":
      // FB call blocks (TON, CTU, R_TRIG, ...) render the same width
      // as a Compare block — the rectangle shows `instance.outputPin`
      // with a small `[TYPE]` tag above. Individual pin bindings
      // live in the NodeDetail editor, not in the rung itself, so we
      // don't grow vertically for input/output pin lists. This keeps
      // mixed contact + FB rungs visually compact and consistent.
      return { cols: 2, rows: 1 }
    case "not":
      return measure(node.arg)
    case "and": {
      if (node.args.length === 0) return { cols: 1, rows: 1 }
      let cols = 0
      let rows = 1
      for (const a of node.args) {
        const m = measure(a)
        cols += m.cols
        rows = Math.max(rows, m.rows)
      }
      return { cols, rows }
    }
    case "or": {
      if (node.args.length === 0) return { cols: 1, rows: 1 }
      let cols = 1
      let rows = 0
      for (const a of node.args) {
        const m = measure(a)
        cols = Math.max(cols, m.cols)
        rows += m.rows
      }
      return { cols, rows }
    }
  }
}

function RungEditor({
  prog,
  rung,
  rungIdx,
  totalRungs,
  selection,
  readOnly,
  liveValues,
  rungDiagnostics,
  onSelect,
  onCommit,
}: {
  prog: LdProgram
  rung: LdProgram["rungs"][number]
  rungIdx: number
  totalRungs: number
  selection: Selection
  readOnly: boolean
  liveValues: LiveReadings | null
  rungDiagnostics: CheckDiagnostic[]
  onSelect: (s: Selection) => void
  onCommit: (next: LdProgram) => void
}) {
  // We only render rung-level error decoration today (banner up top
  // has the per-element detail). Per-coil and per-FB-call colouring
  // can come later by slicing `rungDiagnostics` further.
  const hasRungError = rungDiagnostics.length > 0
  const firstErrorMsg = rungDiagnostics[0]?.message ?? null
  // Network output power: when running, this is `evaluateNode(root,
  // values)` — drives wire colouring AND coil "energised" state.
  // Null when not running; renderers treat null as "no online data,
  // render uncoloured static".
  const networkPowered = liveValues
    ? evaluateNode(rung.logic, liveValues)
    : null
  const layoutBox = measure(rung.logic)
  const cols = layoutBox.cols
  const networkRows = layoutBox.rows
  // Total rows = max(network height, coil count). A 3-coil rung needs
  // 3 rows of vertical space for the coil stack even if the network
  // is a single contact, and vice versa. This is the canonical IEC
  // 61131-3 LD layout — multiple coils on one rung are PARALLEL loads
  // driven by the same network output, drawn as vertical branches at
  // the right edge of the rung.
  const coilCount = rung.coils.length
  const totalRows = Math.max(networkRows, coilCount, 1)
  // Network is centered vertically within the rung's total height; the
  // coil column sits to the right with one row per coil, also centered.
  const networkY = TOP_PAD + ((totalRows - networkRows) * CELL_H) / 2
  const coilStackY =
    TOP_PAD + ((totalRows - Math.max(coilCount, 1)) * CELL_H) / 2
  // Single output column (coils stacked vertically) sits right after
  // the network. With multiple coils we still only need ONE column
  // width — they don't extend horizontally.
  const width = RAIL_PAD * 2 + cols * CELL_W + COIL_W + 24
  const height = totalRows * CELL_H + TOP_PAD * 2

  // The "network exit" y is the middle of the network's vertical band.
  const networkOutY =
    networkY + (networkRows * CELL_H) / 2
  // x where the coil column begins (right edge of the contact network).
  const coilColX = RAIL_PAD + cols * CELL_W
  // x where each coil glyph's left paren sits; centered in COIL_W.
  const coilGlyphX = coilColX + (COIL_W - 36) / 2
  // Y of each individual coil (one row per coil, centered in its row).
  const coilYs = rung.coils.map(
    (_, ci) => coilStackY + ci * CELL_H + CELL_H / 2,
  )

  const isSelected =
    selection?.kind === "rung" && selection.rungIdx === rungIdx

  return (
    <div
      className={cn(
        "rounded-md border border-border bg-card",
        isSelected && "ring-1 ring-highlight",
        // Heavier outline on any rung-tagged diagnostic so users can
        // glance at the page and spot which rungs need attention. The
        // banner up top has the actual messages.
        hasRungError && "border-destructive/70",
      )}
      title={firstErrorMsg ?? undefined}
    >
      <RungToolbar
        rung={rung}
        rungIdx={rungIdx}
        totalRungs={totalRungs}
        readOnly={readOnly}
        hasError={hasRungError}
        onSelectRung={() => onSelect({ kind: "rung", rungIdx })}
        onLabelChange={(label) => onCommit(setRungLabel(prog, rungIdx, label))}
        onDelete={() => {
          onCommit(deleteRung(prog, rungIdx))
          onSelect(null)
        }}
        onMoveUp={() => onCommit(moveRung(prog, rungIdx, rungIdx - 1))}
        onMoveDown={() => onCommit(moveRung(prog, rungIdx, rungIdx + 1))}
        onAddCoil={() => {
          const coilVar =
            prog.variables.find((v) => v.section === "output")?.name ??
            prog.variables.find((v) => v.type === "BOOL")?.name ??
            "out"
          onCommit(addCoil(prog, rungIdx, coilVar))
        }}
      />
      <svg
        viewBox={`0 0 ${width} ${height}`}
        width="100%"
        className="block"
        style={{ height }}
      >
        <line
          x1={RAIL_PAD}
          y1={0}
          x2={RAIL_PAD}
          y2={height}
          className="stroke-foreground"
          strokeWidth={1}
          vectorEffect="non-scaling-stroke"
        />
        <line
          x1={width - RAIL_PAD}
          y1={0}
          x2={width - RAIL_PAD}
          y2={height}
          className="stroke-foreground"
          strokeWidth={1}
          vectorEffect="non-scaling-stroke"
        />

        <RenderNode
          node={rung.logic}
          path={[]}
          x={RAIL_PAD}
          y={networkY}
          cols={cols}
          rows={networkRows}
          rungIdx={rungIdx}
          selection={selection}
          readOnly={readOnly}
          liveValues={liveValues}
          onSelect={onSelect}
          onCommit={(transform) => onCommit(transform(prog))}
        />

        {/* Coil column: vertical stack of parallel loads driven by the
            network output. Standard IEC 61131-3 LD: one horizontal wire
            from network out to a vertical merge column, then one
            horizontal wire from each coil to the right rail.
            In online mode, the wires + merge column light up FX Green
            when the network output is conducting (= coil energised). */}
        {coilCount >= 1 && (
          <>
            {/* Vertical merge wire only when more than one coil — single
                coil rungs use a flat horizontal connection (classical
                single-coil look). */}
            {coilCount > 1 && (
              <line
                x1={coilColX}
                y1={Math.min(networkOutY, coilYs[0])}
                x2={coilColX}
                y2={Math.max(networkOutY, coilYs[coilYs.length - 1])}
                className={powerClass(networkPowered)}
                strokeWidth={1}
                vectorEffect="non-scaling-stroke"
              />
            )}
            {rung.coils.map((coil, ci) => {
              const cy = coilYs[ci]
              const sel =
                selection?.kind === "coil" &&
                selection.rungIdx === rungIdx &&
                selection.coilIdx === ci
              // A coil's effective drive state — set / reset latches
              // hold their var across scans, so the "energised" visual
              // for those should reflect the var's value, not the
              // network's instantaneous output. Standard coils mirror
              // the network output. Either way we colour the wire by
              // the NETWORK output (that's what's actually carrying
              // power right now), and the COIL by its var's live value.
              const coilEnergised = liveValues ? liveValues.bool(coil.var) : null
              return (
                <g key={ci}>
                  {/* horizontal wire: merge column → coil glyph */}
                  <line
                    x1={coilColX}
                    y1={cy}
                    x2={coilGlyphX}
                    y2={cy}
                    className={powerClass(networkPowered)}
                    strokeWidth={1}
                    vectorEffect="non-scaling-stroke"
                  />
                  {/* horizontal wire: coil glyph → right rail */}
                  <line
                    x1={coilGlyphX + 36}
                    y1={cy}
                    x2={width - RAIL_PAD}
                    y2={cy}
                    className={powerClass(networkPowered)}
                    strokeWidth={1}
                    vectorEffect="non-scaling-stroke"
                  />
                  <CoilGlyph
                    coil={coil}
                    x={coilGlyphX}
                    y={cy}
                    selected={sel}
                    energised={coilEnergised}
                    onClick={() =>
                      onSelect(
                        sel ? null : { kind: "coil", rungIdx, coilIdx: ci },
                      )
                    }
                  />
                </g>
              )
            })}
          </>
        )}

        {/* Empty-rung hint when no coil declared. */}
        {coilCount === 0 && (
          <text
            x={coilColX + 12}
            y={networkOutY + 4}
            className="fill-muted-foreground"
            fontSize="10"
            fontFamily='"IBM Plex Mono", ui-monospace, monospace'
          >
            (no coil — add one →)
          </text>
        )}
      </svg>

      {/* Inline selection editors live below the SVG so they get
          predictable layout / scrolling rather than fighting with SVG
          coordinates. Trade-off: a tiny vertical jump when you select
          versus a popup library. */}
      <SelectionDetail
        prog={prog}
        rungIdx={rungIdx}
        rung={rung}
        selection={selection}
        readOnly={readOnly}
        onClose={() => onSelect(null)}
        onCommit={onCommit}
      />
    </div>
  )
}

function RungToolbar({
  rung,
  rungIdx,
  totalRungs,
  readOnly,
  hasError,
  onSelectRung,
  onLabelChange,
  onDelete,
  onMoveUp,
  onMoveDown,
  onAddCoil,
}: {
  rung: LdProgram["rungs"][number]
  rungIdx: number
  totalRungs: number
  readOnly: boolean
  hasError: boolean
  onSelectRung: () => void
  onLabelChange: (next: string | null) => void
  onDelete: () => void
  onMoveUp: () => void
  onMoveDown: () => void
  onAddCoil: () => void
}) {
  const [editingLabel, setEditingLabel] = useState(false)
  return (
    <div
      className={cn(
        "flex items-center gap-2 border-b border-border bg-muted/20 px-2 py-1 text-xs font-medium text-muted-foreground",
        hasError && "border-destructive/30 bg-destructive/5",
      )}
      onClick={onSelectRung}
    >
      {hasError && (
        <span
          className="font-mono text-destructive"
          title="This rung has compile errors — see banner above"
          aria-label="rung has errors"
        >
          ⚠
        </span>
      )}
      <span className="shrink-0">
        rung {rungIdx} · <span className="font-mono normal-case">{rung.id}</span>
      </span>
      <span className="flex-1 normal-case tracking-normal text-foreground/80">
        {editingLabel && !readOnly ? (
          <Input
            autoFocus
            defaultValue={rung.label ?? ""}
            className="h-6 text-xs"
            onBlur={(e) => {
              onLabelChange(e.target.value.trim() || null)
              setEditingLabel(false)
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                onLabelChange((e.target as HTMLInputElement).value.trim() || null)
                setEditingLabel(false)
              } else if (e.key === "Escape") {
                setEditingLabel(false)
              }
            }}
          />
        ) : (
          <span
            className="cursor-text"
            onClick={(e) => {
              e.stopPropagation()
              if (!readOnly) setEditingLabel(true)
            }}
          >
            {rung.label || (
              <span className="text-muted-foreground/60">click to label…</span>
            )}
          </span>
        )}
      </span>
      {!readOnly && (
        <div
          className="flex items-center gap-0.5"
          onClick={(e) => e.stopPropagation()}
        >
          <IconBtn
            title="Move up"
            disabled={rungIdx === 0}
            onClick={onMoveUp}
          >
            <ArrowUp className="size-3" />
          </IconBtn>
          <IconBtn
            title="Move down"
            disabled={rungIdx === totalRungs - 1}
            onClick={onMoveDown}
          >
            <ArrowDown className="size-3" />
          </IconBtn>
          <IconBtn title="Add coil" onClick={onAddCoil}>
            <Plus className="size-3" />
          </IconBtn>
          <IconBtn
            title="Delete rung"
            onClick={onDelete}
            className="hover:text-destructive"
          >
            <Trash2 className="size-3" />
          </IconBtn>
        </div>
      )}
    </div>
  )
}

function IconBtn({
  children,
  onClick,
  disabled,
  title,
  className,
}: {
  children: React.ReactNode
  onClick?: () => void
  disabled?: boolean
  title?: string
  className?: string
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      title={title}
      className={cn(
        "rounded p-0.5 text-muted-foreground transition-colors hover:bg-accent/40 hover:text-foreground disabled:cursor-not-allowed disabled:opacity-30",
        className,
      )}
    >
      {children}
    </button>
  )
}

// =================================================================
//   SVG node renderer (with click handlers)
// =================================================================

interface NodeRenderProps {
  node: LdNode
  path: NodePath
  x: number
  y: number
  cols: number
  rows: number
  rungIdx: number
  selection: Selection
  readOnly: boolean
  liveValues: LiveReadings | null
  onSelect: (s: Selection) => void
  onCommit: (transform: (prog: LdProgram) => LdProgram) => void
}

// =================================================================
//   Online-mode colour helper.
//
//   `powered === null`  → static rendering (no live reading: not running
//                         here, or a value the runtime does not publish),
//                         neutral foreground stroke.
//   `powered === true`  → wire / glyph is conducting, FX-Green stroke.
//   `powered === false` → wire / glyph is NOT conducting, muted.
//
//   We use stroke-current + a CSS text-* class so dark / light themes
//   both inherit the right colour automatically.
// =================================================================
function RenderNode(props: NodeRenderProps) {
  const { node, path, x, y, cols, rows, rungIdx, selection, liveValues, onSelect } = props
  const midY = y + ((rows - 1) * CELL_H) / 2 + CELL_H / 2
  const selected =
    selection?.kind === "node" &&
    selection.rungIdx === rungIdx &&
    pathsEqual(selection.path, path)
  // Power state of this whole sub-tree. Drives glyph colouring; child
  // recursions compute their own. Null means no live reading for it —
  // nothing running here, or a value the runtime does not publish — and
  // renders static.
  const powered = liveValues
    ? evaluateNode(node, liveValues)
    : null

  const click = () => onSelect(selected ? null : { kind: "node", rungIdx, path })

  switch (node.op) {
    case "contact":
      return (
        <ContactGlyph
          x={x}
          y={midY}
          width={CELL_W}
          name={node.var}
          negated={node.negated}
          selected={selected}
          powered={powered}
          onClick={click}
        />
      )
    case "const":
      return (
        <ConstGlyph
          x={x}
          y={midY}
          width={CELL_W}
          value={node.value}
          selected={selected}
          powered={powered}
          onClick={click}
        />
      )
    case "compare":
      return (
        <CompareGlyph
          x={x}
          y={midY}
          width={cols * CELL_W}
          left={operandText(node.left)}
          cmp={comparatorSymbol(node.cmp)}
          right={operandText(node.right)}
          selected={selected}
          powered={powered}
          onClick={click}
        />
      )
    case "fb_call":
      return (
        <FbCallGlyph
          x={x}
          y={midY}
          width={cols * CELL_W}
          instance={node.instance}
          fbType={node.fb_type}
          outputPin={node.output_pin}
          selected={selected}
          powered={powered}
          onClick={click}
        />
      )
    case "not": {
      if (node.arg.op === "contact") {
        return (
          <ContactGlyph
            x={x}
            y={midY}
            width={CELL_W}
            name={node.arg.var}
            negated={!node.arg.negated}
            selected={selected}
            powered={powered}
            onClick={click}
          />
        )
      }
      const m = measure(node.arg)
      return (
        <g>
          <RenderNode
            {...props}
            node={node.arg}
            path={[...path, 0]}
            cols={m.cols}
            rows={m.rows}
          />
          <rect
            x={x + 2}
            y={y + 2}
            width={m.cols * CELL_W - 4}
            height={m.rows * CELL_H - 4}
            fill="none"
            className="stroke-muted-foreground"
            strokeWidth={1}
            strokeDasharray="3 3"
            vectorEffect="non-scaling-stroke"
          />
          <text
            x={x + 4}
            y={y + 12}
            className="fill-muted-foreground"
            fontSize="9"
            fontFamily='"IBM Plex Mono", ui-monospace, monospace'
          >
            NOT
          </text>
        </g>
      )
    }
    case "and": {
      if (node.args.length === 0) {
        return (
          <ConstGlyph
            x={x}
            y={midY}
            width={CELL_W}
            value={true}
            selected={selected}
            powered={powered}
            onClick={click}
          />
        )
      }
      let cursor = x
      const elems: React.ReactNode[] = []
      // For an AND series, the wire BETWEEN args[k-1] and args[k] is
      // powered only if every arg up to and including k-1 conducts.
      // Children render their own glyphs with their own power state;
      // adjacent contact glyphs already cover the wire pixels via
      // their internal stroke. So we don't need to inject extra wires
      // here — the contact's own "lead-in" and "lead-out" sub-lines
      // already pick up that arg's power state.
      node.args.forEach((child, i) => {
        const m = measure(child)
        elems.push(
          <RenderNode
            key={i}
            {...props}
            node={child}
            path={[...path, i]}
            x={cursor}
            cols={m.cols}
            rows={rows}
          />,
        )
        cursor += m.cols * CELL_W
      })
      return <g>{elems}</g>
    }
    case "or": {
      if (node.args.length === 0) {
        return (
          <ConstGlyph
            x={x}
            y={midY}
            width={CELL_W}
            value={false}
            selected={selected}
            powered={powered}
            onClick={click}
          />
        )
      }
      let rowCursor = 0
      const elems: React.ReactNode[] = []
      const inX = x
      const outX = x + cols * CELL_W
      node.args.forEach((child, i) => {
        const m = measure(child)
        const laneY = y + rowCursor * CELL_H
        const laneMidY = laneY + ((m.rows - 1) * CELL_H) / 2 + CELL_H / 2
        // The per-branch padding wire is powered iff that branch
        // itself conducts — it's the branch's "tail" extending out
        // to align with longer siblings.
        const branchPowered = liveValues
          ? evaluateNode(child, liveValues)
          : null
        elems.push(
          <RenderNode
            key={i}
            {...props}
            node={child}
            path={[...path, i]}
            x={inX}
            y={laneY}
            cols={cols}
            rows={m.rows}
          />,
        )
        if (m.cols < cols) {
          elems.push(
            <line
              key={`pad-${i}`}
              x1={inX + m.cols * CELL_W}
              y1={laneMidY}
              x2={outX}
              y2={laneMidY}
              className={powerClass(branchPowered)}
              strokeWidth={1}
              vectorEffect="non-scaling-stroke"
            />,
          )
        }
        rowCursor += m.rows
      })
      const firstY = y + CELL_H / 2
      const lastChildRows = measure(node.args[node.args.length - 1]).rows
      const lastY =
        y + (rowCursor - lastChildRows) * CELL_H + lastChildRows * CELL_H - CELL_H / 2
      // Merge bars: powered if ANY branch is conducting, since the
      // OR's output is the disjunction.
      elems.push(
        <line
          key="merge-in"
          x1={inX}
          y1={firstY}
          x2={inX}
          y2={lastY}
          className={powerClass(powered)}
          strokeWidth={1}
          vectorEffect="non-scaling-stroke"
        />,
        <line
          key="merge-out"
          x1={outX}
          y1={firstY}
          x2={outX}
          y2={lastY}
          className={powerClass(powered)}
          strokeWidth={1}
          vectorEffect="non-scaling-stroke"
        />,
      )
      return <g>{elems}</g>
    }
  }
}

// =================================================================
//   Glyphs (with selection ring + click handlers)
// =================================================================

function ContactGlyph({
  x,
  y,
  width,
  name,
  negated,
  selected,
  powered,
  onClick,
}: {
  x: number
  y: number
  width: number
  name: string
  negated: boolean
  selected: boolean
  powered: boolean | null
  onClick: () => void
}) {
  const cx = x + width / 2
  const half = 10
  const wireClass = powerClass(powered)
  const barClass = powerClass(powered)
  return (
    <g onClick={onClick} className="cursor-pointer">
      {/* Wider invisible hit target so small contact glyphs are easy
          to click on a touchpad. */}
      <rect
        x={x + 4}
        y={y - 18}
        width={width - 8}
        height={36}
        fill="transparent"
      />
      <line
        x1={x}
        y1={y}
        x2={cx - half}
        y2={y}
        className={wireClass}
        strokeWidth={1}
        vectorEffect="non-scaling-stroke"
      />
      <line
        x1={cx + half}
        y1={y}
        x2={x + width}
        y2={y}
        className={wireClass}
        strokeWidth={1}
        vectorEffect="non-scaling-stroke"
      />
      <line
        x1={cx - half}
        y1={y - 9}
        x2={cx - half}
        y2={y + 9}
        className={barClass}
        strokeWidth={1.5}
        vectorEffect="non-scaling-stroke"
      />
      <line
        x1={cx + half}
        y1={y - 9}
        x2={cx + half}
        y2={y + 9}
        className={barClass}
        strokeWidth={1.5}
        vectorEffect="non-scaling-stroke"
      />
      {negated && (
        <line
          x1={cx - half}
          y1={y + 9}
          x2={cx + half}
          y2={y - 9}
          className={barClass}
          strokeWidth={1.5}
          vectorEffect="non-scaling-stroke"
        />
      )}
      <text
        x={cx}
        y={y - 14}
        textAnchor="middle"
        className="fill-foreground"
        fontSize="10"
        fontFamily='"IBM Plex Mono", ui-monospace, monospace'
      >
        {name}
      </text>
      {selected && (
        <rect
          x={cx - half - 3}
          y={y - 12}
          width={half * 2 + 6}
          height={24}
          fill="none"
          className="stroke-highlight"
          strokeWidth={1.5}
          vectorEffect="non-scaling-stroke"
          rx={2}
        />
      )}
    </g>
  )
}

function ConstGlyph({
  x,
  y,
  width,
  value,
  selected,
  powered,
  onClick,
}: {
  x: number
  y: number
  width: number
  value: boolean
  selected: boolean
  powered: boolean | null
  onClick: () => void
}) {
  // When live, a const node's "wire-powered" state is its declared
  // value (TRUE = always conducting, FALSE = never). When not live,
  // fall back to the static representation (solid vs dashed).
  const effectivePowered = powered === null ? null : value
  return (
    <g onClick={onClick} className="cursor-pointer">
      <rect
        x={x}
        y={y - 14}
        width={width}
        height={28}
        fill="transparent"
      />
      <line
        x1={x}
        y1={y}
        x2={x + width}
        y2={y}
        className={
          effectivePowered === null
            ? value
              ? "stroke-foreground"
              : "stroke-muted-foreground"
            : powerClass(effectivePowered)
        }
        strokeWidth={1}
        strokeDasharray={value ? undefined : "4 3"}
        vectorEffect="non-scaling-stroke"
      />
      <text
        x={x + width / 2}
        y={y - 8}
        textAnchor="middle"
        className="fill-muted-foreground"
        fontSize="9"
        fontFamily='"IBM Plex Mono", ui-monospace, monospace'
      >
        {value ? "TRUE" : "FALSE"}
      </text>
      {selected && (
        <rect
          x={x + 4}
          y={y - 12}
          width={width - 8}
          height={24}
          fill="none"
          className="stroke-highlight"
          strokeWidth={1.5}
          vectorEffect="non-scaling-stroke"
          rx={2}
        />
      )}
    </g>
  )
}

/** Compare block — rendered as a **square-cornered** rectangle in
 *  three rows: left operand on top, comparator in the middle (bigger,
 *  bold), right operand on the bottom. This is the canonical IEC /
 *  Codesys / TIA shape for in-network comparison blocks — engineers
 *  reading the rung can resolve "is `temperature` < `50.0`?" in a
 *  single glance because the operands stack vertically.
 *
 *  This is the IEC 61131-3 mechanism for bridging numeric variables
 *  into the boolean network — without it, an LD POU has no way to
 *  react to `temperature > 50` or `level < 0.2`. */
function CompareGlyph({
  x,
  y,
  width,
  left,
  cmp,
  right,
  selected,
  powered,
  onClick,
}: {
  x: number
  y: number
  width: number
  left: string
  cmp: string
  right: string
  selected: boolean
  powered: boolean | null
  onClick: () => void
}) {
  // Square box, three rows. Height slightly larger than a contact so
  // the three operand rows can breathe.
  const pad = 10
  const boxX = x + pad
  const boxW = width - pad * 2
  const boxH = 38
  const boxY = y - boxH / 2
  const cls = powerClass(powered)
  const labelClass =
    powered === null
      ? "fill-foreground"
      : powered
        ? "fill-highlight"
        : "fill-muted-foreground"
  return (
    <g onClick={onClick} className="cursor-pointer">
      {/* hit target covers full cell */}
      <rect
        x={x + 2}
        y={y - 22}
        width={width - 4}
        height={44}
        fill="transparent"
      />
      {/* lead-in / lead-out wires — meet the box at its vertical centre */}
      <line
        x1={x}
        y1={y}
        x2={boxX}
        y2={y}
        className={cls}
        strokeWidth={1}
        vectorEffect="non-scaling-stroke"
      />
      <line
        x1={boxX + boxW}
        y1={y}
        x2={x + width}
        y2={y}
        className={cls}
        strokeWidth={1}
        vectorEffect="non-scaling-stroke"
      />
      {/* Square-cornered rectangle (rx=0) — matches IEC compare-block
          convention from Codesys / TIA / Step7. */}
      <rect
        x={boxX}
        y={boxY}
        width={boxW}
        height={boxH}
        fill="none"
        className={cls}
        strokeWidth={1.25}
        vectorEffect="non-scaling-stroke"
      />
      {/* Three rows: left operand / comparator / right operand. The
          comparator is bold so the eye finds it first. */}
      <text
        x={boxX + boxW / 2}
        y={boxY + 11}
        textAnchor="middle"
        className={labelClass}
        fontSize="10"
        fontFamily='"IBM Plex Mono", ui-monospace, monospace'
      >
        {left}
      </text>
      <text
        x={boxX + boxW / 2}
        y={boxY + 22}
        textAnchor="middle"
        className={labelClass}
        fontSize="11"
        fontFamily='"IBM Plex Mono", ui-monospace, monospace'
        fontWeight={700}
      >
        {cmp}
      </text>
      <text
        x={boxX + boxW / 2}
        y={boxY + 33}
        textAnchor="middle"
        className={labelClass}
        fontSize="10"
        fontFamily='"IBM Plex Mono", ui-monospace, monospace'
      >
        {right}
      </text>
      {selected && (
        <rect
          x={boxX - 2}
          y={boxY - 2}
          width={boxW + 4}
          height={boxH + 4}
          fill="none"
          className="stroke-highlight"
          strokeWidth={1.5}
          vectorEffect="non-scaling-stroke"
        />
      )}
    </g>
  )
}

function FbCallGlyph({
  x,
  y,
  width,
  instance,
  fbType,
  outputPin,
  selected,
  powered,
  onClick,
}: {
  x: number
  y: number
  width: number
  instance: string
  fbType: string
  outputPin: string
  selected: boolean
  powered: boolean | null
  onClick: () => void
}) {
  // Industrial-LD convention for an inline FB-call glyph:
  //   - **instance name** sits above the box (small, regular weight).
  //     This is the variable name the rest of the program references.
  //   - **FB type** is the BIG label inside the box, bold and centred
  //     ("TON", "CTU", "R_TRIG"). The type is what tells you what the
  //     block DOES, so it gets the prime real estate.
  //   - **.outputPin** is a small bottom-right annotation, indicating
  //     which output feeds the network. Only printed when the FB has
  //     more than one BOOL output (CTUD); for single-output FBs ".Q"
  //     is implicit and we omit it for readability.
  //   - Square corners (rx=0), 1.25px border — matches Compare block
  //     so a mixed rung reads as a single visual family.
  const pad = 10
  const boxX = x + pad
  const boxW = width - pad * 2
  const boxH = 38
  const boxY = y - boxH / 2
  const cls = powerClass(powered)
  const labelClass =
    powered === null
      ? "fill-foreground"
      : powered
        ? "fill-highlight"
        : "fill-muted-foreground"
  // Hide `.Q` (the universal default) to reduce noise; show alternate
  // outputs explicitly.
  const showPinTag = outputPin !== "Q"
  return (
    <g onClick={onClick} className="cursor-pointer">
      {/* hit target */}
      <rect
        x={x + 2}
        y={y - 24}
        width={width - 4}
        height={48}
        fill="transparent"
      />
      {/* Instance name above the box, like a label on a real-world
          relay or component. Always foreground colour — independent
          of power state, because it's identity, not behaviour. */}
      <text
        x={boxX + boxW / 2}
        y={boxY - 4}
        textAnchor="middle"
        className="fill-foreground"
        fontSize="10"
        fontFamily='"IBM Plex Mono", ui-monospace, monospace'
      >
        {instance}
      </text>
      {/* lead-in / lead-out wires */}
      <line
        x1={x}
        y1={y}
        x2={boxX}
        y2={y}
        className={cls}
        strokeWidth={1}
        vectorEffect="non-scaling-stroke"
      />
      <line
        x1={boxX + boxW}
        y1={y}
        x2={x + width}
        y2={y}
        className={cls}
        strokeWidth={1}
        vectorEffect="non-scaling-stroke"
      />
      {/* The rectangle — square corners, same border weight as Compare. */}
      <rect
        x={boxX}
        y={boxY}
        width={boxW}
        height={boxH}
        fill="none"
        className={cls}
        strokeWidth={1.25}
        vectorEffect="non-scaling-stroke"
      />
      {/* FB type — the headline, bold + centred. */}
      <text
        x={boxX + boxW / 2}
        y={y + 5}
        textAnchor="middle"
        className={labelClass}
        fontSize="13"
        fontFamily='"IBM Plex Mono", ui-monospace, monospace'
        fontWeight={700}
      >
        {fbType}
      </text>
      {/* Output-pin annotation, bottom-right, only when non-default. */}
      {showPinTag && (
        <text
          x={boxX + boxW - 4}
          y={boxY + boxH - 4}
          textAnchor="end"
          className={labelClass}
          fontSize="9"
          fontFamily='"IBM Plex Mono", ui-monospace, monospace'
        >
          .{outputPin}
        </text>
      )}
      {selected && (
        <rect
          x={boxX - 2}
          y={boxY - 2}
          width={boxW + 4}
          height={boxH + 4}
          fill="none"
          className="stroke-highlight"
          strokeWidth={1.5}
          vectorEffect="non-scaling-stroke"
        />
      )}
    </g>
  )
}

function CoilGlyph({
  coil,
  x,
  y,
  selected,
  energised,
  onClick,
}: {
  coil: LdProgram["rungs"][number]["coils"][number]
  x: number
  y: number
  selected: boolean
  energised: boolean | null
  onClick: () => void
}) {
  const w = 36
  const r = 10
  const cx = x + w / 2
  const inner =
    coil.kind === "set" ? "S" : coil.kind === "reset" ? "R" : null
  // Coil "energised" colour reflects the coil's variable value, not
  // the network's output — set/reset latches hold their state across
  // scans even when the network goes back to 0, and a glowing coil
  // means "this output is currently driven HIGH", which is what
  // operators care about. For null (not live) use neutral.
  const arcClass =
    energised === null
      ? "stroke-foreground"
      : energised
        ? "stroke-highlight"
        : "stroke-muted-foreground/60"
  const innerClass =
    energised === null
      ? "fill-foreground"
      : energised
        ? "fill-highlight"
        : "fill-muted-foreground/60"
  return (
    <g onClick={onClick} className="cursor-pointer">
      <rect
        x={x - 4}
        y={y - 18}
        width={w + 8}
        height={36}
        fill="transparent"
      />
      <text
        x={cx}
        y={y - 14}
        textAnchor="middle"
        className="fill-foreground"
        fontSize="10"
        fontFamily='"IBM Plex Mono", ui-monospace, monospace'
      >
        {coil.var}
      </text>
      <path
        d={`M ${cx - r} ${y - r} A ${r * 1.2} ${r * 1.2} 0 0 0 ${cx - r} ${y + r}`}
        fill="none"
        className={arcClass}
        strokeWidth={1.5}
        vectorEffect="non-scaling-stroke"
      />
      <path
        d={`M ${cx + r} ${y - r} A ${r * 1.2} ${r * 1.2} 0 0 1 ${cx + r} ${y + r}`}
        fill="none"
        className={arcClass}
        strokeWidth={1.5}
        vectorEffect="non-scaling-stroke"
      />
      {inner && (
        <text
          x={cx}
          y={y + 4}
          textAnchor="middle"
          className={innerClass}
          fontSize="11"
          fontFamily='"IBM Plex Mono", ui-monospace, monospace'
          fontWeight={600}
        >
          {inner}
        </text>
      )}
      {selected && (
        <rect
          x={cx - r - 4}
          y={y - r - 4}
          width={(r + 4) * 2}
          height={(r + 4) * 2}
          fill="none"
          className="stroke-highlight"
          strokeWidth={1.5}
          vectorEffect="non-scaling-stroke"
          rx={2}
        />
      )}
    </g>
  )
}

// =================================================================
//   Selection detail panel (below the SVG)
//
//   When something is selected, an inline editor strip drops in
//   between the rung canvas and the next rung. Contains all the
//   actions you'd otherwise hide in popup menus / right-click.
// =================================================================

function SelectionDetail({
  prog,
  rungIdx,
  rung,
  selection,
  readOnly,
  onClose,
  onCommit,
}: {
  prog: LdProgram
  rungIdx: number
  rung: LdProgram["rungs"][number]
  selection: Selection
  readOnly: boolean
  onClose: () => void
  onCommit: (next: LdProgram) => void
}) {
  if (!selection || readOnly) return null
  if (selection.kind === "node" && selection.rungIdx === rungIdx) {
    return (
      <NodeDetail
        prog={prog}
        rungIdx={rungIdx}
        rung={rung}
        path={selection.path}
        onClose={onClose}
        onCommit={onCommit}
      />
    )
  }
  if (selection.kind === "coil" && selection.rungIdx === rungIdx) {
    return (
      <CoilDetail
        prog={prog}
        rungIdx={rungIdx}
        rung={rung}
        coilIdx={selection.coilIdx}
        onClose={onClose}
        onCommit={onCommit}
      />
    )
  }
  return null
}

function NodeDetail({
  prog,
  rungIdx,
  rung,
  path,
  onClose,
  onCommit,
}: {
  prog: LdProgram
  rungIdx: number
  rung: LdProgram["rungs"][number]
  path: NodePath
  onClose: () => void
  onCommit: (next: LdProgram) => void
}) {
  // Walk the path to find the selected node. Defensive against
  // stale selections that survive a structural edit by one frame.
  let node: LdNode | undefined = rung.logic
  try {
    for (const step of path) {
      if (!node) break
      if (node.op === "and" || node.op === "or") node = node.args[step]
      else if (node.op === "not") node = node.arg
      else node = undefined
    }
  } catch {
    node = undefined
  }
  if (!node) return null

  const boolVars = prog.variables
    .filter((v) => v.type === "BOOL" || v.type === "")
    .map((v) => v.name)

  return (
    <DetailBar onClose={onClose}>
      {node.op === "contact" ? (
        <>
          <DetailLabel>contact</DetailLabel>
          <VarPicker
            value={node.var}
            options={boolVars}
            onChange={(v) => onCommit(setContactVar(prog, rungIdx, path, v))}
          />
          <ToggleBtn
            active={node.negated}
            onClick={() => onCommit(toggleNegated(prog, rungIdx, path))}
            title="Toggle normally-closed"
          >
            ¬ negate
          </ToggleBtn>
        </>
      ) : node.op === "compare" ? (
        <CompareEditFields
          prog={prog}
          rungIdx={rungIdx}
          path={path}
          node={node}
          onCommit={onCommit}
        />
      ) : node.op === "fb_call" ? (
        <FbCallEditFields
          prog={prog}
          rungIdx={rungIdx}
          path={path}
          node={node}
          onCommit={onCommit}
        />
      ) : node.op === "const" ? (
        <>
          <DetailLabel>const</DetailLabel>
          <span className="font-mono text-xs">
            {node.value ? "TRUE" : "FALSE"}
          </span>
        </>
      ) : (
        <>
          <DetailLabel>{node.op}</DetailLabel>
          <span className="text-xs text-muted-foreground">
            {"args" in node && Array.isArray(node.args)
              ? `${node.args.length} branches`
              : "1 child"}
          </span>
        </>
      )}

      <Separator />
      <ActionBtn
        onClick={() =>
          onCommit(
            addInSeries(prog, rungIdx, path, "after", newContact()),
          )
        }
        title="Insert a contact in series to the right"
      >
        <Plus className="size-3" />
        contact
      </ActionBtn>
      <ActionBtn
        onClick={() =>
          onCommit(addInSeries(prog, rungIdx, path, "after", newCompare()))
        }
        title="Insert a compare block in series to the right"
      >
        <Plus className="size-3" />
        compare
      </ActionBtn>
      <FbInsertPicker
        title="Insert a function block in series to the right"
        onPick={(type) => {
          const { node: fb } = newFbCall(prog, type)
          onCommit(addInSeries(prog, rungIdx, path, "after", fb))
        }}
        label="fb"
      />
      <Separator />
      <ActionBtn
        onClick={() =>
          onCommit(
            addInParallel(prog, rungIdx, path, "after", newContact()),
          )
        }
        title="Insert a contact in parallel below"
      >
        <Plus className="size-3" />
        ⫽contact
      </ActionBtn>
      <ActionBtn
        onClick={() =>
          onCommit(addInParallel(prog, rungIdx, path, "after", newCompare()))
        }
        title="Insert a compare block in parallel below"
      >
        <Plus className="size-3" />
        ⫽compare
      </ActionBtn>
      <FbInsertPicker
        title="Insert a function block in parallel below"
        onPick={(type) => {
          const { node: fb } = newFbCall(prog, type)
          onCommit(addInParallel(prog, rungIdx, path, "after", fb))
        }}
        label="⫽fb"
      />
      <Separator />
      <ActionBtn
        onClick={() => {
          onCommit(deleteNode(prog, rungIdx, path))
          onClose()
        }}
        title="Delete this element"
        destructive
      >
        <Trash2 className="size-3" />
        delete
      </ActionBtn>
    </DetailBar>
  )
}

/** Compact editing controls for a Compare block:
 *  [left-kind ↓] [left-value]  [cmp ↓]  [right-kind ↓] [right-value]
 *  Each operand chooses between var-reference or inline literal. */
function CompareEditFields({
  prog,
  rungIdx,
  path,
  node,
  onCommit,
}: {
  prog: LdProgram
  rungIdx: number
  path: NodePath
  node: Extract<LdNode, { op: "compare" }>
  onCommit: (next: LdProgram) => void
}) {
  const numericVars = prog.variables
    .filter((v) => v.type !== "BOOL")
    .map((v) => v.name)
  return (
    <>
      <DetailLabel>compare</DetailLabel>
      <OperandPicker
        operand={node.left}
        options={numericVars}
        onChange={(left) =>
          onCommit(updateCompare(prog, rungIdx, path, { left }))
        }
      />
      <Select
        value={node.cmp}
        onValueChange={(v) =>
          onCommit(updateCompare(prog, rungIdx, path, { cmp: v as typeof node.cmp }))
        }
      >
        <SelectTrigger className="h-7 w-16 text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="eq">=</SelectItem>
          <SelectItem value="ne">≠</SelectItem>
          <SelectItem value="lt">&lt;</SelectItem>
          <SelectItem value="le">≤</SelectItem>
          <SelectItem value="gt">&gt;</SelectItem>
          <SelectItem value="ge">≥</SelectItem>
        </SelectContent>
      </Select>
      <OperandPicker
        operand={node.right}
        options={numericVars}
        onChange={(right) =>
          onCommit(updateCompare(prog, rungIdx, path, { right }))
        }
      />
    </>
  )
}

/**
 * Editor row for an FbCall node. Three groups of controls:
 *
 *   1. Identity:  `[FB type ↓]  instance-name-input  → [output-pin ↓]`
 *   2. Pins:      one row per input pin, each an OperandPicker preceded
 *                 by the pin name.
 *
 * Layout is horizontal — for an FB with many pins (CTUD has 5 inputs)
 * the row wraps; that's fine, this bar already breaks for long Compare
 * blocks. Tight + scannable beats fancy + scrolling for editor work.
 */
function FbCallEditFields({
  prog,
  rungIdx,
  path,
  node,
  onCommit,
}: {
  prog: LdProgram
  rungIdx: number
  path: NodePath
  node: Extract<LdNode, { op: "fb_call" }>
  onCommit: (next: LdProgram) => void
}) {
  const def = fbByType(node.fb_type)
  const inputs = fbInputs(node.fb_type)
  // BOOL outputs are the only pins that can feed the surrounding
  // network. Most FBs only have one, but CTUD has two and we want to
  // let the user pick.
  const boolOutputs = fbOutputs(node.fb_type).filter((p) => p.type === "BOOL")
  // For each input pin's OperandPicker, pre-filter variable options
  // by IEC type so users only see relevant choices.
  const varsByType = (iecType: string) =>
    prog.variables.filter((v) => v.type === iecType).map((v) => v.name)
  return (
    <>
      <DetailLabel>fb</DetailLabel>
      <Select
        value={node.fb_type}
        onValueChange={(v) => onCommit(setFbType(prog, rungIdx, path, v))}
      >
        <SelectTrigger className="h-7 w-28 text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {groupedFbs().map((group) => (
            <SelectGroup key={group.label}>
              <SelectLabel>{group.label}</SelectLabel>
              {group.fbs.map((fb) => (
                <SelectItem key={fb.type} value={fb.type} title={fbPinHint(fb)}>
                  {fb.type}
                </SelectItem>
              ))}
            </SelectGroup>
          ))}
        </SelectContent>
      </Select>
      <InstanceNameInput
        value={node.instance}
        onCommit={(v) => onCommit(updateFbCall(prog, rungIdx, path, { instance: v }))}
      />
      {boolOutputs.length > 1 ? (
        <Select
          value={node.output_pin}
          onValueChange={(v) =>
            onCommit(updateFbCall(prog, rungIdx, path, { output_pin: v }))
          }
        >
          <SelectTrigger className="h-7 w-16 text-xs" title="Output pin">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {boolOutputs.map((p) => (
              <SelectItem key={p.pin} value={p.pin}>
                .{p.pin}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      ) : (
        <span
          className="font-mono text-xs text-muted-foreground"
          title={def ? def.description : "Custom FB"}
        >
          .{node.output_pin}
        </span>
      )}
      {inputs.map((pin) => {
        const binding = node.inputs.find((i) => i.pin === pin.pin)
        return (
          <span key={pin.pin} className="inline-flex items-center gap-1">
            <span
              className="font-mono text-xs text-muted-foreground"
              title={`${pin.doc} (${pin.type})`}
            >
              {pin.pin}:
            </span>
            <OperandPicker
              operand={
                binding?.value ?? {
                  kind: "literal" as const,
                  value: pin.type === "TIME" ? "T#0ms" : "0",
                }
              }
              options={varsByType(pin.type)}
              onChange={(v) =>
                onCommit(setFbInputValue(prog, rungIdx, path, pin.pin, v))
              }
            />
          </span>
        )
      })}
    </>
  )
}

/** Compact instance-name editor — same pattern as OperandPicker's text
 *  input: local draft, commit on blur / Enter. Validation is "must be
 *  a legal IEC identifier"; non-conforming names get rejected silently
 *  on commit (the caller's set never lands). */
function InstanceNameInput({
  value,
  onCommit,
}: {
  value: string
  onCommit: (next: string) => void
}) {
  const [draft, setDraft] = useState(value)
  useEffect(() => setDraft(value), [value])
  const commit = (next: string) => {
    const trimmed = next.trim()
    if (!trimmed || trimmed === value) {
      setDraft(value)
      return
    }
    // Cheap IEC identifier check — letters/digits/underscore, must not
    // start with a digit. Anything else snaps back to the previous
    // value rather than committing garbage.
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(trimmed)) {
      setDraft(value)
      return
    }
    onCommit(trimmed)
  }
  return (
    <input
      type="text"
      value={draft}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => commit(draft)}
      onKeyDown={(e) => {
        if (e.key === "Enter") commit(draft)
      }}
      className="h-7 w-24 rounded border border-input bg-transparent px-2 font-mono text-xs"
      title="FB instance name"
    />
  )
}

/** A button-shaped Select that lets the user pick an FB type and
 *  inserts a new FbCall when chosen. Used by the NodeDetail action
 *  row. Renders identical-looking to ActionBtn so it slots in beside
 *  "+ contact" / "+ compare" without visual jolt. */
function FbInsertPicker({
  title,
  onPick,
  label,
}: {
  title: string
  onPick: (fbType: string) => void
  label: string
}) {
  return (
    <Select
      // Force a remount each time the user picks something so the
      // select returns to its placeholder state instead of getting
      // stuck on the last picked type.
      value=""
      onValueChange={(v) => {
        if (v) onPick(v)
      }}
    >
      <SelectTrigger
        className="h-7 gap-1 px-2 text-xs"
        title={title}
        aria-label={title}
      >
        <Plus className="size-3" />
        {label}
      </SelectTrigger>
      <SelectContent>
        {groupedFbs().map((group) => (
          <SelectGroup key={group.label}>
            <SelectLabel>{group.label}</SelectLabel>
            {group.fbs.map((fb) => (
              <SelectItem key={fb.type} value={fb.type} title={fbPinHint(fb)}>
                <span className="font-mono">{fb.type}</span>
                <span className="ml-2 text-muted-foreground">
                  {fb.label.replace(`${fb.type} — `, "")}
                </span>
              </SelectItem>
            ))}
          </SelectGroup>
        ))}
      </SelectContent>
    </Select>
  )
}

/** A single operand picker: a small select to choose var-vs-literal,
 *  then a text input for the name / literal value. */
function OperandPicker({
  operand,
  options,
  onChange,
}: {
  operand: { kind: "var"; name: string } | { kind: "literal"; value: string }
  options: string[]
  onChange: (next: { kind: "var"; name: string } | { kind: "literal"; value: string }) => void
}) {
  const [draft, setDraft] = useState(
    operand.kind === "var" ? operand.name : operand.value,
  )
  useEffect(() => {
    setDraft(operand.kind === "var" ? operand.name : operand.value)
  }, [operand])
  const commit = (next: string) => {
    if (operand.kind === "var" && next !== operand.name) {
      onChange({ kind: "var", name: next })
    } else if (operand.kind === "literal" && next !== operand.value) {
      onChange({ kind: "literal", value: next })
    }
  }
  return (
    <span className="inline-flex gap-1">
      <Select
        value={operand.kind}
        onValueChange={(k) =>
          onChange(
            k === "var"
              ? { kind: "var", name: draft || options[0] || "x" }
              : { kind: "literal", value: draft || "0" },
          )
        }
      >
        <SelectTrigger className="h-7 w-14 text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="var">var</SelectItem>
          <SelectItem value="literal">lit</SelectItem>
        </SelectContent>
      </Select>
      <input
        type="text"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={() => commit(draft.trim())}
        onKeyDown={(e) => {
          if (e.key === "Enter") commit(draft.trim())
        }}
        className="h-7 w-24 rounded border border-input bg-transparent px-2 font-mono text-xs"
        list={
          operand.kind === "var" ? "ld-compare-var-options" : undefined
        }
      />
      {operand.kind === "var" && (
        <datalist id="ld-compare-var-options">
          {options.map((o) => (
            <option key={o} value={o} />
          ))}
        </datalist>
      )}
    </span>
  )
}

function CoilDetail({
  prog,
  rungIdx,
  rung,
  coilIdx,
  onClose,
  onCommit,
}: {
  prog: LdProgram
  rungIdx: number
  rung: LdProgram["rungs"][number]
  coilIdx: number
  onClose: () => void
  onCommit: (next: LdProgram) => void
}) {
  const coil = rung.coils[coilIdx]
  if (!coil) return null
  const candidates = prog.variables
    .filter((v) => v.type === "BOOL")
    .map((v) => v.name)
  return (
    <DetailBar onClose={onClose}>
      <DetailLabel>coil</DetailLabel>
      <VarPicker
        value={coil.var}
        options={candidates}
        onChange={(v) => onCommit(setCoilVar(prog, rungIdx, coilIdx, v))}
      />
      <Select
        value={coil.kind}
        onValueChange={(v) =>
          onCommit(setCoilKind(prog, rungIdx, coilIdx, v as LdCoilKind))
        }
      >
        <SelectTrigger className="h-7 w-32 text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="standard">standard ( )</SelectItem>
          <SelectItem value="set">set (S)</SelectItem>
          <SelectItem value="reset">reset (R)</SelectItem>
        </SelectContent>
      </Select>
      <Separator />
      <ActionBtn
        onClick={() => {
          onCommit(deleteCoil(prog, rungIdx, coilIdx))
          onClose()
        }}
        title="Remove this coil"
        destructive
      >
        <Trash2 className="size-3" />
        delete
      </ActionBtn>
    </DetailBar>
  )
}

// =================================================================
//   Detail-bar primitives
// =================================================================

function DetailBar({
  onClose,
  children,
}: {
  onClose: () => void
  children: React.ReactNode
}) {
  return (
    <div className="flex items-center gap-2 border-t border-highlight/30 bg-highlight/5 px-3 py-1.5 text-xs">
      {children}
      <button
        type="button"
        onClick={onClose}
        className="ml-auto rounded p-0.5 text-muted-foreground hover:bg-accent/40 hover:text-foreground"
        title="Close"
      >
        <X className="size-3" />
      </button>
    </div>
  )
}

function ToggleBtn({
  active,
  onClick,
  title,
  children,
}: {
  active: boolean
  onClick: () => void
  title?: string
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={title}
      className={cn(
        "inline-flex items-center gap-1 rounded px-1.5 py-0.5 font-mono text-xs transition-colors",
        active
          ? "bg-highlight/15 text-highlight"
          : "text-muted-foreground hover:bg-accent/40 hover:text-foreground",
      )}
    >
      {children}
    </button>
  )
}

/** Combo input + datalist. Avoids the overhead of a real Select
 *  for the common case ("type the variable name and pick a suggestion
 *  from declared variables").
 *
 *  Internal draft state so typing doesn't trigger a JSON serialise /
 *  parse round-trip on every keystroke. We commit on blur / Enter,
 *  and reset the draft when `value` changes externally (e.g. user
 *  selects a different element). */
function VarPicker({
  value,
  options,
  onChange,
}: {
  value: string
  options: string[]
  onChange: (v: string) => void
}) {
  const id = useRef(`varpicker-${Math.random().toString(36).slice(2, 8)}`)
  const [draft, setDraft] = useState(value)
  useEffect(() => {
    setDraft(value)
  }, [value])
  return (
    <>
      <input
        list={id.current}
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={() => {
          const next = draft.trim()
          if (next && next !== value) onChange(next)
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            const next = draft.trim()
            if (next && next !== value) onChange(next)
          } else if (e.key === "Escape") {
            setDraft(value)
            ;(e.target as HTMLInputElement).blur()
          }
        }}
        className="h-7 w-32 rounded border border-input bg-transparent px-2 font-mono text-xs"
      />
      <datalist id={id.current}>
        {options.map((o) => (
          <option key={o} value={o} />
        ))}
      </datalist>
    </>
  )
}

// =================================================================
//   Helpers
// =================================================================

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

function pathsEqual(a: NodePath, b: NodePath) {
  if (a.length !== b.length) return false
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false
  return true
}

/** Mark `RotateCw` as referenced so its tree-shake doesn't whine —
 *  we ship the icon for a future "rotate / swap" action. */
void RotateCw
