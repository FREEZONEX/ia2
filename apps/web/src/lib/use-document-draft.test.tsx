// @vitest-environment jsdom
import { act, cleanup, renderHook } from "@testing-library/react"
import { afterEach, describe, expect, it, vi } from "vitest"
import { documentVersion, fetchTasks } from "./api"
import { useDocumentDraft } from "./use-document-draft"

afterEach(() => { cleanup(); vi.unstubAllGlobals() })

async function document(name: string, version: string) {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({
    tasks: [{ name, interval_ms: 100, priority: 1 }], programs: [],
  }), { headers: { ETag: version } })))
  return fetchTasks()
}

describe("versioned form drafts", () => {
  it("keeps dirty inputs and their old version when a background GET arrives", async () => {
    const original = await document("original", '"v1"')
    const incoming = await document("agent edit", '"v2"')
    const { result, rerender } = renderHook(({ value }) => useDocumentDraft(value, "project"), {
      initialProps: { value: original },
    })
    act(() => result.current.setDraft({ ...original, tasks: [{ ...original.tasks[0], name: "mine" }] }))
    rerender({ value: incoming })
    expect(result.current.draft.tasks[0].name).toBe("mine")
    expect(documentVersion(result.current.draft)).toBe('"v1"')
    expect(result.current.dirty).toBe(true)
    expect(result.current.conflict).toBe(true)
  })

  it("adopts a new version when clean or when it acknowledges this exact draft", async () => {
    const original = await document("original", '"v1"')
    const fresh = await document("fresh", '"v2"')
    const saved = await document("mine", '"v3"')
    const { result, rerender } = renderHook(({ value }) => useDocumentDraft(value, "project"), {
      initialProps: { value: original },
    })
    rerender({ value: fresh })
    expect(documentVersion(result.current.draft)).toBe('"v2"')
    act(() => result.current.setDraft({ ...fresh, tasks: [{ ...fresh.tasks[0], name: "mine" }] }))
    rerender({ value: saved })
    expect(documentVersion(result.current.draft)).toBe('"v3"')
    expect(result.current.dirty).toBe(false)
  })

  it("reloads only after an explicit discard choice and keeps edits on failure", async () => {
    const original = await document("original", '"v1"')
    const fresh = await document("fresh", '"v2"')
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false)
    const load = vi.fn().mockResolvedValue(fresh)
    const { result } = renderHook(() => useDocumentDraft(original, "project"))
    act(() => result.current.setDraft({ ...original, tasks: [] }))
    await act(async () => result.current.reload(load))
    expect(load).not.toHaveBeenCalled()
    confirm.mockReturnValue(true)
    load.mockRejectedValueOnce(new Error("offline"))
    await expect(act(async () => result.current.reload(load))).rejects.toThrow("offline")
    expect(result.current.draft.tasks).toEqual([])
    await act(async () => result.current.reload(load))
    expect(result.current.draft.tasks[0].name).toBe("fresh")
    expect(documentVersion(result.current.draft)).toBe('"v2"')
    confirm.mockRestore()
  })
})
