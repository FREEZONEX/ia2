/**
 * The canvas's seam between "where the screen lives" and "how it renders".
 * The same HmiCanvas serves two hosts:
 *
 *   - the IDE (HmiPane): documents come from the project server, writes go
 *     through `/api/runtime/...`, nav switches the workbench view;
 *   - the standalone edge panel (HmiStandalone): documents come read-only
 *     from the edge runtime's `/api/hmi`, writes go to its `/write`, nav
 *     swaps the panel URL.
 *
 * Everything the canvas needs from its surroundings passes through this
 * interface, so the edge bundle never imports the IDE's state layer.
 */

import { createContext, useContext } from "react"

import type { AlarmState } from "@/types/generated/AlarmState"
import type { HistoryResponse } from "@/types/generated/HistoryResponse"
import type { HmiDoc } from "@/types/generated/HmiDoc"
import type { RuntimeMode } from "@/types/generated/RuntimeMode"

export type HmiRuntimeState = {
  running: boolean
  /** Active fault / last error to show in the alarm bar; null when calm. */
  alarm: string | null
  /** Scan-loop mode when the host's status carries it — paused/step
   *  must not present as a healthy green run. */
  mode?: RuntimeMode["kind"]
  /** Devices whose transport is down (their input variables are frozen
   *  at last-known values). Empty/absent = fieldbus healthy. */
  unhealthyDevices?: string[]
  /** The runtime's fastest task interval in ms — the cadence at which
   *  `scan_count` can advance. Feeds the live store's freshness budget, so
   *  a slow-cycle project is judged against its own rate instead of a fixed
   *  window. Absent on a runtime that predates the field. */
  scanPeriodMs?: number | null
}

export type HmiHost = {
  fetchDoc(path: string): Promise<HmiDoc>
  /** Persist a whole document (Arrange-mode drag). Absent → layout is
   *  read-only, which is the edge panel's case. */
  saveDoc?(path: string, doc: HmiDoc): Promise<unknown>
  /** Write one variable. `typeName` comes from the live snapshot so the
   *  host can bit-pack REALs correctly. `pulseMs` asks the RUNTIME to
   *  write 0 back after that many ms — the pulse reset must survive the
   *  page, so it is never a client timer.
   *
   *  Resolves to `null` on a clean write, or to a caveat string when the
   *  runtime applied the value but its device's transport is down. That is
   *  a report, never a retry: the value is in the program and will flush if
   *  the link returns, and the operator is told rather than shown a bare
   *  success. */
  write(
    name: string,
    value: number,
    typeName: string,
    pulseMs?: number,
  ): Promise<string | null>
  /** Navigate to another screen (a `nav` action). */
  nav(target: string): void
  /** Polled by the alarm bar (~2 s cadence). */
  runtimeState(): Promise<HmiRuntimeState>
  /** Stored history for `vars` at `stepMs` buckets — seeds trend/
   *  sparkline ring buffers on mount so a reload keeps the trace. */
  history(vars: string[], stepMs: number): Promise<HistoryResponse>
  /** Current alarm states, sorted standing-first (alarmlist node). */
  alarms(): Promise<AlarmState[]>
  /** Acknowledge one alarm; resolves to its updated state. */
  ackAlarm(id: string): Promise<AlarmState>
}

const HmiHostContext = createContext<HmiHost | null>(null)

export const HmiHostProvider = HmiHostContext.Provider

export function useHmiHost(): HmiHost {
  const host = useContext(HmiHostContext)
  if (!host) throw new Error("HmiCanvas must be rendered inside HmiHostProvider")
  return host
}
