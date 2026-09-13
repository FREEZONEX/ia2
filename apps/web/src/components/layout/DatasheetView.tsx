import { ChevronRight, Lock } from "@/components/ui/icons"
import { useMemo, useState } from "react"

import { STEditor } from "@/components/editor/STEditor"
import { cn } from "@/lib/utils"
import {
  parseBlockDatasheet,
  type BlockDatasheet,
  type DatasheetPin,
} from "@/lib/library-datasheet"

/**
 * Engineering-facing view of a library FUNCTION_BLOCK: a datasheet
 * (graphic preview + interface table + documentation) instead of raw
 * ST. The source is still one click away, folded at the bottom.
 *
 * Replaces the read-only ST editor for `pous/lib/**` blocks — the
 * point being that a library block is a contract to USE, not code to
 * read. See docs decision in the library-block UX discussion.
 */
export function DatasheetView({
  source,
  libraryName,
}: {
  source: string
  libraryName: string
}) {
  const sheet = useMemo(() => parseBlockDatasheet(source), [source])
  const [sourceOpen, setSourceOpen] = useState(false)

  return (
    <div className="h-full min-h-0 overflow-auto">
      <div className="max-w-5xl px-4 py-5">
        {/* Header */}
        <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
          <span className="break-words font-mono text-base font-medium text-foreground">
            {sheet.name}
          </span>
          {sheet.brief && (
            <span className="text-[13px] text-muted-foreground">{sheet.brief}</span>
          )}
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-1.5">
          <span className="rounded bg-muted px-2 py-0.5 font-mono text-xs text-muted-foreground">
            {libraryName}
          </span>
          <span className="inline-flex items-center gap-1 rounded bg-muted px-2 py-0.5 text-xs text-muted-foreground">
            <Lock className="size-2.5" />
            read-only
          </span>
        </div>

        {/* Graphic preview + interface table */}
        <div className="mt-5 flex flex-wrap items-start gap-5">
          <div className="w-[220px] max-w-full shrink-0 bg-muted/40 p-3">
            <BlockPreview sheet={sheet} />
          </div>
          <div className="min-w-0 flex-1 basis-[360px]">
            <PinTable title="Inputs" pins={sheet.inputs} showDefault />
            {sheet.outputs.length > 0 && (
              <div className="mt-3">
                <PinTable title="Outputs" pins={sheet.outputs} />
              </div>
            )}
          </div>
        </div>

        {/* Documentation */}
        {sheet.sections.length > 0 && (
          <div className="mt-6 space-y-3 border-t border-border pt-4">
            {sheet.sections.map((s, i) => (
              <div
                key={i}
                className={cn(
                  "text-[13px] leading-relaxed",
                  s.equivalence
                    ? "text-muted-foreground italic"
                    : "text-muted-foreground",
                )}
              >
                {s.label && (
                  <span className="font-medium text-foreground">
                    {s.label}:{" "}
                  </span>
                )}
                {s.equivalence && <span aria-hidden>≈ </span>}
                <span className="whitespace-pre-wrap">{s.body}</span>
              </div>
            ))}
          </div>
        )}

        {/* Usage hint — how to actually place / call the block */}
        <div className="mt-6 bg-muted/40 px-3 py-2 text-[13px] text-muted-foreground">
          <span className="font-medium text-foreground">Use it: </span>
          add it from the <span className="font-mono">+ Block</span> palette in
          an FBD or LD editor, or declare{" "}
          <span className="font-mono">inst : {sheet.name};</span> and call{" "}
          <span className="font-mono">inst(…)</span> in ST.
        </div>

        {/* Folded ST source */}
        <div className="mt-6 border-t border-border pt-3">
          <button
            type="button"
            onClick={() => setSourceOpen((v) => !v)}
            aria-expanded={sourceOpen}
            className="flex items-center gap-1.5 text-[13px] text-muted-foreground hover:text-foreground"
          >
            <ChevronRight
              className={cn(
                "size-3.5 transition-transform",
                sourceOpen && "rotate-90",
              )}
            />
            View ST source
            <span className="font-mono text-xs text-muted-foreground">
              ({sheet.name.toLowerCase()}.st — read-only)
            </span>
          </button>
          {sourceOpen && (
            <div className="mt-2 h-[420px] overflow-hidden rounded-md border border-border">
              <STEditor
                value={source}
                onChange={() => {}}
                diagnostics={[]}
                readOnly
              />
            </div>
          )}
        </div>
      </div>
    </div>
  )
}

