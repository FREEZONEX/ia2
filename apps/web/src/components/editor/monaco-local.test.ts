// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest"

vi.mock("monaco-editor/esm/vs/editor/editor.api.js", () => ({
  editor: { create: vi.fn() },
}))
vi.mock("monaco-editor/esm/vs/editor/edcore.main.js", () => ({}))
vi.mock("monaco-editor/esm/vs/editor/editor.worker?worker", () => ({
  default: class LocalEditorWorker {},
}))

describe("offline Monaco bootstrap", () => {
  it("resolves the installed editor without inserting a CDN loader script", async () => {
    const createElement = vi.spyOn(document, "createElement")
    try {
      await import("./monaco-local")
      const { loader } = await import("@monaco-editor/react")
      const monaco = await import("monaco-editor/esm/vs/editor/editor.api.js")

      // Exercise the actual @monaco-editor/loader initialization contract.
      expect(await loader.init()).toBe(monaco)
      expect(createElement.mock.calls.some(([tag]) => tag === "script")).toBe(false)

      const { default: LocalEditorWorker } = await import(
        "monaco-editor/esm/vs/editor/editor.worker?worker"
      )
      expect(globalThis.MonacoEnvironment?.getWorker?.("", "iec61131"))
        .toBeInstanceOf(LocalEditorWorker)
    } finally {
      createElement.mockRestore()
    }
  })
})
