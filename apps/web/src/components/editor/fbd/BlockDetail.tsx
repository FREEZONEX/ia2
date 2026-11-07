import { Trash2, X } from "@/components/ui/icons"
import { useEffect, useState } from "react"
import { Separator } from "@/components/editor/shared/DetailBar"
import { blockOutputs, setBlockFbType, setBlockInput, setBlockInstance } from "@/lib/fbd-edit"
import { fbByType, fbInputs, fbPinHint, groupedFbs } from "@/lib/ld-fbs"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import type { FbdBlock } from "@/types/generated/FbdBlock"
import type { FbdInputSource } from "@/types/generated/FbdInputSource"
import type { FbdProgram } from "@/types/generated/FbdProgram"

export function BlockDetail({
  prog,
  block,
  onCommit,
  onClose,
  onDelete,
}: {
  prog: FbdProgram
  block: FbdBlock
  onCommit: (next: FbdProgram) => void
  onClose: () => void
  onDelete: () => void
}) {
  const def = fbByType(block.fb_type)
  const inputDefs = fbInputs(block.fb_type)
  const outputs = blockOutputs(block)
  const varsByType = (iecType: string) =>
    prog.variables.filter((v) => v.type === iecType).map((v) => v.name)

  return (
    <div className="flex flex-wrap items-center gap-2 border-t border-highlight/30 bg-highlight/5 px-3 py-1.5 text-xs">
      <span className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs text-muted-foreground">
        block {block.id}
      </span>
      <Select
        value={block.fb_type}
        onValueChange={(v) => onCommit(setBlockFbType(prog, block.id, v))}
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
      <InstanceInput
        value={block.instance}
        onCommit={(v) => onCommit(setBlockInstance(prog, block.id, v))}
      />
      {outputs.length > 0 && (
        <span
          className="font-mono text-xs text-muted-foreground"
          title={def ? def.description : undefined}
        >
          out: {outputs.join(" / ")}
        </span>
      )}
      <Separator />
      {inputDefs.map((pin) => {
        const binding = block.inputs.find((i) => i.pin === pin.pin)
        return (
          <span key={pin.pin} className="inline-flex items-center gap-1">
            <span
              className="font-mono text-xs text-muted-foreground"
              title={`${pin.doc} (${pin.type})`}
            >
              {pin.pin}:
            </span>
            <PinOperandPicker
              value={
                binding?.value ?? {
                  kind: "literal",
                  value: pin.type === "TIME" ? "T#0ms" : "0",
                }
              }
              blockOptions={prog.blocks.filter((b) => b.id !== block.id)}
              variableOptions={varsByType(pin.type)}
              onChange={(v) =>
                onCommit(setBlockInput(prog, block.id, pin.pin, v))
              }
            />
          </span>
        )
      })}
      <span className="ml-auto inline-flex gap-1">
        <button
          type="button"
          onClick={onDelete}
          title="Delete this block"
          className="flex items-center gap-1 rounded border border-destructive/40 bg-destructive/5 px-1.5 py-1 text-[13px] text-destructive hover:bg-destructive/15"
        >
          <Trash2 className="size-3" />
          delete
        </button>
        <button
          type="button"
          onClick={onClose}
          title="Close"
          className="rounded p-0.5 text-muted-foreground hover:bg-accent/40 hover:text-foreground"
        >
          <X className="size-3" />
        </button>
      </span>
    </div>
  )
}

function InstanceInput({
  value,
  onCommit,
}: {
  value: string
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
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(t)) {
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
      className="h-7 w-24 rounded border border-input bg-transparent px-2 font-mono text-xs"
      title="FB instance name"
    />
  )
}

/** Operand picker for an input pin. Three modes:
 *   - var: pick a declared variable (autocomplete by IEC type)
 *   - literal: free-form ST literal
 *   - block: pick another block + one of its output pins (= a wire)
 */
function PinOperandPicker({
  value,
  blockOptions,
  variableOptions,
  onChange,
}: {
  value: FbdInputSource
  blockOptions: FbdBlock[]
  variableOptions: string[]
  onChange: (next: FbdInputSource) => void
}) {
  const [draft, setDraft] = useState(
    value.kind === "literal"
      ? value.value
      : value.kind === "var"
        ? value.name
        : "",
  )
  useEffect(() => {
    setDraft(
      value.kind === "literal"
        ? value.value
        : value.kind === "var"
          ? value.name
          : "",
    )
  }, [value])

  const commitVar = (next: string) => {
    if (next.trim()) onChange({ kind: "var", name: next.trim() })
  }
  const commitLiteral = (next: string) => {
    if (next.trim()) onChange({ kind: "literal", value: next.trim() })
  }

  return (
    <span className="inline-flex gap-1">
      <Select
        value={value.kind}
        onValueChange={(k) => {
          if (k === "var") {
            onChange({
              kind: "var",
              name: draft || variableOptions[0] || "x",
            })
          } else if (k === "literal") {
            onChange({ kind: "literal", value: draft || "0" })
          } else if (k === "block") {
            // Default to first available block's first output pin.
            const src = blockOptions[0]
            if (src) {
              onChange({
                kind: "block",
                block_id: src.id,
                pin: blockOutputs(src)[0] ?? "Q",
              })
            }
          }
        }}
      >
        <SelectTrigger className="h-7 w-14 text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="var">var</SelectItem>
          <SelectItem value="literal">lit</SelectItem>
          <SelectItem value="block" disabled={blockOptions.length === 0}>
            wire
          </SelectItem>
        </SelectContent>
      </Select>
      {value.kind === "block" ? (
        <>
          <Select
            value={value.block_id}
            onValueChange={(id) =>
              onChange({
                kind: "block",
                block_id: id,
                pin:
                  blockOutputs(blockOptions.find((b) => b.id === id)!)[0] ??
                  "Q",
              })
            }
          >
            <SelectTrigger className="h-7 w-24 text-xs">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {blockOptions.map((b) => (
                <SelectItem key={b.id} value={b.id}>
                  {b.instance}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Select
            value={value.pin}
            onValueChange={(p) =>
              onChange({
                kind: "block",
                block_id: value.block_id,
                pin: p,
              })
            }
          >
            <SelectTrigger className="h-7 w-14 text-xs">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {blockOutputs(
                blockOptions.find((b) => b.id === value.block_id) ??
                  blockOptions[0]!,
              ).map((p) => (
                <SelectItem key={p} value={p}>
                  .{p}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </>
      ) : (
        <Input
          type="text"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={() =>
            value.kind === "var" ? commitVar(draft) : commitLiteral(draft)
          }
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              ;(e.target as HTMLInputElement).blur()
            }
          }}
          className="h-7 w-24 font-mono text-xs"
          list={value.kind === "var" ? "fbd-var-options" : undefined}
        />
      )}
      {value.kind === "var" && (
        <datalist id="fbd-var-options">
          {variableOptions.map((o) => (
            <option key={o} value={o} />
          ))}
        </datalist>
      )}
    </span>
  )
}
