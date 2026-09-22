/**
 * Pure binding-resolution helpers for the HMI canvas.
 *
 * A binding names a program variable; the canvas resolves it against the
 * live VarSnapshot with the Monitor's rules (exact name first, then the
 * bare tail of `instance.variable`), optionally pipes it through a tiny
 * single-variable expression, then formats it for display.
 *
 * The expression language is deliberately not-Turing: one input `x`,
 * numeric literals, + - * / % parentheses, comparisons and ternary. No
 * identifiers besides `x`, no calls, no assignment — cross-variable logic
 * belongs in a POU, and nothing here can loop or allocate.
 */

import type { HmiBinding } from "@/types/generated/HmiBinding"
import type { HmiMapEntry } from "@/types/generated/HmiMapEntry"
import type { VarSnapshot } from "@/types/generated/VarSnapshot"
import type { VarValue } from "@/types/generated/VarValue"

export function bindingVariable(b: HmiBinding): string {
  return typeof b === "string" ? b : b.variable
}

/** Resolve a variable name against the snapshot: exact match, else match
 *  on the tail after the last `.` (multi-PROGRAM `instance.variable`) —
 *  but ONLY when that tail is unambiguous. Two programs both owning a
 *  `level` must show "—", not whichever happened to serialize first;
 *  `cs hmi check` flags the ambiguity so it gets qualified, not guessed. */
export function snapshotVariable(
  snapshot: VarSnapshot | null,
  name: string,
): VarValue | null {
  if (!snapshot) return null
  const exact = snapshot.vars.find((v) => v.name === name)
  if (exact) return exact
  const tail = name.split(".").pop() ?? name
  const hits = snapshot.vars.filter(
    (v) => (v.name.split(".").pop() ?? v.name) === tail,
  )
  return hits.length === 1 ? hits[0] : null
}

/** Resolve identity BEFORE quality: filtering stale candidates first
 * would turn an ambiguous name into the wrong healthy program's value. */
export function lookupVar(
  snapshot: VarSnapshot | null,
  name: string,
): { raw: string; type_name: string } | null {
  const found = snapshotVariable(snapshot, name)
  return found && !found.input?.stale
    ? { raw: found.value, type_name: found.type_name }
    : null
}

/** Snapshot values arrive as display strings ("TRUE", "42", "3.14").
 *  Collapse to a number: booleans become 0/1, unparseable becomes NaN. */
export function toNumber(raw: string): number {
  const t = raw.trim()
  if (/^true$/i.test(t)) return 1
  if (/^false$/i.test(t)) return 0
  const n = Number(t)
  return Number.isFinite(n) ? n : NaN
}

/** Everything a binding can produce: the post-expr number (null when the
 *  variable is unresolved, ambiguous, non-numeric, or the expr failed /
 *  overflowed — never NaN/Infinity), the first matching map output, and
 *  the raw snapshot text + type (for `%s`, STRING and BOOL display). */
export type ResolvedOutput = {
  num: number | null
  out: string | null
  raw: string | null
  type_name: string | null
}

export function resolveOutput(
  snapshot: VarSnapshot | null,
  b: HmiBinding,
): ResolvedOutput {
  const found = lookupVar(snapshot, bindingVariable(b))
  if (!found) return { num: null, out: null, raw: null, type_name: null }
  let value = toNumber(found.raw)
  const spec = typeof b === "string" ? null : b
  if (spec?.expr && Number.isFinite(value)) {
    const v = evalExpr(spec.expr, value)
    value = v === null ? NaN : v
  }
  if (!Number.isFinite(value)) {
    // STRING variables, parse failures, division blow-ups: no number to
    // format or map — callers show the raw text or an em-dash.
    return { num: null, out: null, raw: found.raw, type_name: found.type_name }
  }
  const out = spec?.map ? applyMap(spec.map, value) : null
  return { num: value, out, raw: found.raw, type_name: found.type_name }
}

/** Resolve a full binding to its numeric value (post-expr). */
export function resolveBinding(
  snapshot: VarSnapshot | null,
  b: HmiBinding,
): number | null {
  return resolveOutput(snapshot, b).num
}

/** On/off resolution for state-carrying binds (indicator `on`, a
 *  button's `bind.on`): nonzero = on, unresolved = off — an unknown
 *  variable must read as the calm state, never a lit one. (Contrast
 *  `visible`, whose unresolved default is SHOWN.) */
