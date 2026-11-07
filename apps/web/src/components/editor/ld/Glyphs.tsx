import { powerClass } from "@/components/editor/shared/power"
import type { LdProgram } from "@/types/generated/LdProgram"

export function ContactGlyph({
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

export function ConstGlyph({
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
export function CompareGlyph({
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

export function FbCallGlyph({
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

export function CoilGlyph({
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
