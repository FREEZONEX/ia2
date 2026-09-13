import * as React from "react"

import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"

export type EnumOption<T extends string> = {
  value: T
  label: React.ReactNode
}

/**
 * Thin wrapper over the shadcn `<Select>` for the common case: a fixed (or
 * caller-supplied) list of string-valued options with a trigger + value.
 * Collapses the ~5-line Trigger/Content/Item boilerplate that was repeated
 * ~20× across the device editors into one call.
 *
 * `className` sizes the trigger. `disabled` disables the trigger only
 * (matching the previous hand-rolled behaviour where the value is preserved
 * but the control is locked).
 */
export function EnumSelect<T extends string>({
  value,
  onValueChange,
  options,
  className,
  disabled,
  placeholder,
  id,
  "aria-labelledby": labelledBy,
}: {
  value: T
  onValueChange: (value: T) => void
  options: ReadonlyArray<EnumOption<T>>
  className?: string
  disabled?: boolean
  placeholder?: string
  id?: string
  "aria-labelledby"?: string
}) {
  return (
    <Select value={value} onValueChange={(v) => onValueChange(v as T)}>
      <SelectTrigger id={id} aria-labelledby={labelledBy} className={className} disabled={disabled}>
        <SelectValue placeholder={placeholder} />
      </SelectTrigger>
      <SelectContent>
        {options.map((o) => (
          <SelectItem key={String(o.value)} value={o.value}>
            {o.label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  )
}
