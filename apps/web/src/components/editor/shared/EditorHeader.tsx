/**
 * Shared top-of-pane header for the LD / FBD / SFC editors.
 *
 * The three were near-identical: the POU name in mono, a language
 * badge, a PROGRAM/FUNCTION_BLOCK badge, and a one-line element count.
 * They differed only in the language literal and the count noun, so
 * those come in as props (`language`, `summary`). `children` carries
 * any language-specific trailer — e.g. SFC's live "→ activeStep" badge.
 */

import type { LdPouType } from "@/types/generated/LdPouType"

export function EditorHeader({
  name,
  language,
  pouType,
  summary,
  children,
}: {
  name: string
  /** Short language tag rendered in the badge (`ld` / `fbd` / `sfc`). */
  language: string
  pouType: LdPouType
  /** The element-count line, e.g. "3 rungs · 2 vars". */
  summary: React.ReactNode
  /** Optional language-specific trailer (SFC's active-step badge). */
  children?: React.ReactNode
}) {
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 bg-background px-4 py-2 text-[13px] text-muted-foreground">
      <span className="font-mono text-foreground">
        {name}
      </span>
      <span>
        {language.toUpperCase()}
      </span>
      <span>
        {pouType === "function_block" ? "Function block" : "Program"}
      </span>
      <span>{summary}</span>
      {children}
    </div>
  )
}
