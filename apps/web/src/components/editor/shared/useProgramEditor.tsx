/**
 * Shared editor shell for the three graphical IEC-61131 editors.
 *
 * LD, FBD and SFC each opened with the same skeleton, differing only in
 * the language literal and their own `safeParse` / `serialize`:
 *
 *   - a `useMemo` that parses the JSON source into a typed program,
 *   - a 350 ms-debounced poll of the `check` endpoint for diagnostics,
 *   - the live values that belong to THIS program (`online`), and
 *   - a `commit(next)` that serialises and calls `onChange`.
 *
 * `useProgramEditor` owns that skeleton. Each editor supplies its own
 * `parse` (the language-specific validation / normalisation) and
 * `serialize`, then derives its own online-mode overlay from the
 * returned `online` lookup and renders `<ParseErrorView>` on a parse
 * error. `online` is scoped to the open POU (see `lib/online-vars.ts`):
 * reading the raw snapshot instead would colour this diagram with
 * whichever program happens to be running.
 *
 * `parse` must be a stable reference (a module-level function) so the
 * parse memo only re-runs when `value` changes.
 */

import { useEffect, useMemo, useState } from "react"

import { checkProgram } from "@/lib/api"
import { onlineScope, onlineVars, type OnlineVars } from "@/lib/online-vars"
import { cn } from "@/lib/utils"
import { useLastSnapshot } from "@/state/live-feed"
import { useRuntime } from "@/state/runtime"
import type { CheckDiagnostic } from "@/types/generated/CheckDiagnostic"
import type { LdPouType } from "@/types/generated/LdPouType"

/** Result of an editor's `safeParse`: a typed program or a human error. */
export type ParseResult<P> =
  | { kind: "ok"; program: P }
  | { kind: "error"; message: string }

/** Everything the shell owns, handed back to the editor. */
export interface ProgramEditorShell<P> {
  parsed: ParseResult<P>
  diagnostics: CheckDiagnostic[]
  /** This program's live values, or `null` when none are on the wire —
   *  nothing running, a different program running, a FUNCTION_BLOCK, or a
   *  program scheduled more than once. */
  online: OnlineVars | null
  /** Serialise + `onChange`, unless the editor is read-only. */
  commit: (next: P) => void
}

export function useProgramEditor<P extends { name: string; pou_type: LdPouType }>(opts: {
  value: string
  onChange: (next: string) => void
  readOnly: boolean
  /** Store slug this buffer came from — keeps the project-aware check
   *  from double-counting the on-disk copy. */
  path: string | undefined
  language: "ld" | "fbd" | "sfc"
  /** Stable (module-level) parser for this language's source. */
  parse: (value: string) => ParseResult<P>
  serialize: (program: P) => string
}): ProgramEditorShell<P> {
  const { value, onChange, readOnly, path, language, parse, serialize } = opts

  const parsed = useMemo(() => parse(value), [value, parse])

  // Live runtime reads — drive each editor's online-mode overlay.
  const { isRunning, running, tasks, projectEpoch } = useRuntime()
  const lastSnapshot = useLastSnapshot()
  const program = parsed.kind === "ok" ? parsed.program : null
  const scope = useMemo(
    () =>
      isRunning
        ? onlineScope({
            running,
            tasks,
            path,
            program: program && { name: program.name, pou_type: program.pou_type },
          })
        : null,
    [isRunning, running, tasks, path, program?.name, program?.pou_type],
  )
  const online = useMemo(() => onlineVars(lastSnapshot, scope), [lastSnapshot, scope])

  // Diagnostics — 350 ms debounced poll of the HTTP `check` endpoint.
  // ironplc's LSP doesn't speak graphical JSON, so we re-check whenever
  // the source settles. Long enough to skip mid-typing keystrokes,
  // short enough that the squiggle appears promptly.
  const [diagnostics, setDiagnostics] = useState<CheckDiagnostic[]>([])
  useEffect(() => {
    if (parsed.kind === "error") {
      // JSON itself broken — `check` would just bounce, and the editor
      // already shows the parse error in-pane.
      setDiagnostics([])
      return
    }
    const handle = setTimeout(async () => {
      try {
        setDiagnostics(await checkProgram(value, language, path))
      } catch (e) {
        // Network errors aren't program errors — keep the last good
        // snapshot; logging is enough.
        console.warn(`${language} diagnostics fetch failed:`, e)
      }
    }, 350)
    return () => clearTimeout(handle)
    // projectEpoch: a library import/remove can (un)resolve this POU's
    // FB references without the buffer changing — re-check.
  }, [value, parsed.kind, path, projectEpoch, language])

  const commit = (next: P) => {
    if (readOnly) return
    onChange(serialize(next))
  }

  return { parsed, diagnostics, online, commit }
}

/** The parse-error fallback all three editors rendered identically
 *  (bar the language label): a red banner plus the raw JSON, so the
 *  operator can still see and hand-fix the broken source. */
export function ParseErrorView({
  label,
  message,
  source,
  className,
}: {
  label: string
  message: string
  source: string
  className?: string
}) {
  return (
    <div className={cn("flex h-full min-h-0 flex-col", className)}>
      <div className="border-b border-destructive/40 bg-destructive/5 px-3 py-2 text-xs text-destructive">
        {label} JSON parse error: {message}
      </div>
      <pre className="flex-1 overflow-auto bg-muted/20 px-4 py-3 font-mono text-xs leading-relaxed text-foreground">
        {source}
      </pre>
    </div>
  )
}
