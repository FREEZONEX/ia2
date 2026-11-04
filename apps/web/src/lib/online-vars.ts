/**
 * Which live values belong to the program open in a graphical editor.
 *
 * The snapshot merges every running program. Names stay bare unless two
 * programs declare the same one, in which case they arrive as
 * `instance.name`. The editors used to look names up bare, in whatever
 * snapshot was live, which went wrong three ways:
 *
 *   - a POU that was not running at all was coloured with the running
 *     program's same-named variables (`start`, `run`, `estop`…);
 *   - with several programs scheduled, every shared name was missing — and
 *     a missing BOOL read as FALSE, so an NC contact on a pressed e-stop
 *     showed as closed and its rung as live;
 *   - FB outputs (`t.Q`) are never in the snapshot (the container exposes
 *     the instance, not its fields), so every rung or FBD wire driven by a
 *     timer, counter or edge detector showed as dead, permanently.
 *
 * Everything here answers "unknown" rather than guessing: an editor with no
 * scope shows no live state, and a name that is not on the wire is `null`,
 * never FALSE or 0.
 */

import type { RunningInfo } from "@/state/runtime"
import type { LdPouType } from "@/types/generated/LdPouType"
import type { Tasks } from "@/types/generated/Tasks"
import type { VarSnapshot } from "@/types/generated/VarSnapshot"
import type { VarValue } from "@/types/generated/VarValue"

/** Where the open POU's variables sit in the snapshot: under `instance.`
 *  when set, bare otherwise (a single-program run). */
export type OnlineScope = { instance: string | null }

const same = (a: string, b: string) => a.toLowerCase() === b.toLowerCase()

/** The scope of the POU open in an editor, or `null` when its values are
 *  not in the live snapshot — not running, a FUNCTION_BLOCK (instances
 *  live inside programs and their fields are not published), or scheduled
 *  more than once (which instance to show would be a guess). */
export function onlineScope(args: {
  running: RunningInfo
  tasks: Tasks
  /** Store slug the editor's buffer came from. */
  path: string | undefined
  program: { name: string; pou_type: LdPouType } | null
}): OnlineScope | null {
  const { running, tasks, path, program } = args
  if (!running || !program || program.pou_type !== "program") return null
  if (running.kind === "isolated") {
    if (!same(running.program, program.name)) return null
    // `run(program)` without a file records the program name as the path;
    // with a file, another file may declare a PROGRAM of the same name.
    if (running.filePath !== running.program && running.filePath !== path) return null
    return { instance: null }
  }
  if (running.kind === "scheduled" && !running.programs.some((p) => same(p, program.name))) {
    return null
  }
  // Scheduled here, or on the attached edge (which runs this project's schedule).
  const instances = tasks.programs.filter((p) => same(p.program, program.name))
  if (instances.length !== 1) return null
  return { instance: instances[0].instance }
}

/** A lookup into the snapshot for one POU. Case-insensitive, as IEC names are. */
export type OnlineVars = (name: string) => VarValue | undefined

export function onlineVars(snapshot: VarSnapshot | null, scope: OnlineScope | null): OnlineVars | null {
  if (!snapshot || !scope) return null
  const byName = new Map<string, VarValue>()
  for (const v of snapshot.vars) byName.set(v.name.toLowerCase(), v)
  const prefix = scope.instance ? `${scope.instance.toLowerCase()}.` : null
  return (name) => {
    const key = name.toLowerCase()
    // A name this POU declares is either qualified with its instance (when
    // another program shares it) or bare (when it is unique — and therefore
    // this program's).
    const found = (prefix ? byName.get(prefix + key) : undefined) ?? byName.get(key)
    return found?.input?.stale ? undefined : found
  }
}

export function onlineBool(vars: OnlineVars, name: string): boolean | null {
  const v = vars(name)
  if (!v || v.type_name !== "BOOL") return null
  return v.value === "TRUE"
}

/** Numeric reading of a live value. The bridge ships every numeric / time
 *  type as text (`3.14`, `42`, `T#100ms`); best effort, `null` when there
 *  is no number to read. */
export function onlineNumber(vars: OnlineVars, name: string): number | null {
  const v = vars(name)
  if (!v) return null
  const m = v.value.match(/-?\d+(?:\.\d+)?/)
  if (!m) return null
  const n = parseFloat(m[0])
  return Number.isFinite(n) ? n : null
}

/** How the snapshot shows the value the runtime stores for `text` in a
 *  variable of type `typeName`, as ironplc's `VariableRenderer` prints it:
 *  a STRING is single-quoted Latin-1 bytes, a WSTRING double-quoted UTF-16
 *  code units; printable ASCII passes through and everything else is
 *  `$`-escaped. So `café` in a STRING arrives as `'caf$E9'` and `加料` in a
 *  WSTRING as `"$52A0$6599"`. A STRING keeps each character's low byte —
 *  how older runtimes stored non-Latin-1 text (`等待` as `'I$85'`); current
 *  ones refuse such a literal. */
export function runtimeStringLiteral(text: string, typeName = "STRING"): string {
  const wide = /^\s*WSTRING\b/i.test(typeName)
  const quote = wide ? '"' : "'"
  let out = quote
  // A WSTRING is escaped per UTF-16 code unit (so a surrogate pair is two
  // escapes); a STRING per character, of which it keeps the low byte.
  const units = wide
    ? Array.from({ length: text.length }, (_, i) => text.charCodeAt(i))
    : Array.from(text, (ch) => (ch.codePointAt(0) ?? 0) & 0xff)
  for (const u of units) {
    if (u === 0x24) out += "$$"
    else if (u === quote.charCodeAt(0)) out += `$${quote}`
    else if (u === 0x09) out += "$T"
    else if (u === 0x0a) out += "$L"
    else if (u === 0x0c) out += "$P"
    else if (u === 0x0d) out += "$R"
    else if (u >= 0x20 && u <= 0x7e) out += String.fromCharCode(u)
    else out += `$${u.toString(16).toUpperCase().padStart(wide ? 4 : 2, "0")}`
  }
  return `${out}${quote}`
}
