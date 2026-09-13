/**
 * Shared diagnostics banner for LD / FBD / SFC editors.
 *
 * Replaces the three near-identical local banners that each editor used
 * to define. Renders one row per diagnostic with the message + the
 * graphical-source location; each row expands on demand to show the
 * full context / related / RST explanation that ironplc ships with
 * every problem code.
 *
 * Each editor passes a `formatLocation` that knows how to describe
 * its own `*_location` field — that way this component stays
 * language-agnostic while still giving operators a useful tail hint
 * on every row.
 */

import { useState } from "react"

import { cn } from "@/lib/utils"
import type { CheckDiagnostic } from "@/types/generated/CheckDiagnostic"

interface Props {
  diagnostics: CheckDiagnostic[]
  /** Returns a short hint describing where the diagnostic comes from
   *  ("rung loose · coil 0", "block b1", "step idle", "—"). The caller
   *  uses its own LdLocation / FbdLocation / SfcLocation formatter. */
  formatLocation: (d: CheckDiagnostic) => string
  /** Optional jump-to-source handler. Called when the user clicks the
   *  row body (NOT the expand button). The editor decides what
   *  "jump" means — usually setting the local selection. */
  onJump?: (d: CheckDiagnostic) => void
}

export function DiagnosticsBanner({
  diagnostics,
  formatLocation,
  onJump,
}: Props) {
  return (
    <div className="border-b border-destructive/30 bg-destructive/5 text-xs">
      <div className="flex items-center gap-2 px-3 py-1.5">
        <span className="font-mono font-medium text-destructive">
          {diagnostics.length} {diagnostics.length === 1 ? "error" : "errors"}
        </span>
      </div>
      <ul className="divide-y divide-destructive/15">
        {diagnostics.slice(0, 8).map((d, i) => (
          <Row
            key={i}
            d={d}
            formatLocation={formatLocation}
            onJump={onJump}
          />
        ))}
        {diagnostics.length > 8 && (
          <li className="px-3 py-1 text-muted-foreground">
            +{diagnostics.length - 8} more…
          </li>
        )}
      </ul>
    </div>
  )
}

function Row({
  d,
  formatLocation,
  onJump,
}: {
  d: CheckDiagnostic
  formatLocation: (d: CheckDiagnostic) => string
  onJump?: (d: CheckDiagnostic) => void
}) {
  const [expanded, setExpanded] = useState(false)
  // We have something more to show if ANY of the thick fields is
  // populated — context lines, related labels, or the embedded RST
  // explanation.
  const hasMore =
    d.context.length > 0 || d.related.length > 0 || d.explanation !== null
  return (
    <li>
      <div className="flex items-start gap-2 px-3 py-1">
        <button
          type="button"
          onClick={() => onJump?.(d)}
          disabled={!onJump}
          className="flex flex-1 items-start gap-2 text-left hover:text-foreground disabled:cursor-default"
        >
          <span className="font-mono text-xs text-destructive">
            {d.code}
          </span>
          <span className="flex-1 text-foreground">{d.message}</span>
          {/* The first context entry as a short tail hint — almost
              always one structured fragment like `variable=foo`. */}
          {d.context.length > 0 && (
            <span className="font-mono text-xs text-muted-foreground">
              [{d.context[0]}]
            </span>
          )}
          <span className="font-mono text-xs text-muted-foreground">
            {formatLocation(d)}
          </span>
        </button>
        {hasMore && (
          <button
            type="button"
            onClick={() => setExpanded((e) => !e)}
            className={cn(
              "rounded px-1 font-mono text-[13px] text-muted-foreground hover:bg-accent/40 hover:text-foreground",
              expanded && "bg-accent/30 text-foreground",
            )}
            title={
              expanded
                ? "Hide details"
                : "Show context, related labels, and the full explanation"
            }
            aria-expanded={expanded}
            aria-label={expanded ? "Collapse details" : "Expand details"}
          >
            {expanded ? "−" : "?"}
          </button>
        )}
      </div>
      {expanded && hasMore && <Detail d={d} />}
    </li>
  )
}

function Detail({ d }: { d: CheckDiagnostic }) {
  return (
    <div className="border-t border-destructive/15 bg-destructive/[0.04] px-3 py-2 text-[13px]">
      {/* Context entries: short structured fragments like
          `variable=ghost`. ironplc emits these via Diagnostic.described
          (one fragment per call to `with_context*`). */}
      {d.context.length > 0 && (
        <ul className="mb-2 space-y-0.5">
          {d.context.map((c, i) => (
            <li key={i} className="font-mono text-foreground/80">
              · {c}
            </li>
          ))}
        </ul>
      )}
      {/* Related labels: "did you mean: counter?", "first declared
          here", etc. Each carries its own line/column so the editor
          could later wire it as a clickable jump. For now we show the
          source position as a static suffix. */}
      {d.related.length > 0 && (
        <ul className="mb-2 space-y-0.5">
          {d.related.map((r, i) => (
            <li key={i} className="text-foreground">
              <span className="mr-1 text-muted-foreground">→</span>
              {r.message}{" "}
              <span className="font-mono text-xs text-muted-foreground">
                @ {r.start_line}:{r.start_column}
              </span>
            </li>
          ))}
        </ul>
      )}
      {/* The embedded RST page. Collapsed by default — agents see
          this in the JSON payload directly; humans expand the
          `<details>` when they want the full prose. */}
      {d.explanation && (
        <details className="mt-1">
          <summary className="cursor-pointer text-muted-foreground hover:text-foreground">
            Explanation ({d.code})
          </summary>
          <pre className="mt-1 max-h-72 overflow-auto whitespace-pre-wrap rounded border border-border bg-muted/30 p-2 font-mono text-xs text-foreground/90">
            {d.explanation}
          </pre>
        </details>
      )}
    </div>
  )
}