export function resolveOn(snapshot: VarSnapshot | null, b: HmiBinding): boolean {
  return (resolveBinding(snapshot, b) ?? 0) !== 0
}

/** First matching map entry's output. `eq` matches exactly; otherwise
 *  `[min, max)` with either side optional; a condition-less entry is the
 *  catch-all — order the list accordingly. */
export function applyMap(entries: HmiMapEntry[], value: number): string | null {
  for (const e of entries) {
    if (e.eq != null) {
      if (value === e.eq) return e.out
      continue
    }
    if (e.min != null && value < e.min) continue
    if (e.max != null && value >= e.max) continue
    return e.out
  }
  return null
}

/** Strip the ST string-literal quotes from a raw snapshot value. */
function stripQuotes(raw: string): string {
  const quoted = /^'(.*)'$/.exec(raw.trim())
  return quoted ? quoted[1] : raw.trim()
}

/** Display text for a binding, in priority order: map output; `%s` (raw
 *  snapshot text, quotes stripped); explicit numeric format; BOOLs as
 *  TRUE/FALSE; non-numeric variables (STRING content, `16#…` bit-field
 *  literals) verbatim; then the compact numeric default. Null = "—"
 *  (unresolved variable, or an expr that failed / blew up — an expr
 *  declares numeric intent, so its failures never fall back to text). */
export function displayBinding(
  snapshot: VarSnapshot | null,
  b: HmiBinding,
): string | null {
  const r = resolveOutput(snapshot, b)
  if (r.out !== null) return r.out
  if (r.raw === null) return null
  const spec = typeof b === "string" ? null : b
  const fmt = spec?.format ?? null
  if (fmt === "%s") return stripQuotes(r.raw)
  if (r.num === null) {
    return spec?.expr ? null : stripQuotes(r.raw)
  }
  if (!fmt && r.type_name?.toUpperCase() === "BOOL") {
    return r.raw.trim().toUpperCase()
  }
  return formatBinding(b, r.num)
}

/** Color for a color-class prop (`color`, `fill`, `stroke`): only a map
 *  can produce one — a bare number is not a color. Null = default style. */
export function colorBinding(
  snapshot: VarSnapshot | null,
  b: HmiBinding,
): string | null {
  return resolveOutput(snapshot, b).out
}

/** Named state-color tokens → the design system's CSS variables; any
 *  other string passes through as a literal CSS color. Screens SHOULD
 *  speak tokens (they follow theme changes); literals are the escape
 *  hatch for brand/one-off needs. */
const COLOR_TOKENS: Record<string, string> = {
  ok: "var(--highlight)",
  warn: "var(--warn)",
  alarm: "var(--destructive)",
  info: "var(--trend)",
  muted: "var(--muted-foreground)",
  fg: "var(--foreground)",
  agent: "var(--agent)",
}

export function cssColor(c: string): string {
  return COLOR_TOKENS[c] ?? c
}

/** Format per the binding's printf-ish `format` (%.2f, %d, %s), falling
 *  back to a compact default. */
export function formatBinding(b: HmiBinding, value: number): string {
  // Never print "NaN"/"Infinity" on an operator screen.
  if (!Number.isFinite(value)) return "—"
  const fmt = typeof b === "string" ? null : (b.format ?? null)
  if (fmt) {
    const m = /^%(?:\.(\d+))?([dfs])$/.exec(fmt)
    if (m) {
      const [, prec, kind] = m
      if (kind === "d") return String(Math.round(value))
      if (kind === "f") return value.toFixed(prec ? Number(prec) : 2)
      return String(value)
    }
  }
  if (Number.isInteger(value)) return String(value)
  return value.toFixed(2)
}

// ============================================================
//  Expression evaluator — recursive descent, no eval()
// ============================================================

type Tok =
  | { k: "num"; v: number }
  | { k: "x" }
  | { k: "op"; v: string }

