/**
 * Catalogue of IEC 61131-3 standard function blocks.
 *
 * This is **front-end metadata only** — pin names, types, default
 * values, and human-readable descriptions used by the LD editor to
 * render FB rectangles, populate pin pickers, and validate authoring.
 *
 * The library itself (the actual timing/counting logic) lives in
 * `vendor/ironplc/compiler/vm/src/intrinsic.rs` as Rust intrinsics. We
 * don't reimplement it here. The pin definitions below mirror what
 * ironplc's analyzer accepts; if ironplc adds a new FB to its standard
 * set, add it here too. See `MEMORY/graphical-languages.md` § "Standard
 * function block library — owned by ironplc" for the architecture.
 *
 * Pin types are IEC type names (BOOL / INT / TIME etc.) used purely for
 * documentation and editor input hints. The transpiler does NOT validate
 * them — ironplc enforces type compatibility at compile time, and its
 * diagnostics are the source of truth.
 */

export type FbPinDirection = "input" | "output"

export interface FbPin {
  /** Pin name as it appears in ST: `IN`, `PT`, `CU`, `Q1`, `CV`, ... */
  pin: string
  /** Direction — drives which side of the rendered rectangle it sits on. */
  direction: FbPinDirection
  /** IEC type name. BOOL for logical signals, TIME for `T#3s` etc. */
  type: string
  /**
   * Short docstring shown in tooltips. Keep under 60 chars — it
   * appears beside the pin in the editor.
   */
  doc: string
}

export interface FbDef {
  /** IEC type name — exactly what goes in `inst : <type>;`. */
  type: string
  /** Short human label for the picker (e.g. "On-delay timer"). */
  label: string
  /** Picker category — groups related FBs together. `project` is for
   *  user-defined FBs discovered in the open project (the library). */
  category: "timer" | "counter" | "edge" | "bistable" | "project"
  /** Two-line description for the picker. */
  description: string
  /** All pins, in render order (top-to-bottom on each side). */
  pins: FbPin[]
  /**
   * Suggested instance name prefix — the editor auto-numbers from
   * this (`myT1`, `myT2`, ...). Lowercase identifier.
   */
  instancePrefix: string
  /** Imported-library name when the FB lives under `pous/lib/<name>/`
   *  (palette groups by it); undefined for builtins and the project's
   *  own FBs. */
  library?: string
}

/* ---------------------------------------------------------------- */
/* Timer family — all share the (IN, PT) → (Q, ET) signature.       */
/* See vendor/ironplc/compiler/vm/src/intrinsic.rs::ton/tof/tp.     */
/* ---------------------------------------------------------------- */

const TIMER_PINS: FbPin[] = [
  { pin: "IN", direction: "input", type: "BOOL", doc: "Trigger input" },
  { pin: "PT", direction: "input", type: "TIME", doc: "Preset time (e.g. T#3s)" },
  { pin: "Q", direction: "output", type: "BOOL", doc: "Timer output" },
  { pin: "ET", direction: "output", type: "TIME", doc: "Elapsed time" },
]

/* ---------------------------------------------------------------- */
/* Edge detectors — (CLK) → (Q).                                    */
/* ---------------------------------------------------------------- */

const EDGE_PINS: FbPin[] = [
  { pin: "CLK", direction: "input", type: "BOOL", doc: "Clock input" },
  { pin: "Q", direction: "output", type: "BOOL", doc: "Edge pulse (one scan)" },
]

/* ---------------------------------------------------------------- */
/* Standard FB table — keep in sync with intrinsic.rs.              */
/* Order chosen for the picker UI (timers first, most common to     */
/* least common within each category).                              */
/* ---------------------------------------------------------------- */

