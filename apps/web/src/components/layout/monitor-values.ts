import type { VarValue } from "@/types/generated/VarValue"

const WRITE_TYPES = new Set([
  "BOOL", "REAL", "SINT", "INT", "DINT", "USINT", "UINT", "UDINT",
  "BYTE", "WORD", "DWORD",
])

export function canWriteMonitorType(typeName: string): boolean {
  return WRITE_TYPES.has(typeName.toUpperCase())
}

/** Parse display values without silently truncating REALs or guessing zero. */
export function monitorWriteValue(text: string, typeName: string): number {
  if (!canWriteMonitorType(typeName)) throw new Error(`${typeName} writes are not supported`)
  const trimmed = text.trim()
  const value = /^16#[0-9a-f]+$/i.test(trimmed)
    ? Number.parseInt(trimmed.slice(3), 16)
    : Number(trimmed)
  if (!trimmed || !Number.isFinite(value)) throw new Error("Enter a finite number")
  if (typeName.toUpperCase() !== "REAL" && !Number.isInteger(value)) {
    throw new Error(`${typeName} requires a whole number`)
  }
  return value
}

export function currentForceValue(variable: VarValue): number {
  if (variable.type_name.toUpperCase() === "BOOL") {
    if (variable.value === "TRUE") return 1
    if (variable.value === "FALSE") return 0
    throw new Error("BOOL value is unavailable")
  }
  return monitorWriteValue(variable.value, variable.type_name)
}
