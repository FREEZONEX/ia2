/**
 * Sequential Function Chart (SFC) — editor.
 *
 * Renders an SFC POU as a Codesys / TIA-style vertical-flow chart:
 *
 *   - **Steps** are square-cornered boxes stacked vertically and
 *     horizontally centred. The initial step gets the classic IEC
 *     "double frame".
 *   - **Actions** for each step are NOT crammed into the step box;
 *     they sit as independent small two-column boxes to the RIGHT of
 *     the step, connected by a short horizontal line. This matches
 *     industrial SFC conventions and makes a busy step (3+ actions)
 *     readable at a glance.
 *   - **Transitions** render as a thick horizontal bar centred on the
 *     vertical flow line — the bar literally crosses the wire between
 *     two steps, exactly like IEC 61131-3 § 2.6.3 specifies. The
 *     condition + target step name sit to the right of the bar.
 *   - **Active step** (when running) inverts colours: highlight fill,
 *     background text — survives printing and dark mode.
 *
 * Editing:
 *   - Toolbar "+ Step" appends a new step.
 *   - Click a step → detail bar to rename / mark initial / delete /
 *     add action / reorder.
 *   - Click an action → detail bar to change qualifier and body.
 *   - Click a transition bar → detail bar to edit from / to / condition
 *     / reorder / delete.
 *
 * Authoring keeps the JSON file as the source of truth — every UI
 * change serialises through `onChange`, the editor re-parses on the
 * next tick. No internal mutable state to drift.
 */

import { Plus, Trash2, X } from "@/components/ui/icons"
import { useEffect, useMemo, useState } from "react"

import { DiagnosticsBanner } from "@/components/editor/DiagnosticsBanner"
import { ActionBtn, DetailLabel, Separator } from "@/components/editor/shared/DetailBar"
import {
  indexDiagnostics,
  type DiagnosticKey,
} from "@/components/editor/shared/diagnostics"
import { EditorHeader } from "@/components/editor/shared/EditorHeader"
import { ReadonlyVariablePanel } from "@/components/editor/shared/ReadonlyVariablePanel"
import {
  ParseErrorView,
  useProgramEditor,
  type ParseResult,
} from "@/components/editor/shared/useProgramEditor"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { cn } from "@/lib/utils"
import {
  addAction,
  addStep,
  addTransition,
  moveTransition,
  parseProgram,
  removeAction,
  removeStep,
  removeTransition,
  renameStep,
  serializeProgram,
  setActionBody,
  setActionQualifier,
  setInitialStep,
  updateTransition,
} from "@/lib/sfc-edit"
import type { CheckDiagnostic } from "@/types/generated/CheckDiagnostic"
import type { SfcAction } from "@/types/generated/SfcAction"
import type { SfcLocation } from "@/types/generated/SfcLocation"
import type { SfcProgram } from "@/types/generated/SfcProgram"
import type { SfcQualifier } from "@/types/generated/SfcQualifier"
import type { SfcTransition } from "@/types/generated/SfcTransition"

// =================================================================
//   Selection state
// =================================================================

/** Whatever element the user has currently clicked. The detail bar
 *  reads this to decide which controls to show. `null` = nothing
 *  selected, the canvas is in "view" mode. */
type Selection =
  | { kind: "step"; name: string }
  | { kind: "action"; step: string; index: number }
  | { kind: "transition"; index: number }
  | null

// =================================================================
//   Component
// =================================================================

