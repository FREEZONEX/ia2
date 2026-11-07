import type { NodePath } from "@/lib/ld-edit"

export type Selection =
  | { kind: "node"; rungIdx: number; path: NodePath }
  | { kind: "coil"; rungIdx: number; coilIdx: number }
  | { kind: "rung"; rungIdx: number }
  | { kind: "variable"; name: string }
  | null
