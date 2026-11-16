import { Plus } from "@/components/ui/icons"
import { fbPinHint, groupedFbs } from "@/lib/ld-fbs"
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
} from "@/components/ui/select"

export function Toolbar({
  readOnly,
  onAdd,
}: {
  readOnly: boolean
  onAdd: (fbType: string) => void
}) {
  if (readOnly) return null
  return (
    <div className="flex flex-wrap items-center gap-2 border-b border-border bg-background px-4 pb-2 text-[13px]">
      <Select
        value=""
        onValueChange={(v) => {
          if (v) onAdd(v)
        }}
      >
        <SelectTrigger
          className="h-8 gap-2 px-3 text-[13px]"
          title="Insert a new function block"
          aria-label="Insert a new function block"
        >
          <Plus className="size-3" />
          Block
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
      <span className="text-xs text-muted-foreground">
        drag a block's header to reposition · click a block to edit
      </span>
    </div>
  )
}