export function SFCEditor({
  value,
  onChange,
  className,
  readOnly = false,
  path,
}: {
  value: string
  onChange: (next: string) => void
  className?: string
  readOnly?: boolean
  /** Store slug this buffer came from — keeps the project-aware check
   *  from double-counting the on-disk copy. */
  path?: string
}) {
  const { parsed, diagnostics, isRunning, lastSnapshot, commit } =
    useProgramEditor<SfcProgram>({
      value,
      onChange,
      readOnly,
      path,
      language: "sfc",
      parse: safeParse,
      serialize: serializeProgram,
    })

  // Online-mode current step lookup (see also LDEditor / FBDEditor).
  const activeStep = useMemo<string | null>(() => {
    if (!isRunning || !lastSnapshot) return null
    const v = lastSnapshot.vars.find((x) => x.name === "__sfc_step")
    if (!v) return null
    return v.value.replace(/^'/, "").replace(/'$/, "")
  }, [lastSnapshot, isRunning])

  const [sel, setSel] = useState<Selection>(null)
  // Drop selection on external source changes (revert, POU switch).
  useEffect(() => setSel(null), [value])

  if (parsed.kind === "error") {
    return (
      <ParseErrorView
        label="SFC"
        message={parsed.message}
        source={value}
        className={className}
      />
    )
  }

  const prog = parsed.program
  const diagIndex = useMemo(
    () => indexDiagnostics(diagnostics, sfcDiagnosticKey),
    [diagnostics],
  )

  // Cache outbound transitions per step so the render pass doesn't
  // re-scan transitions[] for every step. Also keep the **global**
  // transition index for each entry — SfcTransition has no id, the
  // detail bar needs an index to mutate by.
  const outboundByStep = useMemo(() => {
    const m = new Map<string, Array<{ t: SfcTransition; index: number }>>()
    prog.transitions.forEach((t, index) => {
      const list = m.get(t.from) ?? []
      list.push({ t, index })
      m.set(t.from, list)
    })
    return m
  }, [prog.transitions])

  return (
    <div className={cn("flex h-full min-h-0 flex-col", className)}>
      <EditorHeader
        name={prog.name}
        language="sfc"
        pouType={prog.pou_type}
        summary={
          <>
            {prog.steps.length} step{prog.steps.length === 1 ? "" : "s"} ·{" "}
            {prog.transitions.length} transition
            {prog.transitions.length === 1 ? "" : "s"}
          </>
        }
      >
        {activeStep && (
          <span className="ml-3 rounded bg-highlight/15 px-1.5 py-0.5 font-mono text-xs normal-case text-foreground">
            → {activeStep}
          </span>
        )}
      </EditorHeader>
      <Toolbar
        readOnly={readOnly}
        canAddTransition={prog.steps.length >= 2}
        onAddStep={() => {
          const { prog: next, name } = addStep(prog)
          commit(next)
          setSel({ kind: "step", name })
        }}
        onAddTransition={() => {
          if (prog.steps.length < 2) return
          const next = addTransition(
            prog,
            prog.steps[0].name,
            prog.steps[1].name,
            "TRUE",
          )
          commit(next)
          setSel({ kind: "transition", index: next.transitions.length - 1 })
        }}
      />
      {diagnostics.length > 0 && (
        <DiagnosticsBanner
          diagnostics={diagnostics}
          formatLocation={(d) => describeLocation(d.sfc_location)}
        />
      )}
      <div className="flex-1 overflow-auto bg-background">
        <ReadonlyVariablePanel
          variables={prog.variables}
          byVariable={diagIndex.byVariable}
        />
        {prog.steps.length === 0 ? (
          <EmptyState
            readOnly={readOnly}
            onAddStep={() => {
              const { prog: next, name } = addStep(prog)
              commit(next)
              setSel({ kind: "step", name })
            }}
          />
        ) : (
          <div className="py-6">
            {prog.steps.map((step, i) => {
              const isActive = activeStep === step.name
              const isInitial = step.name === prog.initial_step
              const outbound = outboundByStep.get(step.name) ?? []
              const stepSelected =
                sel?.kind === "step" && sel.name === step.name
              const hasStepError = diagIndex.byElement.has(step.name)
              return (
                <div
                  key={step.name}
                  className="flex flex-col items-stretch"
                >
                  {/* Connector dropping in from above (transition or
                      canvas edge). Skip for the very first step. */}
                  {i > 0 && (
                    <Connector
                      active={isActive}
                      length={16}
                    />
                  )}

                  {/* Step row: spacer | step + actions | spacer.
                      Step + actions are kept together so a horizontal
                      connector can join them; spacers centre the step
                      column on the canvas while still allowing the
                      actions block to stretch right. */}
                  <div className="flex w-full items-stretch px-4">
                    <div className="flex-1" />
                    <StepBox
                      name={step.name}
                      isInitial={isInitial}
                      isActive={isActive}
                      selected={stepSelected}
                      hasError={hasStepError}
                      onClick={() => setSel({ kind: "step", name: step.name })}
                    />
                    <ActionList
                      step={step.name}
                      actions={step.actions}
                      stepActive={isActive}
                      selection={sel}
                      readOnly={readOnly}
                      onSelectAction={(index) =>
                        setSel({ kind: "action", step: step.name, index })
                      }
                      onAddAction={() => {
                        const next = addAction(prog, step.name)
                        commit(next)
                        setSel({
                          kind: "action",
                          step: step.name,
                          index: step.actions.length,
                        })
                      }}
                    />
                    <div className="flex-1" />
                  </div>

                  {/* Outbound transitions or fall-through connector. */}
                  {outbound.length > 0 ? (
                    outbound.map(({ t, index }) => (
                      <TransitionRow
                        key={`${t.from}-${t.to}-${index}`}
                        transition={t}
                        index={index}
                        active={isActive}
                        selected={
                          sel?.kind === "transition" && sel.index === index
                        }
                        onClick={() => setSel({ kind: "transition", index })}
                      />
                    ))
                  ) : i < prog.steps.length - 1 ? (
                    <Connector active={isActive} length={16} />
                  ) : null}
                </div>
              )
            })}
          </div>
        )}
      </div>
      {!readOnly && sel && (
        <DetailBar
          prog={prog}
          sel={sel}
          onClose={() => setSel(null)}
          onCommit={commit}
          onSelect={setSel}
        />
      )}
    </div>
  )
}

