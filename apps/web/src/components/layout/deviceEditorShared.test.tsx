// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { afterEach, describe, expect, it, vi } from "vitest"

import { LinkedToCell, type LinkProps } from "./deviceEditorShared"

afterEach(cleanup)

describe("device binding removal", () => {
  it("unlinks only the selected device when another device shares the same channel and variable", async () => {
    const first = { application: "pump", variable: "speed", direction: "output" as const, device: "drive_a", channel: "setpoint" }
    const otherDevice = { ...first, device: "drive_b" }
    const otherChannel = { ...first, channel: "backup" }
    const saveIomap = vi.fn().mockResolvedValue(undefined)
    const link: LinkProps = {
      deviceName: "drive_a",
      iomap: { mappings: [first, otherDevice, otherChannel] },
      saveIomap,
      apps: ["pump"],
      varsByApp: {},
    }
    render(<LinkedToCell channelName="setpoint" link={link} />)
    fireEvent.click(screen.getByRole("button", { name: "Unlink" }))
    await waitFor(() => expect(saveIomap).toHaveBeenCalledWith({ mappings: [otherDevice, otherChannel] }))
  })
})