export const STANDARD_FBS: FbDef[] = [
  {
    type: "TON",
    label: "TON — On-delay timer",
    category: "timer",
    description:
      "Q goes TRUE after IN has been TRUE continuously for PT. Falling IN immediately resets.",
    pins: TIMER_PINS,
    instancePrefix: "myT",
  },
  {
    type: "TOF",
    label: "TOF — Off-delay timer",
    category: "timer",
    description:
      "Q follows IN, but stays TRUE for PT after IN goes FALSE. Rising IN during timing aborts the delay.",
    pins: TIMER_PINS,
    instancePrefix: "myTof",
  },
  {
    type: "TP",
    label: "TP — Pulse timer",
    category: "timer",
    description:
      "Rising IN starts a pulse of length PT. IN changes during the pulse are ignored — the pulse runs to completion.",
    pins: TIMER_PINS,
    instancePrefix: "myPulse",
  },

  {
    type: "CTU",
    label: "CTU — Up counter",
    category: "counter",
    description:
      "CV increments on each rising CU. Q is TRUE when CV >= PV. R = TRUE forces CV to 0.",
    pins: [
      { pin: "CU", direction: "input", type: "BOOL", doc: "Count up trigger" },
      { pin: "R", direction: "input", type: "BOOL", doc: "Reset to 0" },
      { pin: "PV", direction: "input", type: "INT", doc: "Preset / target value" },
      { pin: "Q", direction: "output", type: "BOOL", doc: "TRUE when CV ≥ PV" },
      { pin: "CV", direction: "output", type: "INT", doc: "Current count" },
    ],
    instancePrefix: "myCnt",
  },
  {
    type: "CTD",
    label: "CTD — Down counter",
    category: "counter",
    description:
      "CV decrements on each rising CD. Q is TRUE when CV <= 0. LD = TRUE forces CV to PV.",
    pins: [
      { pin: "CD", direction: "input", type: "BOOL", doc: "Count down trigger" },
      { pin: "LD", direction: "input", type: "BOOL", doc: "Load CV := PV" },
      { pin: "PV", direction: "input", type: "INT", doc: "Initial value" },
      { pin: "Q", direction: "output", type: "BOOL", doc: "TRUE when CV ≤ 0" },
      { pin: "CV", direction: "output", type: "INT", doc: "Current count" },
    ],
    instancePrefix: "myDown",
  },
  {
    type: "CTUD",
    label: "CTUD — Up/down counter",
    category: "counter",
    description:
      "Combined up and down counter with separate triggers. QU = TRUE when CV ≥ PV, QD = TRUE when CV ≤ 0. R resets, LD loads.",
    pins: [
      { pin: "CU", direction: "input", type: "BOOL", doc: "Count up trigger" },
      { pin: "CD", direction: "input", type: "BOOL", doc: "Count down trigger" },
      { pin: "R", direction: "input", type: "BOOL", doc: "Reset to 0" },
      { pin: "LD", direction: "input", type: "BOOL", doc: "Load CV := PV" },
      { pin: "PV", direction: "input", type: "INT", doc: "Preset value" },
      { pin: "QU", direction: "output", type: "BOOL", doc: "TRUE when CV ≥ PV" },
      { pin: "QD", direction: "output", type: "BOOL", doc: "TRUE when CV ≤ 0" },
      { pin: "CV", direction: "output", type: "INT", doc: "Current count" },
    ],
    instancePrefix: "myUd",
  },

  {
    type: "R_TRIG",
    label: "R_TRIG — Rising edge",
    category: "edge",
    description:
      "Q is TRUE for one scan when CLK transitions FALSE → TRUE. Useful for one-shot triggers.",
    pins: EDGE_PINS,
    instancePrefix: "myEdge",
  },
  {
    type: "F_TRIG",
    label: "F_TRIG — Falling edge",
    category: "edge",
    description:
      "Q is TRUE for one scan when CLK transitions TRUE → FALSE.",
    pins: EDGE_PINS,
    instancePrefix: "myFall",
  },

  {
    type: "SR",
    label: "SR — Set-dominant latch",
    category: "bistable",
    description:
      "Q1 := S1 OR (NOT R AND Q1). If both S1 and R are TRUE, set wins — Q1 stays TRUE.",
    pins: [
      { pin: "S1", direction: "input", type: "BOOL", doc: "Set (dominant)" },
      { pin: "R", direction: "input", type: "BOOL", doc: "Reset" },
      { pin: "Q1", direction: "output", type: "BOOL", doc: "Latched output" },
    ],
    instancePrefix: "mySr",
  },
  {
    type: "RS",
    label: "RS — Reset-dominant latch",
    category: "bistable",
    description:
      "Q1 := NOT R1 AND (S OR Q1). If both S and R1 are TRUE, reset wins — Q1 stays FALSE.",
    pins: [
      { pin: "S", direction: "input", type: "BOOL", doc: "Set" },
      { pin: "R1", direction: "input", type: "BOOL", doc: "Reset (dominant)" },
      { pin: "Q1", direction: "output", type: "BOOL", doc: "Latched output" },
    ],
    instancePrefix: "myRs",
  },
]

/* ---------------------------------------------------------------- */
/* Project FB registry — user-defined FUNCTION_BLOCKs discovered in  */
/* the open project (e.g. the process-control library once imported).*/
/* Set once when the project tree loads (see runtime state); the     */
/* graphical editors then offer them in the block palette alongside  */
/* the builtins, with pins derived from each FB's VAR_INPUT /         */
/* VAR_OUTPUT. Module-level so the existing pure lookup helpers       */
/* resolve them without threading a catalogue through every caller.  */
/* ---------------------------------------------------------------- */

let PROJECT_FBS: FbDef[] = []

/** Replace the project FB registry. Builtins always win on a name
 *  clash (a project must not shadow TON, etc.). */
