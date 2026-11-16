import { powerClass } from "@/components/editor/shared/power"
import { blockOutputs } from "@/lib/fbd-edit"
import { cn } from "@/lib/utils"
import type { FbdBlock } from "@/types/generated/FbdBlock"
import type { LivePins } from "./types"
import { PIN_ROW_H, HEADER_H, STUB_LEN, type BlockLayout } from "./layout"

export function BlockGlyph({
  block,
  layout,
  liveValues,
  hasError,
  selected,
  readOnly,
  onPointerDown,
  onOutputPinPointerDown,
}: {
  block: FbdBlock
  layout: BlockLayout
  liveValues: LivePins | null
  hasError: boolean
  selected: boolean
  readOnly: boolean
  onPointerDown: (e: React.PointerEvent) => void
  onOutputPinPointerDown: (pin: string, e: React.PointerEvent) => void
}) {
  const outputs = blockOutputs(block)
  // Industrial-FBD convention (Codesys / TIA / Step7):
  //   - **Instance name** sits ABOVE the block (regular weight, small).
  //     It's the variable name; the rest of the program references it.
  //   - **FB type** is BOLD at the top inside the box. The type tells
  //     you what the block does, so it gets the most visual weight.
  //   - **Input pin names** left-aligned just inside the left edge.
  //   - **Output pin names** right-aligned just inside the right edge.
  //   - **Input values** (vars / literals / `inst.pin`) printed in a
  //     fainter colour after the pin name — present only when the
  //     pin isn't being wired by another block, since wired inputs
  //     are visually obvious from the connecting line.
  //   - The box is a plain rectangle, square corners. No header strip,
  //     no rounded corners, no "card shadow".
  return (
    <g>
      {/* Instance label above the box. Stays outside the click/drag
          target so a quick click doesn't accidentally start a drag. */}
      <text
        x={layout.x + layout.w / 2}
        y={layout.y - 6}
        textAnchor="middle"
        className="fill-foreground pointer-events-none"
        fontSize="10"
        fontFamily='"IBM Plex Mono", ui-monospace, monospace'
      >
        {block.instance}
      </text>
      {/* The box. Square corners, single-weight border. */}
      <rect
        x={layout.x}
        y={layout.y}
        width={layout.w}
        height={layout.h}
        className={cn(
          "fill-card",
          hasError
            ? "stroke-destructive"
            : selected
              ? "stroke-highlight"
              : "stroke-foreground",
        )}
        strokeWidth={selected || hasError ? 2 : 1.25}
        vectorEffect="non-scaling-stroke"
        onPointerDown={onPointerDown}
        style={{ cursor: readOnly ? "default" : "move" }}
      />
      {/* FB type — bold, centred, top of the inner area. The hit-test
          rect above the body keeps drag working over this region. */}
      <rect
        x={layout.x}
        y={layout.y}
        width={layout.w}
        height={HEADER_H}
        fill="transparent"
        onPointerDown={onPointerDown}
        style={{ cursor: readOnly ? "default" : "move" }}
      />
      <text
        x={layout.x + layout.w / 2}
        y={layout.y + 17}
        textAnchor="middle"
        className="fill-foreground pointer-events-none"
        fontSize="13"
        fontFamily='"IBM Plex Mono", ui-monospace, monospace'
        fontWeight={700}
      >
        {block.fb_type}
      </text>

      {/* Input pins (left side, inside the box) */}
      {(block.inputs.length > 0 ? block.inputs : [null]).map((input, i) => {
        const y = layout.y + HEADER_H + i * PIN_ROW_H + PIN_ROW_H / 2
        if (!input) {
          return (
            <text
              key="empty"
              x={layout.x + 6}
              y={y + 3}
              className="fill-muted-foreground pointer-events-none"
              fontSize="10"
              fontFamily='"IBM Plex Mono", ui-monospace, monospace'
              fontStyle="italic"
            >
              no inputs
            </text>
          )
        }
        // Wired inputs show only the pin name — the value is visible
        // from the wire itself, no need to clutter the box with it.
        // For var/literal we print the operand in muted text after
        // the pin name. The narrow `if/else` (vs. ternary) keeps
        // TypeScript's discriminated-union narrowing happy.
        let valueText: string | null = null
        if (input.value.kind === "var") {
          valueText = input.value.name
        } else if (input.value.kind === "literal") {
          valueText = input.value.value
        }
        return (
          <g key={input.pin}>
            {/* Connector stub: short horizontal line poking out from
                the block edge with a dot at its far end (Codesys /
                TIA convention). Wires terminate at the dot, not at
                the block edge. */}
            <line
              x1={layout.x - STUB_LEN}
              y1={y}
              x2={layout.x}
              y2={y}
              className="stroke-foreground pointer-events-none"
              strokeWidth={1}
              vectorEffect="non-scaling-stroke"
            />
            <circle
              cx={layout.x - STUB_LEN}
              cy={y}
              r={2.5}
              className="fill-foreground pointer-events-none"
            />
            {/* Pin name, inside-left, bold. */}
            <text
              x={layout.x + 5}
              y={y + 3}
              className="fill-foreground pointer-events-none"
              fontSize="10"
              fontFamily='"IBM Plex Mono", ui-monospace, monospace'
              fontWeight={600}
            >
              {input.pin}
            </text>
            {/* Value to the right of the pin name, smaller + muted. */}
            {valueText && (
              <text
                x={layout.x + 5 + input.pin.length * 6.5 + 6}
                y={y + 3}
                className="fill-muted-foreground pointer-events-none"
                fontSize="9"
                fontFamily='"IBM Plex Mono", ui-monospace, monospace'
              >
                {valueText}
              </text>
            )}
          </g>
        )
      })}

      {/* Output pins on the right side. Same pattern as inputs:
          pin name INSIDE right-aligned, bold, with a connector stub
          (short line + dot) reaching outside the box. Each dot is a
          drag handle for the wire-creation gesture — EVERY output is
          wireable (a PID's REAL `out`, a timer's `ET`, a BOOL `Q`),
          since the transpiler doesn't restrict wires to a BOOL
          network. */}
      {outputs.map((pin, i, arr) => {
        const cy = layout.y + HEADER_H + ((i + 0.5) * (layout.h - HEADER_H)) / arr.length
        const live = liveValues ? liveValues.pin(block, pin) : null
        return (
          <g key={pin}>
            {/* Pin name, inside-right, bold — matches the input
                pattern visually. */}
            <text
              x={layout.x + layout.w - 5}
              y={cy + 3}
              textAnchor="end"
              className="fill-foreground pointer-events-none"
              fontSize="10"
              fontFamily='"IBM Plex Mono", ui-monospace, monospace'
              fontWeight={600}
            >
              {pin}
            </text>
            {/* Connector stub outside the right edge. */}
            <line
              x1={layout.x + layout.w}
              y1={cy}
              x2={layout.x + layout.w + STUB_LEN}
              y2={cy}
              className="stroke-foreground pointer-events-none"
              strokeWidth={1}
              vectorEffect="non-scaling-stroke"
            />
            {/* Larger transparent hit area for the wire-drag gesture. */}
            <circle
              cx={layout.x + layout.w + STUB_LEN}
              cy={cy}
              r={9}
              fill="transparent"
              style={{ cursor: readOnly ? "default" : "crosshair" }}
              onPointerDown={(e) => onOutputPinPointerDown(pin, e)}
            />
            <circle
              cx={layout.x + layout.w + STUB_LEN}
              cy={cy}
              r={2.5}
              className={powerClass(live).replace("stroke-", "fill-")}
              pointerEvents="none"
            />
          </g>
        )
      })}
    </g>
  )
}
