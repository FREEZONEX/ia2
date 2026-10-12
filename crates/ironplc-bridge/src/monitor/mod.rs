//! The SHARED runtime-monitor core.
//!
//! The IDE server (`crates/server`) and the edge runtime
//! (`crates/runtime`) both expose the same online debug surface —
//! status / snapshot / pause / resume / step / force / unforce /
//! write — over their own HTTP routers. The semantics live HERE, once:
//! the wire shapes both sides serialize, the pulse-reset guarantee, and
//! the typed-value decoding the northbound publisher and alarm
//! evaluation use. The binaries keep only their axum glue.
//!
//! History note: before this module the op-set was implemented three
//! times (server routes, runtime main, plus the server's ssh proxy) and
//! drifted — the HMI pulse-reset bug was exactly that class of failure.

pub mod alarms;
pub mod history;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub use alarms::{AlarmEngine, AlarmJournalEntry, AlarmState};
pub use history::{Historian, HistoryPoint, HistoryResponse, HistorySeries};

use crate::runtime::{ProgramHandle, RuntimeMode, RuntimeWriteError, WriteOutcome};

/// One pinned (forced) variable in the runtime's debug state.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ForceEntry {
    pub name: String,
    pub value: i32,
}

/// Response of every mode-changing op (pause / resume / step).
#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct ModeResponse {
    pub mode: RuntimeMode,
}

/// Body of a step request. Defaults to 1 cycle — "step" without a
/// count is the common case.
#[derive(Debug, Deserialize, TS)]
#[ts(export)]
pub struct StepRequest {
    #[serde(default = "default_step_cycles")]
    pub cycles: u32,
}

fn default_step_cycles() -> u32 {
    1
}

/// How the edge runtime's snapshot tap is doing: the task that takes each
/// snapshot off the scan loop and feeds the historian, the alarm engine, the
/// latest-snapshot slot behind `/status` and the `/events` stream.
///
/// The scan loop is a separate thread. When the tap stops, or fails on every
/// snapshot, the program keeps running and `/health` keeps answering `ok`,
/// while history stops recording, process alarms stop being evaluated and
/// `scan_count` stops moving: a monitoring probe that only checks `status`
/// would call that runtime healthy. Hence its own field, like
/// `watchdog_tripped` and `fault`.
///
/// `Deserialize` as well as `Serialize`: the IDE server parses this back out
/// of a remote edge runtime's `/health` when probing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SnapshotTapHealth {
    /// `false` once the tap has ended and will not come back until the runtime
    /// restarts: nothing is feeding history, alarms or `/status` any more.
    pub running: bool,
    /// Incidents since the runtime started: a stage that panicked on a
    /// snapshot, or snapshots skipped because the tap fell behind the scan
    /// loop. A count that climbs between two polls means it is failing now.
    #[ts(type = "number")]
    pub faults: u64,
    /// The latest incident in words, `null` when there has been none.
    pub last_fault: Option<String>,
}

/// Wall-clock microseconds since the UNIX epoch — the human-facing
/// time base for alarm journals and ack stamps.
pub fn wall_now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// The text of a caught panic payload (`panic!("literal")` carries a `&str`;
/// a formatted `panic!` or an `expect`/`unwrap` failure carries a `String`).
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    crate::runtime::panic_message(payload)
}

/// The body both HTTP servers (IDE server, edge runtime) answer with when a
/// request handler panicked: the panic text, and what the caller can rely on.
///
/// Without an answer the connection just closes, which a client cannot tell
/// from a network fault. The handler may have got part of the way before it
/// panicked, so the text does not claim that nothing happened. Logs the panic
/// at ERROR; one implementation, so the two surfaces cannot drift (the
/// status stays each server's own `500`).
pub fn handler_panic_body(payload: &(dyn std::any::Any + Send)) -> String {
    let detail = panic_message(payload);
    tracing::error!(%detail, "a request handler panicked; answering 500");
    format!(
        "internal error: the request handler panicked ({detail}). This is a server bug. \
         The request did not finish; if it was a write, re-read the resource before retrying."
    )
}

