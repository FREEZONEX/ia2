import { afterEach, describe, expect, it, vi } from "vitest"
import { apiFetch, fetchPou, savePou, fetchTasks, updateTasks, documentVersion, withDocumentVersion } from "./api"

afterEach(() => vi.unstubAllGlobals())

describe("project request headers", () => {
  it("sends a Unicode project name and literal percent without a ByteString error", async () => {
    vi.stubGlobal("window", { location: { search: "?project=" + encodeURIComponent("控制 %20") } })
    const fetch = vi.fn().mockResolvedValue(new Response("{}"))
    vi.stubGlobal("fetch", fetch)
    await apiFetch("/api/project")
    const headers = fetch.mock.calls[0][1].headers as Headers
    expect(headers.get("X-IA2-Project")).toBe("%E6%8E%A7%E5%88%B6%20%2520")
    expect(headers.get("X-IA2-Project-Encoding")).toBe("percent")
  })

  it("preserves an explicitly supplied legacy selector without adding an encoding", async () => {
    vi.stubGlobal("window", { location: { search: "?project=ignored" } })
    const fetch = vi.fn().mockResolvedValue(new Response("{}"))
    vi.stubGlobal("fetch", fetch)
    await apiFetch("/api/project", { headers: { "X-IA2-Project": "a%20b" } })
    const headers = fetch.mock.calls[0][1].headers as Headers
    expect(headers.get("X-IA2-Project")).toBe("a%20b")
    expect(headers.has("X-IA2-Project-Encoding")).toBe(false)
  })
})

describe("replacement writes use the draft's version", () => {
  const response = (value: unknown, version: string, header = "ETag") =>
    new Response(JSON.stringify(value), { headers: { [header]: version } })
  const pou = (source: string) => ({ path: "main", source, declarations: [] })

  it("does not let a newer GET or a successful PUT upgrade an old draft", async () => {
    const fetch = vi.fn()
      .mockResolvedValueOnce(response(pou("original"), '"v1"'))
      .mockResolvedValueOnce(response(pou("other writer"), '"v2"'))
      .mockResolvedValueOnce(response({ ok: true }, '"v3"', "X-IA2-Version"))
      .mockResolvedValueOnce(response({ ok: true }, '"v4"', "X-IA2-Version"))
    vi.stubGlobal("fetch", fetch)
    const base = await fetchPou("main")
    const draft = { ...base, source: "my edit" }
    await fetchPou("main")
    const saved = await savePou("main", draft.source, draft)
    const newBase = withDocumentVersion(draft, saved)
    expect(documentVersion(base)).toBe('"v1"')
    expect(documentVersion(draft)).toBe('"v1"')
    expect(documentVersion(newBase)).toBe('"v3"')
    expect((fetch.mock.calls[2][1].headers as Headers).get("If-Match")).toBe('"v1"')
    await savePou("main", "next edit", newBase)
    expect((fetch.mock.calls[3][1].headers as Headers).get("If-Match")).toBe('"v3"')
    expect(JSON.stringify(draft)).toBe(JSON.stringify(pou("my edit")))
  })

  it("keeps tokens on spread task drafts without adding metadata to the payload", async () => {
    const tasks = { tasks: [], programs: [] }
    const fetch = vi.fn()
      .mockResolvedValueOnce(response(tasks, '"tasks-1"'))
      .mockResolvedValueOnce(response({ ok: true }, '"tasks-2"', "X-IA2-Version"))
    vi.stubGlobal("fetch", fetch)
    const base = await fetchTasks()
    const draft = { ...base, tasks: [{ name: "fast", interval_ms: 20, priority: 1 }] }
    await updateTasks(draft)
    const request = fetch.mock.calls[1][1]
    expect((request.headers as Headers).get("If-Match")).toBe('"tasks-1"')
    expect(JSON.parse(request.body)).toEqual({ tasks: draft.tasks, programs: [] })
  })

  it("keeps the draft and never retries a 412 with a fresher token", async () => {
    const fetch = vi.fn()
      .mockResolvedValueOnce(response(pou("original"), '"v1"'))
      .mockResolvedValueOnce(new Response("version mismatch", { status: 412 }))
    vi.stubGlobal("fetch", fetch)
    const base = await fetchPou("main")
    await expect(savePou("main", "my edit", base)).rejects.toThrow(/412.*Local changes have been kept/)
    expect(fetch).toHaveBeenCalledTimes(2)
    expect(documentVersion(base)).toBe('"v1"')
  })

  it("refuses an unversioned full replacement before sending a request", async () => {
    const fetch = vi.fn()
    vi.stubGlobal("fetch", fetch)
    await expect(updateTasks({ tasks: [], programs: [] })).rejects.toThrow(/Reload from disk/)
    expect(fetch).not.toHaveBeenCalled()
  })
})
