// Bit-packing for the runtime write/force surface. Both the IDE server
// (`POST /api/runtime/variables/{name}`) and the edge runtime
// (`POST /write`) take an i32; the VM reads slots untyped, so a REAL
// write must arrive as its f32 bit pattern. Shared by the IDE api layer
// and the standalone HMI panel so the packing can't drift.

export function encodeForWrite(value: number, typeName: string): number {
  const t = typeName.toUpperCase()
  if (t === "REAL") {
    const buf = new ArrayBuffer(4)
    new Float32Array(buf)[0] = value
    return new Int32Array(buf)[0]
  }
  // BOOL, integer family, BYTE/WORD/DWORD all pass through as integers.
  return Math.trunc(value)
}

/** One wording for both hosts when the runtime applied a write but the
 *  device carrying that variable has a dead transport.
 *
 *  Deliberately not "failed": the value DID land in the program and will
 *  flush if the link returns. What the operator must not conclude is that
 *  the plant acted on it. Lives here, beside the encoder, because the IDE
 *  server path and the edge panel path both already come through this
 *  module and must say the same thing. */
export function undeliveredNotice(device: string): string {
  return `Set in the program, but NOT reaching the field — device "${device}" link is down; do not read this as the plant having obeyed`
}

/** Read the undelivered-device caveat out of a write response body.
 *
 *  The edge panel talks to the runtime's `/write`, whose body is hand-built
 *  JSON on the Rust side rather than a generated type — so nothing checks that
 *  this key still matches what the runtime emits. Named and tested on both
 *  sides for that reason: a rename there and a silent loss of the warning here
 *  would otherwise look identical to a healthy write.
 *
 *  Tolerant of a body that is missing, malformed, or from an older runtime
 *  that has no such field: those mean "nothing to report", not "failed". */
export function undeliveredFrom(body: unknown): string | null {
  const device = (body as { undelivered_device?: unknown } | null | undefined)
    ?.undelivered_device
  return typeof device === "string" && device !== "" ? undeliveredNotice(device) : null
}