export function setProjectFbs(fbs: FbDef[]): void {
  const builtinNames = new Set(STANDARD_FBS.map((f) => f.type))
  PROJECT_FBS = fbs.filter((f) => !builtinNames.has(f.type))
}

/** Palette grouping for the block pickers: builtins under "Standard",
 *  the project's own FBs under "Project blocks", and each imported
 *  library under its own name. Groups with no members are omitted. */
export function groupedFbs(): Array<{ label: string; fbs: FbDef[] }> {
  const groups: Array<{ label: string; fbs: FbDef[] }> = [
    { label: "Standard", fbs: [...STANDARD_FBS] },
  ]
  const own = PROJECT_FBS.filter((f) => !f.library)
  if (own.length > 0) groups.push({ label: "Project blocks", fbs: own })
  const byLibrary = new Map<string, FbDef[]>()
  for (const f of PROJECT_FBS) {
    if (!f.library) continue
    if (!byLibrary.has(f.library)) byLibrary.set(f.library, [])
    byLibrary.get(f.library)!.push(f)
  }
  for (const [library, fbs] of [...byLibrary.entries()].sort(([a], [b]) =>
    a.localeCompare(b),
  )) {
    groups.push({ label: library, fbs })
  }
  return groups
}

/** Build FbDefs from project POUs. `fbPous` is the FUNCTION_BLOCK
 *  declarations (name + the file's slug); `pinsFor` yields each one's
 *  pins from its parsed VAR_INPUT / VAR_OUTPUT. FBs with no pins yet
 *  (failed parse / empty skeleton) are still listed so they can be
 *  placed. FBs under `lib/<library>/` carry the library name so the
 *  palette can group them. */
export function buildProjectFbDefs(
  fbPous: { type: string; path?: string }[],
  pinsFor: (type: string) => FbPin[],
): FbDef[] {
  return fbPous.map((p) => {
    const library = p.path?.startsWith("lib/")
      ? p.path.split("/")[1]
      : undefined
    return {
      type: p.type,
      label: library
        ? `${p.type} — ${library}`
        : `${p.type} — project block`,
      category: "project" as const,
      description: library
        ? `Function block from the ${library} library.`
        : "User-defined function block from this project.",
      pins: pinsFor(p.type),
      instancePrefix: p.type.replace(/^FB_/i, "").toLowerCase() || "fb",
      library,
    }
  })
}

/** One-line `in1, in2 → out1, out2` pin summary for palette tooltips,
 *  so you can see a block's interface before placing it. */
export function fbPinHint(fb: FbDef): string {
  const ins = fb.pins.filter((p) => p.direction === "input").map((p) => p.pin)
  const outs = fb.pins.filter((p) => p.direction === "output").map((p) => p.pin)
  const inStr = ins.join(", ") || "—"
  return outs.length ? `${inStr} → ${outs.join(", ")}` : inStr
}

/** Look up an FB by its IEC type name (builtin or project). Case-sensitive. */
export function fbByType(type: string): FbDef | undefined {
  return STANDARD_FBS.find((fb) => fb.type === type) ?? PROJECT_FBS.find((fb) => fb.type === type)
}

/** All BOOL output pin names for the given FB type, used by the
 *  output-pin selector in the editor. Falls back to `["Q"]` for
 *  unknown types so user-defined FBs still get a sensible default. */
export function fbBoolOutputs(type: string): string[] {
  const fb = fbByType(type)
  if (!fb) return ["Q"]
  return fb.pins.filter((p) => p.direction === "output" && p.type === "BOOL").map((p) => p.pin)
}

/** All input pins for the given FB type. */
export function fbInputs(type: string): FbPin[] {
  const fb = fbByType(type)
  if (!fb) return []
  return fb.pins.filter((p) => p.direction === "input")
}

/** All output pins for the given FB type. */
export function fbOutputs(type: string): FbPin[] {
  const fb = fbByType(type)
  if (!fb) return []
  return fb.pins.filter((p) => p.direction === "output")
}

/**
 * Suggest a fresh instance name for an FB type, given the names already
 * in use across the POU — its FB instances AND its variables, since the
 * transpiler declares every instance next to them. We always return
 * `prefix + N` for the smallest N ≥ 1 that doesn't collide, comparing
 * case-insensitively as IEC 61131-3 does (`MYT1` is taken by `myT1`).
 */
export function suggestInstanceName(type: string, used: Iterable<string>): string {
  const fb = fbByType(type)
  const prefix = fb?.instancePrefix ?? "myFb"
  const taken = new Set(Array.from(used, (name) => name.toLowerCase()))
  let n = 1
  // eslint-disable-next-line no-constant-condition
  while (true) {
    const candidate = `${prefix}${n}`
    if (!taken.has(candidate.toLowerCase())) return candidate
    n += 1
  }
}
