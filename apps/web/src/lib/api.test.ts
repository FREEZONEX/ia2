import { afterEach, describe, expect, it, vi } from "vitest"
import { apiFetch } from "./api"

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