function PinTable({
  title,
  pins,
  showDefault = false,
}: {
  title: string
  pins: DatasheetPin[]
  showDefault?: boolean
}) {
  if (pins.length === 0) return null
  return (
    <div>
      <div className="mb-1 text-xs text-muted-foreground">
        {title}
      </div>
      <div className="min-w-0 overflow-x-auto">
        <table className="ia2-table w-full min-w-[400px]">
          <tbody>
            {pins.map((p) => (
              <tr key={p.name} className="border-b border-border/60 last:border-0 align-top">
                <td className="py-1 pr-3 font-mono text-foreground">{p.name}</td>
                <td className="py-1 pr-3 text-muted-foreground">{p.type}</td>
                {showDefault && (
                  <td className="py-1 pr-3 font-mono text-[12px] text-muted-foreground">
                    {p.default ?? "—"}
                  </td>
                )}
                <td className="w-1/2 py-1 text-muted-foreground">
                  {p.description ?? ""}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  )
}

/** Self-contained FBD-style block glyph: a rectangle with the FB name,
 *  input pins down the left, output pins down the right. Mirrors how the
 *  block reads in the FBD editor so the datasheet and the diagram agree. */
function BlockPreview({ sheet }: { sheet: BlockDatasheet }) {
  const HEADER = 26
  const ROW = 20
  const PAD_BOTTOM = 10
  const rows = Math.max(sheet.inputs.length, sheet.outputs.length, 1)
  const blockH = HEADER + rows * ROW + PAD_BOTTOM
  const blockX = 56
  const blockW = 96
  const blockY = 8
  const stub = 14
  const w = blockX + blockW + stub + 8
  const h = blockY + blockH + 8

  const pinY = (i: number) => blockY + HEADER + i * ROW + ROW / 2

  return (
    <svg
      width="100%"
      viewBox={`0 0 ${w} ${h}`}
      role="img"
      aria-label={`${sheet.name} block with ${sheet.inputs.length} inputs and ${sheet.outputs.length} outputs`}
    >
      {/* the box first, so the pin labels render on top of it */}
      <rect
        x={blockX}
        y={blockY}
        width={blockW}
        height={blockH}
        rx={3}
        className="fill-card stroke-foreground"
        strokeWidth={1}
      />
      <text
        x={blockX + blockW / 2}
        y={blockY + 16}
        textAnchor="middle"
        className="fill-foreground"
        fontSize="10"
        fontWeight={700}
        fontFamily="ui-monospace, monospace"
      >
        {sheet.name}
      </text>
      {/* input stubs + names */}
      {sheet.inputs.map((p, i) => (
        <g key={`i-${p.name}`}>
          <line
            x1={blockX - stub}
            y1={pinY(i)}
            x2={blockX}
            y2={pinY(i)}
            className="stroke-muted-foreground/50"
            strokeWidth={1}
          />
          <circle
            cx={blockX - stub}
            cy={pinY(i)}
            r={2.5}
            className="fill-highlight"
          />
          <text
            x={blockX + 5}
            y={pinY(i) + 3}
            className="fill-foreground"
            fontSize="9"
            fontFamily="ui-monospace, monospace"
          >
            {p.name}
          </text>
        </g>
      ))}
      {/* output stubs */}
      {sheet.outputs.map((p, i) => (
        <g key={`o-${p.name}`}>
          <line
            x1={blockX + blockW}
            y1={pinY(i)}
            x2={blockX + blockW + stub}
            y2={pinY(i)}
            className="stroke-muted-foreground/50"
            strokeWidth={1}
          />
          <circle
            cx={blockX + blockW + stub}
            cy={pinY(i)}
            r={2.5}
            className="fill-highlight"
          />
          <text
            x={blockX + blockW - 5}
            y={pinY(i) + 3}
            textAnchor="end"
            className="fill-foreground"
            fontSize="9"
            fontFamily="ui-monospace, monospace"
          >
            {p.name}
          </text>
        </g>
      ))}
    </svg>
  )
}