// =================================================================
//   Toolbar
// =================================================================

function Toolbar({
  readOnly,
  canAddTransition,
  onAddStep,
  onAddTransition,
}: {
  readOnly: boolean
  canAddTransition: boolean
  onAddStep: () => void
  onAddTransition: () => void
}) {
  if (readOnly) return null
  return (
    <div className="flex flex-wrap items-center gap-2 border-b border-border bg-background px-4 pb-2 text-[13px]">
      <Button
        type="button"
        onClick={onAddStep}
        variant="secondary" size="sm"
        title="Append a new step"
      >
        <Plus className="size-3" />
        Step
      </Button>
      <Button
        type="button"
        onClick={onAddTransition}
        disabled={!canAddTransition}
        variant="secondary" size="sm"
        title={
          canAddTransition
            ? "Add a transition between two steps"
            : "Need at least two steps first"
        }
      >
        <Plus className="size-3" />
        Transition
      </Button>
      <span className="text-xs text-muted-foreground">
        click any element to edit · double frame = initial step ·
        transitions evaluate top-to-bottom (first match wins)
      </span>
    </div>
  )
}

function EmptyState({
  readOnly,
  onAddStep,
}: {
  readOnly: boolean
  onAddStep: () => void
}) {
  return (
    <div className="flex h-64 items-center justify-center text-sm text-muted-foreground">
      <div className="flex flex-col items-center gap-2">
        <span>No steps yet.</span>
        {!readOnly && (
          <button
            type="button"
            onClick={onAddStep}
            className="flex items-center gap-1 rounded border border-input bg-card px-3 py-1.5 text-xs hover:bg-accent/30"
          >
            <Plus className="size-3" />
            Add first step
          </button>
        )}
      </div>
    </div>
  )
}

// =================================================================
//   Step / Action / Transition rendering
// =================================================================

/** Thin vertical connector line between consecutive elements. Turns
 *  highlight-coloured when the upstream step is active, hinting at
 *  the current control-flow path. */
function Connector({ active, length }: { active: boolean; length: number }) {
  return (
    <div
      className={cn(
        "mx-auto w-px",
        active ? "bg-highlight" : "bg-foreground",
      )}
      style={{ height: `${length}px` }}
    />
  )
}

