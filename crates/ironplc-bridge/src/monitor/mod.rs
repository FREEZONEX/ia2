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

/// Wall-clock microseconds since the UNIX epoch — the human-facing
/// time base for alarm journals and ack stamps.
pub fn wall_now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
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
            if let Err(e) = h.write_variable_ungoverned(&n, 0).await {
                // The program may have stopped in the window — that also
                // clears the variable, so a failed reset is benign then;
                // log it so a live-program failure is still visible.
                tracing::warn!(variable = %n, ?e, "pulse reset write failed");
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
    fn nan_real_falls_back_to_display() {
        let nan_bits = f32::NAN.to_bits() as u64;
        assert_eq!(
            typed_value("REAL", nan_bits, "NaN"),
            serde_json::json!("NaN")
        );
    }
}
