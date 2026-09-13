import "./monaco-local"
import Editor, { type Monaco, type OnMount } from "@monaco-editor/react"
import type { editor } from "monaco-editor"
import { useCallback, useEffect, useRef } from "react"

import { fetchSymbols } from "@/lib/api"
import { useDarkMode } from "@/lib/dark-mode"
import { groupedFbs } from "@/lib/ld-fbs"
import type { CheckDiagnostic } from "@/types/generated/CheckDiagnostic"
import type { VariableInfo } from "@/types/generated/VariableInfo"
import { ACID_DARK, ACID_LIGHT, defineAcidThemes } from "./acid-theme"
import {
  LANGUAGE_ID,
  builtins,
  editorOptions,
  keywords,
  languageConfiguration,
  monarch,
  typeKeywords,
} from "./iec61131-language"

let languageRegistered = false

type Props = {
  value: string
  onChange: (value: string) => void
  diagnostics: CheckDiagnostic[]
  /** Library blocks open read-only — the server rejects writes under
   *  `pous/lib/**`, so the editor shouldn't accept them either. */
  readOnly?: boolean
  /** Bumps when an agent-driven update replaced the content (see
   *  `pouSpawnStore`). Plays the staggered per-line reveal. */
  spawnTick?: number
}

export function STEditor({
  value,
  onChange,
  diagnostics,
  readOnly = false,
  spawnTick = 0,
}: Props) {
  const editorRef = useRef<editor.IStandaloneCodeEditor | null>(null)
  const monacoRef = useRef<Monaco | null>(null)
  const dark = useDarkMode()
  const theme = dark === "dark" ? ACID_DARK : ACID_LIGHT

  // Live symbol table — repopulated whenever the source settles (350ms
  // debounce, same cadence as our diagnostics poll). The completion
  // and hover providers read this via a ref so they don't need React
  // re-renders to pick up new variables.
  const symbolsRef = useRef<VariableInfo[]>([])

  const handleMount: OnMount = useCallback((editorInstance, monaco) => {
    editorRef.current = editorInstance
    monacoRef.current = monaco
    if (languageRegistered) return
    monaco.languages.register({ id: LANGUAGE_ID })
    monaco.languages.setMonarchTokensProvider(LANGUAGE_ID, monarch)
    monaco.languages.setLanguageConfiguration(LANGUAGE_ID, languageConfiguration)

    const Kind = monaco.languages.CompletionItemKind
    const detailKeyword = "keyword"
    const detailType = "IEC type"
    const detailBuiltin = "stdlib"
    monaco.languages.registerCompletionItemProvider(LANGUAGE_ID, {
      triggerCharacters: [],
      provideCompletionItems(model: editor.ITextModel, position: { lineNumber: number; column: number }) {
        const word = model.getWordUntilPosition(position)
        const range = {
          startLineNumber: position.lineNumber,
          endLineNumber: position.lineNumber,
          startColumn: word.startColumn,
          endColumn: word.endColumn,
        }
        const make = (
          label: string,
          kind: number,
          detail: string,
        ) => ({ label, kind, insertText: label, detail, range })
        // Variables / FB instances — extracted from the current
        // source by the bridge. ironplc doesn't advertise a
        // completionProvider, but our HTTP /api/symbols extraction
        // gives equivalent coverage for the cases that matter
        // (variables + FB instances declared in the file).
        const varSuggestions = symbolsRef.current.map((s) => {
          const kind =
            s.direction === "fb_instance"
              ? Kind.Class
              : s.type_name.toUpperCase().startsWith("BOOL")
                ? Kind.Variable
                : Kind.Variable
          return make(s.name, kind, `${s.direction} : ${s.type_name}`)
        })
        // Library / project FUNCTION_BLOCK types — so writing
        // `inst : FB_P…` completes to FB_PID with the block's one-line
        // doc. Resolved live (the registry fills in as the project tree
        // loads), Standard builtins already come through `builtins`.
        const fbTypeSuggestions = groupedFbs()
          .filter((g) => g.label !== "Standard")
          .flatMap((g) =>
            g.fbs.map((fb) => ({
              label: fb.type,
              kind: Kind.Class,
              insertText: fb.type,
              detail: fb.library ? `library · ${fb.library}` : "project FB",
              documentation: fb.description,
              range,
            })),
          )
        return {
          suggestions: [
            ...varSuggestions,
            ...fbTypeSuggestions,
            ...keywords.map((k) => make(k, Kind.Keyword, detailKeyword)),
            ...typeKeywords.map((t) =>
              make(t, Kind.TypeParameter, detailType),
            ),
            ...builtins.map((b) => make(b, Kind.Function, detailBuiltin)),
          ],
        }
      },
    })

    // Hover provider: when the cursor sits on an identifier, look it
    // up in the live symbol table and produce a markdown card showing
    // its IEC type + section. Falls back to the keyword/type list so
    // hovering `TON` reveals "standard function block", not nothing.
    monaco.languages.registerHoverProvider(LANGUAGE_ID, {
      provideHover(model: editor.ITextModel, position: { lineNumber: number; column: number }) {
        const word = model.getWordAtPosition(position)
        if (!word) return null
        const sym = symbolsRef.current.find((s) => s.name === word.word)
        if (sym) {
          return {
            range: {
              startLineNumber: position.lineNumber,
              endLineNumber: position.lineNumber,
              startColumn: word.startColumn,
              endColumn: word.endColumn,
            },
            contents: [
              {
                value: `**${sym.name}** : \`${sym.type_name}\``,
              },
              { value: `*${sym.direction}*` },
            ],
          }
        }
        // Keyword / builtin / type fallback. Useful for "what is TON?"
        // — we don't have docs inline, but we name the category.
        if (keywords.includes(word.word as never)) {
          return makeHover(position, word, "**keyword**")
        }
        if (typeKeywords.includes(word.word as never)) {
          return makeHover(position, word, `**IEC type** \`${word.word}\``)
        }
        if (builtins.includes(word.word as never)) {
          return makeHover(
            position,
            word,
            `**Standard library** — \`${word.word}\`. See \`cs explain\` if a diagnostic refers to it.`,
          )
        }
        return null
      },
    })

    languageRegistered = true
  }, [])

  // Agent-generated reveal: give every visible Monaco line a small
  // slide-in with a per-line stagger, so code reads as being WRITTEN
  // rather than blinked into place. Runs only on `spawnTick` bumps
  // (SSE-driven agent updates) — human typing never triggers it.
  // Monaco rebuilds `.view-line` DOM asynchronously after the value
  // prop lands, hence the double-rAF + small delay before we stamp
  // delays; lines are staggered by their on-screen position (virtual
  // scrolling does not keep DOM order = visual order).
  useEffect(() => {
    if (spawnTick === 0) return
    const t = setTimeout(() => {
      requestAnimationFrame(() =>
        requestAnimationFrame(() => {
          const dom = editorRef.current?.getDomNode()
          if (!dom) return
          const lines = [...dom.querySelectorAll<HTMLElement>(".view-line")].sort(
            (a, b) => a.offsetTop - b.offsetTop,
          )
          lines.forEach((el, i) => {
            el.style.animationDelay = `${Math.min(i * 22, 700)}ms`
          })
          dom.classList.add("pou-lines-spawn")
          setTimeout(() => {
            dom.classList.remove("pou-lines-spawn")
            lines.forEach((el) => {
              el.style.animationDelay = ""
            })
          }, 1600)
        }),
      )
    }, 60)
    return () => clearTimeout(t)
  }, [spawnTick])

  // Refetch symbols whenever the source settles. Same 350ms debounce
  // as the diagnostics path; if symbols fetch fails we just keep the
  // last good list (network blip shouldn't kill completion).
  useEffect(() => {
    const handle = setTimeout(async () => {
      try {
        symbolsRef.current = await fetchSymbols(value, "st")
      } catch (e) {
        console.warn("ST symbols fetch failed:", e)
      }
    }, 350)
    return () => clearTimeout(handle)
  }, [value])

  // Push diagnostics into Monaco's marker pool whenever they change.
  // We also forward Monaco's `relatedInformation` field so the
  // "did you mean: foo?" hints show up under the error squiggle —
  // ironplc Diagnostic.secondary populates `related[]` upstream.
  useEffect(() => {
    const editor = editorRef.current
    const monaco = monacoRef.current
    if (!editor || !monaco) return
    const model = editor.getModel()
    if (!model) return
    const markers = diagnostics.map((d) => {
      const ctx = d.context.length > 0 ? ` — ${d.context.join(", ")}` : ""
      return {
        severity: monaco.MarkerSeverity.Error,
        message: `${d.code}: ${d.message}${ctx}`,
        startLineNumber: d.start_line,
        startColumn: d.start_column,
        endLineNumber: d.end_line,
        endColumn: Math.max(d.end_column, d.start_column + 1),
        code: d.code,
        source: "ironplc",
        relatedInformation: d.related.map((r) => ({
          resource: model.uri,
          message: r.message,
          startLineNumber: r.start_line,
          startColumn: r.start_column,
          endLineNumber: r.end_line,
          endColumn: Math.max(r.end_column, r.start_column + 1),
        })),
      }
    })
    monaco.editor.setModelMarkers(model, "iec61131", markers)
  }, [diagnostics])

  return (
    <Editor
      height="100%"
      width="100%"
      language={LANGUAGE_ID}
      value={value}
      theme={theme}
      // Themes must exist before the first paint, otherwise Monaco falls
      // back to stock `vs` for a frame and the editor flashes white on a
      // dark workbench. `beforeMount` runs ahead of that first paint;
      // `onMount` would be one frame too late.
      beforeMount={defineAcidThemes}
      onChange={(v) => onChange(v ?? "")}
      onMount={handleMount}
      options={{ ...editorOptions, readOnly, domReadOnly: readOnly }}
    />
  )
}

/** Small helper to assemble a Monaco hover object from a single
 *  markdown string. Used by the keyword / builtin / type fallback
 *  branches in the hover provider above. */
function makeHover(
  position: { lineNumber: number; column: number },
  word: { word: string; startColumn: number; endColumn: number },
  markdown: string,
) {
  return {
    range: {
      startLineNumber: position.lineNumber,
      endLineNumber: position.lineNumber,
      startColumn: word.startColumn,
      endColumn: word.endColumn,
    },
    contents: [{ value: markdown }],
  }
}
