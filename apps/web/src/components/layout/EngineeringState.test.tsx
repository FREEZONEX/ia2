// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { afterEach, beforeEach, expect, it, vi } from "vitest"
import { EngineeringStateView } from "./EngineeringState"
import { fetchEngineeringHistory, refreshEngineeringState } from "@/lib/api"
import type { EngineeringObservation } from "@/types/generated/EngineeringObservation"
vi.mock("@/lib/api", () => ({ fetchEngineeringHistory: vi.fn(), refreshEngineeringState: vi.fn() }))
const history = vi.mocked(fetchEngineeringHistory), refresh = vi.mocked(refreshEngineeringState)
const observation = (id = "old", device = "bus", retention = "disabled_incremental"): EngineeringObservation => ({
  id, project: "plant", target: { device, slave: 1 }, runtime_session: "session", started_at_unix_ms: 1, finished_at_unix_ms: 2,
  source: "device_sdo_upload", profile: "inovance_sv660n", complete: true, identity_consistent: true, identity_strength: "slot_and_model_only",
  bus_continuity: "not_verified", retention, mechanical_reference: "not_verified", findings: [], parameters: [],
})
const records = (...observations: EngineeringObservation[]) => ({ schema_version: 1, capacity: 64, observations })
const view = (device = "bus") => <EngineeringStateView scope={{ project: "plant", edge: "edge-a" }} device={device} slaves={[1]} />
beforeEach(() => { vi.resetAllMocks(); history.mockResolvedValue(records(observation())) })
afterEach(cleanup)
it("shows stored evidence without issuing a hardware refresh", async () => {
  render(view())
  await screen.findByText(/Incremental · position/)
  expect(screen.getByText(/Recorded evidence · not live/)).toBeTruthy()
  expect(refresh).not.toHaveBeenCalled()
  expect(history.mock.calls[0][0]).toEqual({ project: "plant", edge: "edge-a" })
})
it("keeps failed refresh distinct from historical successful reads", async () => {
  refresh.mockRejectedValue(new Error("No program running"))
  render(view()); await screen.findByText(/Incremental · position/)
  fireEvent.click(screen.getByText("Refresh device observation"))
  expect(await screen.findByRole("alert")).toHaveProperty("textContent", expect.stringContaining("remain historical"))
  expect(screen.getByText(/Incremental · position/)).toBeTruthy()
})
it("shows a failed latest batch as unknown instead of reusing an older value", async () => {
  refresh.mockResolvedValue({ observation: { ...observation("new", "bus", "unknown"), complete: false }, saved: true, storage_error: null })
  render(view()); await screen.findByText(/Incremental · position/)
  fireEvent.click(screen.getByText("Refresh device observation"))
  await screen.findByText("Position retention unknown")
  expect(screen.queryByText(/Incremental · position/)).toBeNull()
})
it("reports fresh but unsaved evidence without claiming durable success", async () => {
  refresh.mockResolvedValue({ observation: observation("new"), saved: false, storage_error: "disk full" })
  render(view()); await screen.findByText(/Incremental · position/)
  fireEvent.click(screen.getByText("Refresh device observation"))
  expect(await screen.findByRole("alert")).toHaveProperty("textContent", expect.stringContaining("storage is unconfirmed: disk full"))
})
it("ignores a late response after changing the selected device", async () => {
  let finish!: (result: Awaited<ReturnType<typeof refreshEngineeringState>>) => void
  refresh.mockImplementation(() => new Promise(resolve => { finish = resolve }))
  const { rerender } = render(view()); await screen.findByText(/Incremental · position/)
  fireEvent.click(screen.getByText("Refresh device observation"))
  history.mockResolvedValue(records())
  rerender(view("different"))
  await screen.findByText(/No observation recorded/)
  await act(async () => finish({ observation: observation("late"), saved: true, storage_error: null }))
  expect(screen.queryByText(/Incremental · position/)).toBeNull()
})
it("does not let a late archive load replace a newer refresh", async () => {
  let finish!: (result: Awaited<ReturnType<typeof fetchEngineeringHistory>>) => void
  history.mockImplementation(() => new Promise(resolve => { finish = resolve }))
  refresh.mockResolvedValue({ observation: observation("new", "bus", "single_turn_only"), saved: true, storage_error: null })
  render(view()); fireEvent.click(screen.getByText("Refresh device observation"))
  await screen.findByText(/Single-turn · multi-turn/)
  await act(async () => finish(records(observation())))
  await waitFor(() => expect(screen.getByText(/Single-turn · multi-turn/)).toBeTruthy())
})
