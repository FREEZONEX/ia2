import { ArrowDown, ArrowUp, Plus, Trash2 } from "@/components/ui/icons"
import { useState } from "react"
import { powerClass } from "@/components/editor/shared/power"
import { Input } from "@/components/ui/input"
import { cn } from "@/lib/utils"
import type { CheckDiagnostic } from "@/types/generated/CheckDiagnostic"
import {
  addCoil,
  deleteRung,
  evaluateNode,
  moveRung,
  setRungLabel,
  type LiveReadings,
} from "@/lib/ld-edit"
import type { LdProgram } from "@/types/generated/LdProgram"
import type { Selection } from "./selection"
import { RAIL_PAD, layoutRung } from "./layout"
import { RenderNode } from "./RenderNode"
import { CoilGlyph } from "./Glyphs"
import { SelectionDetail } from "./SelectionDetail"

export function RungEditor({
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
  const {
    cols, networkRows, networkY, width, height,
    coilCount, coilColX, coilGlyphX, coilYs, networkOutY,
  } = layoutRung(rung)

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
