import { Plus, Trash2, X } from "@/components/ui/icons"
import { useEffect, useRef, useState } from "react"
import { ActionBtn, DetailLabel, Separator } from "@/components/editor/shared/DetailBar"
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { cn } from "@/lib/utils"
import {
  addInParallel,
  addInSeries,
  deleteCoil,
  deleteNode,
  newCompare,
  newContact,
  newFbCall,
  setCoilKind,
  setCoilVar,
  setContactVar,
  setFbInputValue,
  setFbType,
  toggleNegated,
  updateCompare,
  updateFbCall,
  type NodePath,
} from "@/lib/ld-edit"
import { fbByType, fbInputs, fbOutputs, fbPinHint, groupedFbs } from "@/lib/ld-fbs"
import type { LdCoilKind } from "@/types/generated/LdCoilKind"
import type { LdNode } from "@/types/generated/LdNode"
import type { LdProgram } from "@/types/generated/LdProgram"
import type { Selection } from "./selection"

export function SelectionDetail({
  prog,
  rungIdx,
  rung,
  selection,
  readOnly,
  onClose,
  onCommit,
}: {
  prog: LdProgram
  rungIdx: number
  rung: LdProgram["rungs"][number]
  selection: Selection
  readOnly: boolean
  onClose: () => void
  onCommit: (next: LdProgram) => void
}) {
  if (!selection || readOnly) return null
  if (selection.kind === "node" && selection.rungIdx === rungIdx) {
    return (
      <NodeDetail
        prog={prog}
        rungIdx={rungIdx}
        rung={rung}
        path={selection.path}
        onClose={onClose}
        onCommit={onCommit}
      />
    )
  }
  if (selection.kind === "coil" && selection.rungIdx === rungIdx) {
    return (
      <CoilDetail
        prog={prog}
        rungIdx={rungIdx}
        rung={rung}
        coilIdx={selection.coilIdx}
        onClose={onClose}
        onCommit={onCommit}
      />
    )
  }
  return null
}

