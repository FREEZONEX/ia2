import { Plus, X } from "@/components/ui/icons"
import { useEffect, useState } from "react"
import { blockOutputs, removeOutputBinding, setOutputBinding } from "@/lib/fbd-edit"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import type { FbdBlock } from "@/types/generated/FbdBlock"
import type { FbdProgram } from "@/types/generated/FbdProgram"

export function OutputsBar({
  prog,
  onCommit,
}: {
  prog: FbdProgram
  onCommit: (next: FbdProgram) => void
  onClose: () => void
}) {
  // Compact view: list each VAR_OUTPUT binding with a remove button,
  // and a picker to add a new binding. Live values intentionally
  // omitted — the canvas already shows them on the stub wires.
  const outputVars = prog.variables.filter((v) => v.section === "output")
  const unbound = outputVars.filter(
    (v) => !prog.outputs.some((o) => o.variable === v.name),
  )
  return (
    <div className="flex flex-wrap items-center gap-2 border-t border-border bg-muted/10 px-3 py-1.5 text-xs">
      <span className="font-mono text-xs text-muted-foreground">
        outputs
      </span>
      {prog.outputs.map((o) => {
        const srcBlock = prog.blocks.find((b) => b.id === o.from_block)
        return (
          <span
            key={o.variable}
            className="inline-flex items-center gap-1 rounded border border-border bg-card px-1.5 py-0.5 font-mono"
          >
            <span className="text-foreground">{o.variable}</span>
            <span className="text-muted-foreground">
              ← {srcBlock?.instance ?? o.from_block}.{o.from_pin}
            </span>
            <button
              type="button"
              onClick={() => onCommit(removeOutputBinding(prog, o.variable))}
              className="ml-0.5 rounded p-0.5 text-muted-foreground hover:bg-destructive/15 hover:text-destructive"
              title={`Unbind ${o.variable}`}
            >
              <X className="size-3" />
            </button>
          </span>
        )
      })}
      {unbound.length > 0 && prog.blocks.length > 0 && (
        <AddOutputBinding
          unbound={unbound.map((v) => v.name)}
          blocks={prog.blocks}
          onAdd={(variable, fromBlock, fromPin) =>
            onCommit(setOutputBinding(prog, variable, fromBlock, fromPin))
          }
        />
      )}
    </div>
  )
}

function AddOutputBinding({
  unbound,
  blocks,
  onAdd,
}: {
  unbound: string[]
  blocks: FbdBlock[]
  onAdd: (variable: string, fromBlock: string, fromPin: string) => void
}) {
  const [variable, setVariable] = useState(unbound[0])
  const [blockId, setBlockId] = useState(blocks[0]?.id ?? "")
  const block = blocks.find((b) => b.id === blockId) ?? blocks[0]
  const outputs = block ? blockOutputs(block) : ["Q"]
  const [pin, setPin] = useState(outputs[0] ?? "Q")
  useEffect(() => {
    if (!unbound.includes(variable)) setVariable(unbound[0])
  }, [unbound, variable])
  if (!variable || !block) return null
  return (
    <span className="inline-flex items-center gap-1">
      <Select value={variable} onValueChange={setVariable}>
        <SelectTrigger className="h-7 w-24 text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {unbound.map((v) => (
            <SelectItem key={v} value={v}>
              {v}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      <span className="text-muted-foreground">←</span>
      <Select value={blockId} onValueChange={setBlockId}>
        <SelectTrigger className="h-7 w-24 text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {blocks.map((b) => (
            <SelectItem key={b.id} value={b.id}>
              {b.instance}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      <Select value={pin} onValueChange={setPin}>
        <SelectTrigger className="h-7 w-14 text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {outputs.map((p) => (
            <SelectItem key={p} value={p}>
              .{p}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      <button
        type="button"
        onClick={() => onAdd(variable, blockId, pin)}
        className="flex items-center gap-1 rounded border border-highlight/40 bg-highlight/10 px-1.5 py-0.5 text-[13px] text-foreground hover:bg-highlight/20"
      >
        <Plus className="size-3" />
        add
      </button>
    </span>
  )
}
