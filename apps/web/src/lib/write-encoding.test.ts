import { describe, expect, it } from "vitest"
import { undeliveredFrom } from "./write-encoding"

describe("undeliveredFrom", () => {
  // The runtime's /write body is hand-built JSON on the Rust side; this key
  // is checked there by `write_response_tests` and here. If they ever stop
  // agreeing, a stranded write reads as a clean success on the panel.
  it("reports the device the runtime named", () => {
    expect(undeliveredFrom({ ok: true, name: "stop_cmd", value: 1, undelivered_device: "bus_a" }))
      .toContain("bus_a")
  })
  it.each([
    ["delivered", { ok: true, value: 1, undelivered_device: null }],
    ["an older runtime with no such field", { ok: true, value: 1 }],
    ["a body that failed to parse", null],
    ["a non-string device", { undelivered_device: 7 }],
    ["an empty device name", { undelivered_device: "" }],
  ])("stays silent for %s", (_label, body) => {
    expect(undeliveredFrom(body)).toBeNull()
  })
})