function StepBox({
  name,
  isInitial,
  isActive,
  selected,
  hasError,
  onClick,
}: {
  name: string
  isInitial: boolean
  isActive: boolean
  selected: boolean
  hasError: boolean
  onClick: () => void
}) {
  // Initial step uses the classic IEC "double frame": an outer 2px
  // border with a small inset and an inner 2px border. We do it via
  // padding on the outer wrapper so the two borders are properly
  // separated visually without absolute positioning.
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "block cursor-pointer outline-none",
        isInitial ? "border-2 border-foreground p-[3px]" : "",
        selected && "ring-2 ring-offset-1 ring-highlight",
        hasError && "border-destructive",
      )}
    >
      <div
        className={cn(
          "border-2 min-w-[140px] px-6 py-2 text-center font-mono text-sm font-semibold",
          hasError
            ? "border-destructive bg-card text-foreground"
            : isActive
              ? "border-highlight bg-highlight text-background"
              : "border-foreground bg-card text-foreground",
        )}
      >
        {name}
      </div>
    </button>
  )
}

function ActionList({
  step,
  actions,
  stepActive,
  selection,
  readOnly,
  onSelectAction,
  onAddAction,
}: {
  step: string
  actions: SfcAction[]
  stepActive: boolean
  selection: Selection
  readOnly: boolean
  onSelectAction: (index: number) => void
  onAddAction: () => void
}) {
  // Render the actions as a flex column to the right of the step,
  // joined by a short horizontal connector line. Each action is its
  // own two-column box: qualifier letter (bold) | body (mono text).
  // Following Codesys's convention this gets the actions out of the
  // step's body, where 3+ actions otherwise cause vertical bloat.
  if (actions.length === 0 && readOnly) return null
  return (
    <div className="flex items-center pl-2">
      {/* Connector line from the step's right edge into the action
          stack. Drawn only when there are actions to point at. */}
      {actions.length > 0 && (
        <div
          className={cn(
            "h-[2px] w-4",
            stepActive ? "bg-highlight" : "bg-foreground",
          )}
        />
      )}
      <ul className="flex flex-col gap-1">
        {actions.map((a, i) => {
          const sel =
            selection?.kind === "action" &&
            selection.step === step &&
            selection.index === i
          return (
            <li key={i}>
              <button
                type="button"
                onClick={() => onSelectAction(i)}
                className={cn(
                  "flex items-stretch border-2 border-foreground bg-card text-left outline-none",
                  sel && "ring-2 ring-highlight ring-offset-1",
                )}
                title={qualifierTooltip(a.qualifier)}
              >
                <span className="border-r-2 border-foreground bg-muted/30 px-1.5 py-0.5 font-mono text-xs font-bold text-foreground">
                  {a.qualifier}
                </span>
                <pre className="max-w-xs whitespace-pre-wrap px-2 py-0.5 font-mono text-[13px] text-foreground">
                  {a.body || (
                    // An empty body compiles to a transpile error — flag
                    // it as something to fill, not a neutral placeholder.
                    <span className="italic text-destructive/80">
                      empty — add an ST statement
                    </span>
                  )}
                </pre>
              </button>
            </li>
          )
        })}
        {!readOnly && (
          <li>
            <button
              type="button"
              onClick={onAddAction}
              className="flex items-center gap-1 border border-dashed border-muted-foreground/50 bg-transparent px-2 py-0.5 text-xs text-muted-foreground hover:bg-accent/30 hover:text-foreground"
              title={`Add an action to ${step}`}
            >
              <Plus className="size-3" />
              action
            </button>
          </li>
        )}
      </ul>
    </div>
  )
}

function TransitionRow({
  transition,
  index: _index,
  active,
  selected,
  onClick,
}: {
  transition: SfcTransition
  index: number
  active: boolean
  selected: boolean
  onClick: () => void
}) {
  // Layout (IEC § 2.6.3): a thick horizontal bar that visually
  // CROSSES the vertical flow line. We achieve that with three flex
  // columns:
  //    [ left spacer ] [ bar centred on the flow ] [ right: cond / target ]
  // The bar's width is fixed (small enough to look like a "cross-
  // hatch" mark, not a separator). The connector lines above and
  // below the bar are part of this row so the gap is consistent.
  return (
    <div className="flex w-full items-stretch px-4">
      <div className="flex-1" />
      <div className="flex flex-col items-center">
        <div
          className={cn(
            "h-3 w-px",
            active ? "bg-highlight" : "bg-foreground",
          )}
        />
        <button
          type="button"
          onClick={onClick}
          className={cn(
            "h-1.5 w-24 cursor-pointer outline-none",
            active ? "bg-highlight" : "bg-foreground",
            selected && "ring-2 ring-highlight ring-offset-1",
          )}
          title={`Transition ${transition.from} → ${transition.to}`}
        />
        <div
          className={cn(
            "h-3 w-px",
            active ? "bg-highlight" : "bg-foreground",
          )}
        />
      </div>
      <div className="flex-1 self-center pl-3 font-mono text-[13px]">
        <span className="text-foreground">{transition.condition}</span>
        <span className="ml-2 text-muted-foreground">→ {transition.to}</span>
      </div>
    </div>
  )
}

