import { describe, expect, it } from "vitest"
import { pouDocumentUri } from "./lsp-client"

describe("POU document URI", () => {
  it("preserves nested logical paths without a host drive dependency", () => {
    expect(pouDocumentUri("application/main")).toBe("file:///application/main.st")
  })

  it("keeps Unicode and URI delimiters inside the path", () => {
    const uri = pouDocumentUri("控制/主 程序#1%")
    const parsed = new URL(uri)
    expect(parsed.hash).toBe("")
    expect(parsed.search).toBe("")
    expect(decodeURIComponent(parsed.pathname)).toBe("/控制/主 程序#1%.st")
  })
})