function NodeDetail({
  prog,
  rungIdx,
  rung,
  path,
  onClose,
  onCommit,
}: {
  prog: LdProgram
  rungIdx: number
  rung: LdProgram["rungs"][number]
  path: NodePath
  onClose: () => void
  onCommit: (next: LdProgram) => void
}) {
  // Walk the path to find the selected node. Defensive against
  // stale selections that survive a structural edit by one frame.
  let node: LdNode | undefined = rung.logic
  try {
    for (const step of path) {
      if (!node) break
      if (node.op === "and" || node.op === "or") node = node.args[step]
      else if (node.op === "not") node = node.arg
      else node = undefined
    }
  } catch {
    node = undefined
  }
  if (!node) return null

  const boolVars = prog.variables
    .filter((v) => v.type === "BOOL" || v.type === "")
    .map((v) => v.name)

  return (
    <DetailBar onClose={onClose}>
      {node.op === "contact" ? (
        <>
          <DetailLabel>contact</DetailLabel>
          <VarPicker
            value={node.var}
            options={boolVars}
            onChange={(v) => onCommit(setContactVar(prog, rungIdx, path, v))}
          />
          <ToggleBtn
            active={node.negated}
            onClick={() => onCommit(toggleNegated(prog, rungIdx, path))}
            title="Toggle normally-closed"
          >
            ¬ negate
          </ToggleBtn>
        </>
      ) : node.op === "compare" ? (
        <CompareEditFields
          prog={prog}
          rungIdx={rungIdx}
          path={path}
          node={node}
          onCommit={onCommit}
        />
      ) : node.op === "fb_call" ? (
        <FbCallEditFields
          prog={prog}
          rungIdx={rungIdx}
          path={path}
          node={node}
          onCommit={onCommit}
        />
      ) : node.op === "const" ? (
        <>
          <DetailLabel>const</DetailLabel>
          <span className="font-mono text-xs">
            {node.value ? "TRUE" : "FALSE"}
          </span>
        </>
      ) : (
        <>
          <DetailLabel>{node.op}</DetailLabel>
          <span className="text-xs text-muted-foreground">
            {"args" in node && Array.isArray(node.args)
              ? `${node.args.length} branches`
              : "1 child"}
          </span>
        </>
      )}

      <Separator />
      <ActionBtn
        onClick={() =>
          onCommit(
            addInSeries(prog, rungIdx, path, "after", newContact()),
          )
        }
        title="Insert a contact in series to the right"
      >
        <Plus className="size-3" />
        contact
      </ActionBtn>
      <ActionBtn
        onClick={() =>
          onCommit(addInSeries(prog, rungIdx, path, "after", newCompare()))
        }
        title="Insert a compare block in series to the right"
      >
        <Plus className="size-3" />
        compare
      </ActionBtn>
      <FbInsertPicker
        title="Insert a function block in series to the right"
        onPick={(type) => {
          const { node: fb } = newFbCall(prog, type)
          onCommit(addInSeries(prog, rungIdx, path, "after", fb))
        }}
        label="fb"
      />
      <Separator />
      <ActionBtn
        onClick={() =>
          onCommit(
            addInParallel(prog, rungIdx, path, "after", newContact()),
          )
        }
        title="Insert a contact in parallel below"
      >
        <Plus className="size-3" />
        ⫽contact
      </ActionBtn>
      <ActionBtn
        onClick={() =>
          onCommit(addInParallel(prog, rungIdx, path, "after", newCompare()))
        }
        title="Insert a compare block in parallel below"
      >
        <Plus className="size-3" />
        ⫽compare
      </ActionBtn>
      <FbInsertPicker
        title="Insert a function block in parallel below"
        onPick={(type) => {
          const { node: fb } = newFbCall(prog, type)
          onCommit(addInParallel(prog, rungIdx, path, "after", fb))
        }}
        label="⫽fb"
      />
      <Separator />
      <ActionBtn
        onClick={() => {
          onCommit(deleteNode(prog, rungIdx, path))
          onClose()
        }}
        title="Delete this element"
        destructive
      >
        <Trash2 className="size-3" />
        delete
      </ActionBtn>
    </DetailBar>
  )
}

/** Compact editing controls for a Compare block:
 *  [left-kind ↓] [left-value]  [cmp ↓]  [right-kind ↓] [right-value]
 *  Each operand chooses between var-reference or inline literal. */
function CompareEditFields({
  prog,
  rungIdx,
  path,
  node,
  onCommit,
}: {
  prog: LdProgram
  rungIdx: number
  path: NodePath
  node: Extract<LdNode, { op: "compare" }>
  onCommit: (next: LdProgram) => void
}) {
  const numericVars = prog.variables
    .filter((v) => v.type !== "BOOL")
    .map((v) => v.name)
  return (
    <>
      <DetailLabel>compare</DetailLabel>
      <OperandPicker
        operand={node.left}
        options={numericVars}
        onChange={(left) =>
          onCommit(updateCompare(prog, rungIdx, path, { left }))
        }
      />
      <Select
        value={node.cmp}
        onValueChange={(v) =>
          onCommit(updateCompare(prog, rungIdx, path, { cmp: v as typeof node.cmp }))
        }
      >
        <SelectTrigger className="h-7 w-16 text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="eq">=</SelectItem>
          <SelectItem value="ne">≠</SelectItem>
          <SelectItem value="lt">&lt;</SelectItem>
          <SelectItem value="le">≤</SelectItem>
          <SelectItem value="gt">&gt;</SelectItem>
          <SelectItem value="ge">≥</SelectItem>
        </SelectContent>
      </Select>
      <OperandPicker
        operand={node.right}
        options={numericVars}
        onChange={(right) =>
          onCommit(updateCompare(prog, rungIdx, path, { right }))
        }
      />
    </>
  )
}