// =================================================================
//   Detail bar (selected element editor)
// =================================================================

function DetailBar({
  prog,
  sel,
  onClose,
  onCommit,
  onSelect,
}: {
  prog: SfcProgram
  sel: Exclude<Selection, null>
  onClose: () => void
  onCommit: (next: SfcProgram) => void
  onSelect: (s: Selection) => void
}) {
  if (sel.kind === "step") {
    return (
      <StepDetail
        prog={prog}
        name={sel.name}
        onCommit={onCommit}
        onClose={onClose}
        onSelectAfter={onSelect}
      />
    )
  }
  if (sel.kind === "action") {
    return (
      <ActionDetail
        prog={prog}
        step={sel.step}
        index={sel.index}
        onCommit={onCommit}
        onClose={onClose}
      />
    )
  }
  return (
    <TransitionDetail
      prog={prog}
      index={sel.index}
      onCommit={onCommit}
      onClose={onClose}
    />
  )
}

function StepDetail({
  prog,
  name,
  onCommit,
  onClose,
  onSelectAfter,
}: {
  prog: SfcProgram
  name: string
  onCommit: (next: SfcProgram) => void
  onClose: () => void
  onSelectAfter: (s: Selection) => void
}) {
  const step = prog.steps.find((s) => s.name === name)
  if (!step) return null
  const isInitial = prog.initial_step === name
  return (
    <DetailContainer>
      <DetailLabel>step</DetailLabel>
      <RenameInput
        value={name}
        existing={prog.steps.map((s) => s.name).filter((n) => n !== name)}
        onCommit={(v) => {
          onCommit(renameStep(prog, name, v))
          onSelectAfter({ kind: "step", name: v })
        }}
      />
      <Separator />
      <button
        type="button"
        onClick={() => onCommit(setInitialStep(prog, name))}
        disabled={isInitial}
        className={cn(
          "h-7 rounded px-2 text-[13px]",
          isInitial
            ? "cursor-default bg-highlight/15 text-foreground"
            : "border border-input hover:bg-accent/30",
        )}
        title="Mark as the initial / entry step"
      >
        {isInitial ? "initial ✓" : "set initial"}
      </button>
      <Separator />
      <ActionBtn
        onClick={() => onCommit(addAction(prog, name))}
        title={`Add an action to ${name}`}
      >
        <Plus className="size-3" />
        action
      </ActionBtn>
      <span className="ml-auto inline-flex gap-1">
        <DangerBtn
          onClick={() => {
            onCommit(removeStep(prog, name))
            onClose()
          }}
          title={`Delete step ${name} and all referring transitions`}
        >
          <Trash2 className="size-3" />
          delete
        </DangerBtn>
        <CloseBtn onClick={onClose} />
      </span>
    </DetailContainer>
  )
}

function ActionDetail({
  prog,
  step,
  index,
  onCommit,
  onClose,
}: {
  prog: SfcProgram
  step: string
  index: number
  onCommit: (next: SfcProgram) => void
  onClose: () => void
}) {
  const s = prog.steps.find((s) => s.name === step)
  const action = s?.actions[index]
  if (!action) return null
  return (
    <DetailContainer>
      <DetailLabel>action</DetailLabel>
      <span className="font-mono text-xs text-muted-foreground">
        {step} · #{index}
      </span>
      <Select
        value={action.qualifier}
        onValueChange={(q) =>
          onCommit(
            setActionQualifier(prog, step, index, q as SfcQualifier),
          )
        }
      >
        <SelectTrigger className="h-7 w-16 text-xs" title="Qualifier">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="N">N — while active</SelectItem>
          <SelectItem value="S">S — on entry (set)</SelectItem>
          <SelectItem value="R">R — on entry (reset)</SelectItem>
        </SelectContent>
      </Select>
      <ActionBodyInput
        value={action.body}
        onCommit={(v) => onCommit(setActionBody(prog, step, index, v))}
      />
      <span className="ml-auto inline-flex gap-1">
        <DangerBtn
          onClick={() => {
            onCommit(removeAction(prog, step, index))
            onClose()
          }}
          title="Delete this action"
        >
          <Trash2 className="size-3" />
          delete
        </DangerBtn>
        <CloseBtn onClick={onClose} />
      </span>
    </DetailContainer>
  )
}

