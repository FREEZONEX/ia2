// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest"

import { invalidationBus, Topic } from "./invalidation"
import { handleMutationEvent } from "./runtime"
import type { MutationEvent } from "@/types/generated/MutationEvent"

afterEach(() => {
  window.history.replaceState(null, "", "/")
})

describe("project close events", () => {
  function deliver(closedProject: string, displayedProject: string | null) {
    const close = vi.fn()
    const refetchTree = vi.fn()
    const unsubscribe = invalidationBus.subscribe(Topic.PROJECT_META, refetchTree)
    const event: MutationEvent = {
      project: closedProject,
      topic: Topic.PROJECT_META,
      detail: { kind: "project_closed" },
    }
    try {
      handleMutationEvent(event, { current: null }, { current: "" }, { current: null }, displayedProject, close)
    } finally {
      unsubscribe()
    }
    return { close, refetchTree }
  }

  it("closes the selected window without fetching a deleted project tree", () => {
    window.history.replaceState(null, "", "/?project=" + encodeURIComponent("控制 工程"))
    const { close, refetchTree } = deliver("控制 工程", "控制 工程")
    expect(close).toHaveBeenCalledOnce()
    expect(refetchTree).not.toHaveBeenCalled()
  })

  it("keeps a different window and its selection intact", () => {
    window.history.replaceState(null, "", "/?project=other")
    const { close, refetchTree } = deliver("closed", "other")
    expect(close).not.toHaveBeenCalled()
    expect(refetchTree).not.toHaveBeenCalled()
    expect(new URL(window.location.href).searchParams.get("project")).toBe("other")
  })

  it("uses the displayed project when a window has no URL selector", () => {
    expect(deliver("other", "shown").close).not.toHaveBeenCalled()
    expect(deliver("shown", "shown").close).toHaveBeenCalledOnce()
    expect(deliver("", null).close).not.toHaveBeenCalled()
  })
})