function lex(src: string): Tok[] | null {
  const out: Tok[] = []
  let i = 0
  const ops = ["<=", ">=", "==", "!=", "&&", "||", "<", ">", "+", "-", "*", "/", "%", "(", ")", "?", ":", "!"]
  while (i < src.length) {
    const c = src[i]
    if (c === " " || c === "\t") {
      i++
      continue
    }
    if (/[0-9.]/.test(c)) {
      let j = i
      while (j < src.length && /[0-9.]/.test(src[j])) j++
      const n = Number(src.slice(i, j))
      if (!Number.isFinite(n)) return null
      out.push({ k: "num", v: n })
      i = j
      continue
    }
    if (c === "x" && !/[a-zA-Z0-9_]/.test(src[i + 1] ?? "")) {
      out.push({ k: "x" })
      i++
      continue
    }
    const op = ops.find((o) => src.startsWith(o, i))
    if (!op) return null
    out.push({ k: "op", v: op })
    i += op.length
  }
  return out
}

/** Evaluate `expr` with `x` bound. Returns null on any parse error —
 *  callers render an em-dash rather than a wrong number. */
export function evalExpr(expr: string, x: number): number | null {
  const toks = lex(expr)
  if (!toks) return null
  let pos = 0
  const peek = () => toks[pos]
  const eat = (v?: string): Tok | null => {
    const t = toks[pos]
    if (!t) return null
    if (v !== undefined && !(t.k === "op" && t.v === v)) return null
    pos++
    return t
  }

  // ternary → or → and → cmp → add → mul → unary → atom
  function ternary(): number | null {
    const cond = or()
    if (cond === null) return null
    if (peek()?.k === "op" && (peek() as { v: string }).v === "?") {
      eat("?")
      const a = ternary()
      if (a === null || !eat(":")) return null
      const b = ternary()
      if (b === null) return null
      return cond !== 0 ? a : b
    }
    return cond
  }
  function or(): number | null {
    let l = and()
    if (l === null) return null
    while (peek()?.k === "op" && (peek() as { v: string }).v === "||") {
      eat("||")
      const r = and()
      if (r === null) return null
      l = l !== 0 || r !== 0 ? 1 : 0
    }
    return l
  }
  function and(): number | null {
    let l = cmp()
    if (l === null) return null
    while (peek()?.k === "op" && (peek() as { v: string }).v === "&&") {
      eat("&&")
      const r = cmp()
      if (r === null) return null
      l = l !== 0 && r !== 0 ? 1 : 0
    }
    return l
  }
  function cmp(): number | null {
    let l = add()
    if (l === null) return null
    for (;;) {
      const t = peek()
      if (t?.k !== "op" || !["<", ">", "<=", ">=", "==", "!="].includes(t.v)) return l
      eat(t.v)
      const r = add()
      if (r === null) return null
      const res: boolean =
        t.v === "<" ? l < r
        : t.v === ">" ? l > r
        : t.v === "<=" ? l <= r
        : t.v === ">=" ? l >= r
        : t.v === "==" ? l === r
        : l !== r
      l = res ? 1 : 0
    }
  }
  function add(): number | null {
    let l = mul()
    if (l === null) return null
    for (;;) {
      const t = peek()
      if (t?.k !== "op" || (t.v !== "+" && t.v !== "-")) return l
      eat(t.v)
      const r = mul()
      if (r === null) return null
      l = t.v === "+" ? l + r : l - r
    }
  }
  function mul(): number | null {
    let l = unary()
    if (l === null) return null
    for (;;) {
      const t = peek()
      if (t?.k !== "op" || !["*", "/", "%"].includes(t.v)) return l
      eat(t.v)
      const r = unary()
      if (r === null) return null
      l = t.v === "*" ? l * r : t.v === "/" ? l / r : l % r
    }
  }
  function unary(): number | null {
    const t = peek()
    if (t?.k === "op" && t.v === "-") {
      eat("-")
      const v = unary()
      return v === null ? null : -v
    }
    if (t?.k === "op" && t.v === "!") {
      eat("!")
      const v = unary()
      return v === null ? null : v === 0 ? 1 : 0
    }
    return atom()
  }
  function atom(): number | null {
    const t = peek()
    if (!t) return null
    if (t.k === "num") {
      pos++
      return t.v
    }
    if (t.k === "x") {
      pos++
      return x
    }
    if (t.k === "op" && t.v === "(") {
      eat("(")
      const v = ternary()
      if (v === null || !eat(")")) return null
      return v
    }
    return null
  }

  const result = ternary()
  return pos === toks.length ? result : null
}
