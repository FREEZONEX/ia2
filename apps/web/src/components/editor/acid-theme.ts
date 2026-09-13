import type { Monaco } from "@monaco-editor/react"

/** Monaco uses the same opaque surfaces as the Tier0 workbench.
 * Syntax colors carry language roles; comments and line numbers stay readable.
 * Theme IDs are stable so existing editor instances keep their selection. */

const LIGHT = {
  bg: "#FFFFFF",
  fg: "#050B14",
  comment: "#626A66",
  keyword: "#6E3C9C",
  type: "#1E7A66",
  number: "#4E7A2C",
  operator: "#585C62",
  string: "#4E7A2C",
  lineNr: "#6F7773",
  lineNrActive: "#353D38",
  selection: "#DDF59C",
  lineHighlight: "#F9F9F9",
} as const

const DARK = {
  bg: "#151A19",
  fg: "#F9F9F9",
  comment: "#ABB5AF",
  keyword: "#C49AD8",
  type: "#6FC1A8",
  number: "#A9CE80",
  operator: "#B5BAB7",
  string: "#A9CE80",
  lineNr: "#88958D",
  lineNrActive: "#CDD5D0",
  selection: "#354528",
  lineHighlight: "#1C211F",
} as const

export const ACID_LIGHT = "ia2-acid-light"
export const ACID_DARK = "ia2-acid-dark"

function rules(p: typeof LIGHT | typeof DARK) {
  // Monaco strips the leading '#' in token rules but not in `colors`.
  const h = (c: string) => c.slice(1)
  return [
    { token: "comment", foreground: h(p.comment), fontStyle: "italic" },
    { token: "keyword", foreground: h(p.keyword) },
    // IEC types (INT / DINT / BOOL / TIME…) — our monarch emits
    // `type.identifier` for these.
    { token: "type.identifier", foreground: h(p.type) },
    { token: "number", foreground: h(p.number) },
    { token: "number.hex", foreground: h(p.number) },
    { token: "number.time", foreground: h(p.number) },
    { token: "string", foreground: h(p.string) },
    { token: "string.escape", foreground: h(p.string) },
    { token: "string.quote", foreground: h(p.string) },
    { token: "operator", foreground: h(p.operator) },
    { token: "delimiter", foreground: h(p.operator) },
    { token: "identifier", foreground: h(p.fg) },
    // Standard-library calls (TON, CTU, …) share the type hue: both
    // are "things the standard gives you", vs. your own identifiers.
    { token: "support.function", foreground: h(p.type) },
  ]
}

function colors(p: typeof LIGHT | typeof DARK) {
  return {
    "editor.background": p.bg,
    "editor.foreground": p.fg,
    "editorLineNumber.foreground": p.lineNr,
    "editorLineNumber.activeForeground": p.lineNrActive,
    "editor.selectionBackground": p.selection,
    "editor.inactiveSelectionBackground": `${p.selection}80`,
    "editor.lineHighlightBackground": p.lineHighlight,
    "editor.lineHighlightBorder": "#00000000",
    "editorCursor.foreground": p.fg,
    "editorWidget.background": p.lineHighlight,
    "editorWidget.border": p.lineNr,
    "editorSuggestWidget.background": p.lineHighlight,
    "editorSuggestWidget.selectedBackground": p.selection,
    "editorGutter.background": p.bg,
    "editorIndentGuide.background1": p.lineHighlight,
    "scrollbarSlider.background": `${p.lineNr}40`,
    "scrollbarSlider.hoverBackground": `${p.lineNr}70`,
    "scrollbarSlider.activeBackground": `${p.lineNr}90`,
  }
}

/** Idempotent — Monaco tolerates re-defining a theme by the same name. */
export function defineAcidThemes(monaco: Monaco) {
  monaco.editor.defineTheme(ACID_LIGHT, {
    base: "vs",
    inherit: true,
    rules: rules(LIGHT),
    colors: colors(LIGHT),
  })
  monaco.editor.defineTheme(ACID_DARK, {
    base: "vs-dark",
    inherit: true,
    rules: rules(DARK),
    colors: colors(DARK),
  })
}
