import type { FailsafeStatus } from "@/types/generated/FailsafeStatus"

type OutputLatchStatus = {
  watchdog_tripped?: boolean | null
  latch_reason?: string | null
  failsafe_status?: FailsafeStatus | null
}

/** A successful adapter call says nothing about a frame or physical output. */
export function failsafeDetail(report: FailsafeStatus | null | undefined): string {
  if (!report) return "Failsafe acknowledgement unavailable; output state is unconfirmed."
  const parts: string[] = []
  if (report.pending.length > 0) parts.push(`Pending: ${report.pending.join(", ")}`)
  if (report.unconfirmed.length > 0) {
    parts.push(`Unconfirmed: ${report.unconfirmed.map(({ device, error }) => `${device} (${error})`).join("; ")}`)
  }
  if (report.acknowledged.length > 0) {
    parts.push(`Adapter acknowledgements: ${report.acknowledged.join(", ")}`)
  }
  if (parts.length === 0) parts.push("No device acknowledgements in the report")
  return `${parts.join(". ")}. Adapter acknowledgements do not confirm wire or physical outputs.`
}

/** `watchdog_tripped` inhibits the normal output phase; it is not confirmation.
 *  Also surface a report during a skewed poll that has not seen the latch yet. */
export function outputLatchMessage(status: OutputLatchStatus): string | null {
  if (!status.watchdog_tripped && !status.failsafe_status) return null
  const reason = status.watchdog_tripped
    ? status.latch_reason
      ? `Normal output phase inhibited — ${status.latch_reason}`
      : "Watchdog tripped — normal output phase inhibited"
    : "Failsafe report"
  return `${reason}. ${failsafeDetail(status.failsafe_status)}`
}