/**
 * Editor row for an FbCall node. Three groups of controls:
 *
 *   1. Identity:  `[FB type ↓]  instance-name-input  → [output-pin ↓]`
 *   2. Pins:      one row per input pin, each an OperandPicker preceded
 *                 by the pin name.
 *
 * Layout is horizontal — for an FB with many pins (CTUD has 5 inputs)
 * the row wraps; that's fine, this bar already breaks for long Compare
 * blocks. Tight + scannable beats fancy + scrolling for editor work.
 */
function FbCallEditFields({
  prog,
  rungIdx,
  path,
  node,
  onCommit,
}: {
  prog: LdProgram
  rungIdx: number
  path: NodePath
  node: Extract<LdNode, { op: "fb_call" }>
  onCommit: (next: LdProgram) => void
}) {
  const def = fbByType(node.fb_type)
  const inputs = fbInputs(node.fb_type)
  // BOOL outputs are the only pins that can feed the surrounding
  // network. Most FBs only have one, but CTUD has two and we want to
  // let the user pick.
  const boolOutputs = fbOutputs(node.fb_type).filter((p) => p.type === "BOOL")
  // For each input pin's OperandPicker, pre-filter variable options
  // by IEC type so users only see relevant choices.
  const varsByType = (iecType: string) =>
    prog.variables.filter((v) => v.type === iecType).map((v) => v.name)
  return (
    <>
      <DetailLabel>fb</DetailLabel>
      <Select
        value={node.fb_type}
        onValueChange={(v) => onCommit(setFbType(prog, rungIdx, path, v))}
      >
        <SelectTrigger className="h-7 w-28 text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {groupedFbs().map((group) => (
            <SelectGroup key={group.label}>
              <SelectLabel>{group.label}</SelectLabel>
              {group.fbs.map((fb) => (
                <SelectItem key={fb.type} value={fb.type} title={fbPinHint(fb)}>
                  {fb.type}
                </SelectItem>
              ))}
            </SelectGroup>
          ))}
        </SelectContent>
      </Select>
      <InstanceNameInput
        value={node.instance}
        onCommit={(v) => onCommit(updateFbCall(prog, rungIdx, path, { instance: v }))}
      />
      {boolOutputs.length > 1 ? (
        <Select
          value={node.output_pin}
          onValueChange={(v) =>
            onCommit(updateFbCall(prog, rungIdx, path, { output_pin: v }))
          }
        >
          <SelectTrigger className="h-7 w-16 text-xs" title="Output pin">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {boolOutputs.map((p) => (
              <SelectItem key={p.pin} value={p.pin}>
                .{p.pin}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      ) : (
        <span
          className="font-mono text-xs text-muted-foreground"
          title={def ? def.description : "Custom FB"}
        >
          .{node.output_pin}
        </span>
      )}
      {inputs.map((pin) => {
        const binding = node.inputs.find((i) => i.pin === pin.pin)
        return (
          <span key={pin.pin} className="inline-flex items-center gap-1">
            <span
              className="font-mono text-xs text-muted-foreground"
              title={`${pin.doc} (${pin.type})`}
            >
              {pin.pin}:
            </span>
            <OperandPicker
              operand={
                binding?.value ?? {
                  kind: "literal" as const,
                  value: pin.type === "TIME" ? "T#0ms" : "0",
                }
              }
              options={varsByType(pin.type)}
              onChange={(v) =>
                onCommit(setFbInputValue(prog, rungIdx, path, pin.pin, v))
              }
            />
          </span>
        )
      })}
    </>
  )
}

/** Compact instance-name editor — same pattern as OperandPicker's text
 *  input: local draft, commit on blur / Enter. Validation is "must be
 *  a legal IEC identifier"; non-conforming names get rejected silently
 *  on commit (the caller's set never lands). */
function InstanceNameInput({
  value,
  onCommit,
}: {
  value: string
  onCommit: (next: string) => void
}) {
  const [draft, setDraft] = useState(value)
  useEffect(() => setDraft(value), [value])
  const commit = (next: string) => {
    const trimmed = next.trim()
    if (!trimmed || trimmed === value) {
      setDraft(value)
      return
    }
    // Cheap IEC identifier check — letters/digits/underscore, must not
    // start with a digit. Anything else snaps back to the previous
    // value rather than committing garbage.
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(trimmed)) {
      setDraft(value)
      return
    }
    onCommit(trimmed)
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
      className="h-7 w-24 rounded border border-input bg-transparent px-2 font-mono text-xs"
      title="FB instance name"
    />
  )
}