function TransitionDetail({
  prog,
  index,
  onCommit,
  onClose,
}: {
  prog: SfcProgram
  index: number
  onCommit: (next: SfcProgram) => void
  onClose: () => void
}) {
  const t = prog.transitions[index]
  if (!t) return null
  return (
    <DetailContainer>
      <DetailLabel>transition</DetailLabel>
      <span className="font-mono text-xs text-muted-foreground">
        #{index}
      </span>
      <Select
        value={t.from}
        onValueChange={(v) =>
          onCommit(updateTransition(prog, index, { from: v }))
        }
      >
        <SelectTrigger className="h-7 w-24 text-xs" title="from">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {prog.steps.map((s) => (
            <SelectItem key={s.name} value={s.name}>
              {s.name}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      <span className="text-muted-foreground">→</span>
      <Select
        value={t.to}
        onValueChange={(v) =>
          onCommit(updateTransition(prog, index, { to: v }))
        }
      >
        <SelectTrigger className="h-7 w-24 text-xs" title="to">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {prog.steps.map((s) => (
            <SelectItem key={s.name} value={s.name}>
              {s.name}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      <ConditionInput
        value={t.condition}
        onCommit={(v) => onCommit(updateTransition(prog, index, { condition: v }))}
      />
      <Separator />
      <ActionBtn
        onClick={() => onCommit(moveTransition(prog, index, index - 1))}
        disabled={index === 0}
        title="Higher priority (evaluate earlier in the cascade)"
      >
        ▲
      </ActionBtn>
      <ActionBtn
        onClick={() => onCommit(moveTransition(prog, index, index + 1))}
        disabled={index >= prog.transitions.length - 1}
        title="Lower priority"
      >
        ▼
      </ActionBtn>
      <span className="ml-auto inline-flex gap-1">
        <DangerBtn
          onClick={() => {
            onCommit(removeTransition(prog, index))
            onClose()
          }}
          title="Delete this transition"
        >
          <Trash2 className="size-3" />
          delete
        </DangerBtn>
        <CloseBtn onClick={onClose} />
      </span>
    </DetailContainer>
  )
}

// =================================================================
//   Detail-bar primitives
// =================================================================

function DetailContainer({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex flex-wrap items-center gap-2 border-t border-highlight/30 bg-highlight/5 px-3 py-1.5 text-xs">
      {children}
    </div>
  )
}

function DangerBtn({
  onClick,
  title,
  children,
}: {
  onClick: () => void
  title: string
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={title}
      className="flex h-7 items-center gap-1 rounded border border-destructive/40 bg-destructive/5 px-2 text-[13px] text-destructive hover:bg-destructive/15"
    >
      {children}
    </button>
  )
}

function CloseBtn({ onClick }: { onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={onClick}
      title="Close"
      className="rounded p-0.5 text-muted-foreground hover:bg-accent/40 hover:text-foreground"
    >
      <X className="size-3" />
    </button>
  )
}

function RenameInput({
  value,
  existing,
  onCommit,
}: {
  value: string
  existing: string[]
  onCommit: (v: string) => void
}) {
  const [draft, setDraft] = useState(value)
  useEffect(() => setDraft(value), [value])
  const commit = (next: string) => {
    const t = next.trim()
    if (!t || t === value) {
      setDraft(value)
      return
    }
    if (t.includes("'") || existing.includes(t)) {
      setDraft(value)
      return
    }
    onCommit(t)
  }
  return (
    <input
      type="text"
      value={draft}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => commit(draft)}
      onKeyDown={(e) => {
        if (e.key === "Enter") commit(draft)
      }}
      className="h-7 w-32 rounded border border-input bg-transparent px-2 font-mono text-xs"
      title="Step name (IEC identifier — no apostrophes)"
    />
  )
}

function ActionBodyInput({
  value,
  onCommit,
}: {
  value: string
  onCommit: (v: string) => void
}) {
  const [draft, setDraft] = useState(value)
  useEffect(() => setDraft(value), [value])
  return (
    <Input
      type="text"
      value={draft}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => {
        // Trim so a whitespace-only body becomes "" — flagged on the
        // canvas as empty — rather than a body that looks blank but
        // smuggles spaces past the eye and fails to compile.
        const next = draft.trim()
        if (next !== value) onCommit(next)
      }}
      onKeyDown={(e) => {
        if (e.key === "Enter") (e.target as HTMLInputElement).blur()
      }}
      placeholder="ST statement, e.g. inlet := TRUE"
      className="h-7 w-64 font-mono text-xs"
      title="Inline ST executed under this qualifier (required — an empty body fails to compile)"
    />
  )
}

function ConditionInput({
  value,
  onCommit,
}: {
  value: string
  onCommit: (v: string) => void
}) {
  const [draft, setDraft] = useState(value)
  useEffect(() => setDraft(value), [value])
  return (
    <Input
      type="text"
      value={draft}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => {
        if (draft !== value) onCommit(draft)
      }}
      onKeyDown={(e) => {
        if (e.key === "Enter") (e.target as HTMLInputElement).blur()
      }}
      placeholder="condition (ST boolean expression)"
      className="h-7 w-56 font-mono text-xs"
      title="ST boolean expression — fires the transition when TRUE"
    />
  )
}

// =================================================================
//   Diagnostics
// =================================================================

/** Classify an SFC diagnostic for the shared `indexDiagnostics`: step
 *  and action errors bucket under the step name; variable errors under
 *  the variable name; everything else is dropped. */
function sfcDiagnosticKey(d: CheckDiagnostic): DiagnosticKey {
  const loc = d.sfc_location
  if (!loc) return null
  if (loc.kind === "step") return { element: loc.name }
  if (loc.kind === "action") return { element: loc.step }
  if (loc.kind === "variable") return { variable: loc.name }
  return null
}

function describeLocation(loc: SfcLocation | null | undefined): string {
  if (!loc) return "—"
  switch (loc.kind) {
    case "variable":
      return `var ${loc.name}`
    case "step":
      return `step ${loc.name}`
    case "action":
      return `step ${loc.step} · action ${loc.action_index}`
    case "transition":
      return `transition #${loc.index}`
  }
}

// (DiagnosticsBanner moved to ./DiagnosticsBanner.tsx — shared with
//  LDEditor and FBDEditor. We keep describeLocation here because the
//  formatter is SFC-specific.)

// =================================================================
//   Helpers
// =================================================================

function qualifierTooltip(q: string): string {
  switch (q) {
    case "N":
      return "N (Non-stored): fires every scan while the step is active"
    case "S":
      return "S (Set): fires once on entry — typically asserts an output"
    case "R":
      return "R (Reset): fires once on entry — typically deasserts an output"
    default:
      return q
  }
}

function safeParse(source: string): ParseResult<SfcProgram> {
  try {
    const obj = JSON.parse(source)
    if (
      !obj ||
      typeof obj !== "object" ||
      !Array.isArray(obj.steps) ||
      !Array.isArray(obj.transitions)
    ) {
      return { kind: "error", message: "missing `steps` or `transitions` array" }
    }
    // Normalise the same way FBDEditor does — older files may have
    // `actions` omitted on a step (backend used to skip empty arrays
    // during serialization).
    if (!Array.isArray(obj.variables)) obj.variables = []
    for (const s of obj.steps) {
      if (s && typeof s === "object" && !Array.isArray(s.actions)) {
        s.actions = []
      }
    }
    return { kind: "ok", program: parseProgram(JSON.stringify(obj)) }
  } catch (e) {
    return { kind: "error", message: String(e) }
  }
}