/// Snapshot of the currently-forced variables.
pub fn force_entries(handle: &ProgramHandle) -> Vec<ForceEntry> {
    let mut entries: Vec<ForceEntry> = handle
        .forces()
        .into_iter()
        .map(|(name, value)| ForceEntry { name, value })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Pause / resume / step share one shape: mutate, then report the mode.
pub fn pause(handle: &ProgramHandle) -> ModeResponse {
    handle.pause();
    ModeResponse {
        mode: handle.mode(),
    }
}

pub fn resume(handle: &ProgramHandle) -> ModeResponse {
    handle.resume();
    ModeResponse {
        mode: handle.mode(),
    }
}

pub fn step(handle: &ProgramHandle, cycles: u32) -> ModeResponse {
    handle.step(cycles);
    ModeResponse {
        mode: handle.mode(),
    }
}

/// One-shot variable write with the OPTIONAL momentary-pulse contract:
/// after `pulse_ms` the RUNTIME writes 0 back, so a closed operator tab
/// or suspended tablet can't leave a momentary command latched.
/// Overlapping pulses on one variable are latest-write-wins.
///
/// This is the single implementation of that safety guarantee — the
/// IDE server and the edge runtime both call it.
///
/// The initial write is governed like any external write; the reset is
/// the runtime's OWN safety action, not an external write, so it
/// bypasses governance (same rationale as the force bypass, ADR-0002).
/// Otherwise a rule with `min > 0` would clamp the reset up to `min`
/// and silently latch the momentary command forever.
pub async fn write_with_pulse(
    handle: &ProgramHandle,
    name: &str,
    value: i32,
    pulse_ms: Option<u32>,
) -> Result<WriteOutcome, RuntimeWriteError> {
    let v = handle.write_variable(name, value).await?;
    if let Some(ms) = pulse_ms {
        let h = handle.clone();
        let n = name.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(ms as u64)).await;
            match h.write_variable_ungoverned(&n, 0).await {
                // The program may have stopped in the window — that also
                // clears the variable, so a failed reset is benign then;
                // log it so a live-program failure is still visible.
                Err(e) => tracing::warn!(variable = %n, ?e, "pulse reset write failed"),
                // Applied to the VM but NOT reaching the field: the link went
                // down inside the pulse window. The momentary value is still
                // on the wire from the first half of this write, and stays
                // there for the outage — which is exactly the latch `pulse_ms`
                // exists to prevent, so it cannot pass silently.
                //
                // Self-correcting, not permanent: the output phase flushes the
                // current VM value every scan, so the 0 lands when the link
                // returns. What is lost without this line is any record that
                // the plant held the command in between.
                Ok(o) => {
                    if let Some(device) = o.undelivered_device {
                        tracing::warn!(
                            variable = %n,
                            %device,
                            "pulse reset applied to the VM but not delivered — that \
                             device's link is down, so the field holds the momentary \
                             value until it returns"
                        );
                    }
                }
            }
        });
    }
    Ok(v)
}

/// Decode a variable's raw 64-bit slot into a typed JSON value, given
/// its IEC type name. This is what the northbound publisher feeds the
/// plant platform and what alarm expressions evaluate against — one
/// decoder, not N re-parsers of display strings.
///
/// `display` is the IDE-facing formatted string; it is the fallback for
/// types without a numeric wire form (STRING/WSTRING and anything
/// unknown), passed through as a JSON string.
pub fn typed_value(type_name: &str, bits: u64, display: &str) -> serde_json::Value {
    let t = type_name.to_ascii_uppercase();
    match t.as_str() {
        "BOOL" => serde_json::Value::Bool(bits != 0),
        "REAL" => {
            let f = f32::from_bits(bits as u32) as f64;
            number_or_string(f, display)
        }
        "LREAL" => {
            let f = f64::from_bits(bits);
            number_or_string(f, display)
        }
        // Signed integer family: the VM stores sign-extended values in
        // the low 32 bits of the slot.
        "SINT" | "INT" | "DINT" => serde_json::json!(bits as u32 as i32 as i64),
        "LINT" => serde_json::json!(bits as i64),
        // Unsigned + bit-string family.
        "USINT" | "BYTE" => serde_json::json!(bits as u8),
        "UINT" | "WORD" => serde_json::json!(bits as u16),
        "UDINT" | "DWORD" => serde_json::json!(bits as u32),
        "ULINT" | "LWORD" => serde_json::json!(bits),
        "TIME" => serde_json::json!(bits as u32 as i32 as i64),
        _ => serde_json::Value::String(display.to_string()),
    }
}

/// JSON has no NaN/Infinity — fall back to the display string so the
/// value is never silently dropped or nulled.
fn number_or_string(f: f64, display: &str) -> serde_json::Value {
    match serde_json::Number::from_f64(f) {
        Some(n) => serde_json::Value::Number(n),
        None => serde_json::Value::String(display.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_values_decode_by_iec_type() {
        assert_eq!(typed_value("BOOL", 1, "TRUE"), serde_json::json!(true));
        assert_eq!(typed_value("BOOL", 0, "FALSE"), serde_json::json!(false));
        assert_eq!(
            typed_value("REAL", 12.5f32.to_bits() as u64, "12.5"),
            serde_json::json!(12.5)
        );
        assert_eq!(
            typed_value("LREAL", (-2.25f64).to_bits(), "-2.25"),
            serde_json::json!(-2.25)
        );
        // -42 sign-extended into a 32-bit slot.
        assert_eq!(
            typed_value("DINT", (-42i32) as u32 as u64, "-42"),
            serde_json::json!(-42)
        );
        assert_eq!(
            typed_value("WORD", 0x1637, "16#1637"),
            serde_json::json!(0x1637)
        );
        assert_eq!(
            typed_value("STRING", 0, "'hello'"),
            serde_json::json!("'hello'")
        );
    }

    #[test]
    fn handler_panic_body_carries_the_panic_text_and_the_retry_advice() {
        let literal = std::panic::catch_unwind(|| panic!("no group at index")).unwrap_err();
        let body = handler_panic_body(literal.as_ref());
        assert!(body.contains("no group at index"), "{body}");
        assert!(body.contains("handler panicked"), "{body}");
        assert!(body.contains("re-read the resource"), "{body}");

        let formatted = std::panic::catch_unwind(|| {
            let n = 2;
            panic!("no group at index '{n}'")
        })
        .unwrap_err();
        assert!(handler_panic_body(formatted.as_ref()).contains("index '2'"));

        // A payload that is not text still gets a body, not a second panic.
        let other = std::panic::catch_unwind(|| std::panic::panic_any(42_u32)).unwrap_err();
        assert!(handler_panic_body(other.as_ref()).contains("non-string panic payload"));
    }

    #[test]
    fn nan_real_falls_back_to_display() {
        let nan_bits = f32::NAN.to_bits() as u64;
        assert_eq!(
            typed_value("REAL", nan_bits, "NaN"),
            serde_json::json!("NaN")
        );
    }
}