/** A button-shaped Select that lets the user pick an FB type and
 *  inserts a new FbCall when chosen. Used by the NodeDetail action
 *  row. Renders identical-looking to ActionBtn so it slots in beside
 *  "+ contact" / "+ compare" without visual jolt. */
function FbInsertPicker({
  title,
  onPick,
  label,
}: {
  title: string
  onPick: (fbType: string) => void
  label: string
}) {
  return (
    <Select
      // Force a remount each time the user picks something so the
      // select returns to its placeholder state instead of getting
      // stuck on the last picked type.
      value=""
      onValueChange={(v) => {
        if (v) onPick(v)
      }}
    >
      <SelectTrigger
        className="h-7 gap-1 px-2 text-xs"
        title={title}
        aria-label={title}
      >
        <Plus className="size-3" />
        {label}
      </SelectTrigger>
      <SelectContent>
        {groupedFbs().map((group) => (
          <SelectGroup key={group.label}>
            <SelectLabel>{group.label}</SelectLabel>
            {group.fbs.map((fb) => (
              <SelectItem key={fb.type} value={fb.type} title={fbPinHint(fb)}>
                <span className="font-mono">{fb.type}</span>
                <span className="ml-2 text-muted-foreground">
                  {fb.label.replace(`${fb.type} — `, "")}
                </span>
              </SelectItem>
            ))}
          </SelectGroup>
        ))}
      </SelectContent>
    </Select>
  )
}

/** A single operand picker: a small select to choose var-vs-literal,
 *  then a text input for the name / literal value. */
function OperandPicker({
  operand,
  options,
  onChange,
}: {
  operand: { kind: "var"; name: string } | { kind: "literal"; value: string }
  options: string[]
  onChange: (next: { kind: "var"; name: string } | { kind: "literal"; value: string }) => void
}) {
  const [draft, setDraft] = useState(
    operand.kind === "var" ? operand.name : operand.value,
  )
  useEffect(() => {
    setDraft(operand.kind === "var" ? operand.name : operand.value)
  }, [operand])
  const commit = (next: string) => {
    if (operand.kind === "var" && next !== operand.name) {
      onChange({ kind: "var", name: next })
    } else if (operand.kind === "literal" && next !== operand.value) {
      onChange({ kind: "literal", value: next })
    }
  }
  return (
    <span className="inline-flex gap-1">
      <Select
        value={operand.kind}
        onValueChange={(k) =>
          onChange(
            k === "var"
              ? { kind: "var", name: draft || options[0] || "x" }
              : { kind: "literal", value: draft || "0" },
          )
        }
      >
        <SelectTrigger className="h-7 w-14 text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="var">var</SelectItem>
          <SelectItem value="literal">lit</SelectItem>
        </SelectContent>
      </Select>
      <input
        type="text"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={() => commit(draft.trim())}
        onKeyDown={(e) => {
          if (e.key === "Enter") commit(draft.trim())
        }}
        className="h-7 w-24 rounded border border-input bg-transparent px-2 font-mono text-xs"
        list={
          operand.kind === "var" ? "ld-compare-var-options" : undefined
        }
      />
      {operand.kind === "var" && (
        <datalist id="ld-compare-var-options">
          {options.map((o) => (
            <option key={o} value={o} />
          ))}
        </datalist>
      )}
    </span>
  )
}

function CoilDetail({
  prog,
  rungIdx,
  rung,
  coilIdx,
  onClose,
  onCommit,
}: {
  prog: LdProgram
  rungIdx: number
  rung: LdProgram["rungs"][number]
  coilIdx: number
  onClose: () => void
  onCommit: (next: LdProgram) => void
}) {
  const coil = rung.coils[coilIdx]
  if (!coil) return null
  const candidates = prog.variables
    .filter((v) => v.type === "BOOL")
    .map((v) => v.name)
  return (
    <DetailBar onClose={onClose}>
      <DetailLabel>coil</DetailLabel>
      <VarPicker
        value={coil.var}
        options={candidates}
        onChange={(v) => onCommit(setCoilVar(prog, rungIdx, coilIdx, v))}
      />
      <Select
        value={coil.kind}
        onValueChange={(v) =>
          onCommit(setCoilKind(prog, rungIdx, coilIdx, v as LdCoilKind))
        }
      >
        <SelectTrigger className="h-7 w-32 text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="standard">standard ( )</SelectItem>
          <SelectItem value="set">set (S)</SelectItem>
          <SelectItem value="reset">reset (R)</SelectItem>
        </SelectContent>
      </Select>
      <Separator />
      <ActionBtn
        onClick={() => {
          onCommit(deleteCoil(prog, rungIdx, coilIdx))
          onClose()
        }}
        title="Remove this coil"
        destructive
      >
        <Trash2 className="size-3" />
        delete
      </ActionBtn>
    </DetailBar>
  )
}

function DetailBar({
  onClose,
  children,
}: {
  onClose: () => void
  children: React.ReactNode
}) {
  return (
    <div className="flex items-center gap-2 border-t border-highlight/30 bg-highlight/5 px-3 py-1.5 text-xs">
      {children}
      <button
        type="button"
        onClick={onClose}
        className="ml-auto rounded p-0.5 text-muted-foreground hover:bg-accent/40 hover:text-foreground"
        title="Close"
      >
        <X className="size-3" />
      </button>
    </div>
  )
}

function ToggleBtn({
  active,
  onClick,
  title,
  children,
}: {
  active: boolean
  onClick: () => void
  title?: string
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={title}
      className={cn(
        "inline-flex items-center gap-1 rounded px-1.5 py-0.5 font-mono text-xs transition-colors",
        active
          ? "bg-highlight/15 text-highlight"
          : "text-muted-foreground hover:bg-accent/40 hover:text-foreground",
      )}
    >
      {children}
    </button>
  )
}

/** Combo input + datalist. Avoids the overhead of a real Select
 *  for the common case ("type the variable name and pick a suggestion
 *  from declared variables").
 *
 *  Internal draft state so typing doesn't trigger a JSON serialise /
 *  parse round-trip on every keystroke. We commit on blur / Enter,
 *  and reset the draft when `value` changes externally (e.g. user
 *  selects a different element). */
function VarPicker({
  value,
  options,
  onChange,
}: {
  value: string
  options: string[]
  onChange: (v: string) => void
}) {
  const id = useRef(`varpicker-${Math.random().toString(36).slice(2, 8)}`)
  const [draft, setDraft] = useState(value)
  useEffect(() => {
    setDraft(value)
  }, [value])
  return (
    <>
      <input
        list={id.current}
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={() => {
          const next = draft.trim()
          if (next && next !== value) onChange(next)
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            const next = draft.trim()
            if (next && next !== value) onChange(next)
          } else if (e.key === "Escape") {
            setDraft(value)
            ;(e.target as HTMLInputElement).blur()
          }
        }}
        className="h-7 w-32 rounded border border-input bg-transparent px-2 font-mono text-xs"
      />
      <datalist id={id.current}>
        {options.map((o) => (
          <option key={o} value={o} />
        ))}
      </datalist>
    </>
  )
}
