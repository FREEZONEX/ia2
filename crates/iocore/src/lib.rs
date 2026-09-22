//! The trait an external fieldbus adapter (Modbus, EtherCAT, …) implements
//! so the ironplc VM scan loop can read inputs before `run_round` and write
//! outputs after.
//!
//! Lives in its own crate so concrete adapters (`iomap-modbus`,
//! `iomap-ethercat`) and the ironplc-bridge runtime can all depend on it
//! without forming a cycle.

mod health;

use async_trait::async_trait;
use serde::Serialize;
use thiserror::Error;

pub use health::{HealthTracker, HealthTransition};

/// One field value crossing the adapter boundary.
///
/// **Signedness lives in the variant, not in the bits.** There is no
/// signed-8 or signed-16 lane: a negative value of *any* width is carried
/// as [`ChannelValue::I32`], sign-extended. `U16` means an unsigned
/// number, and every consumer reads it that way — `to_i32` widens it
/// without sign extension, so a two's-complement bit pattern parked in
/// `U16` is read back as a large positive number.
///
/// This is the rule the bridge already depends on: its output direction
/// (`value_for_type`) maps every signed IEC type to `I32` and only the
/// unsigned ones to `U16`. An adapter that decodes a signed field into
/// `U16` therefore breaks the round trip — the value it writes to the
/// wire comes back as a different number.
///
/// The failure is invisible on narrow variables and only appears on wide
/// ones: a 16-bit pattern written to an `INT` is truncated back to 16
/// bits by the VM and reads correctly by accident, while the same value
/// in a `DINT`, `LINT`, `REAL` or `LREAL` is off by 65536.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub enum ChannelValue {
    Bool(bool),
    /// An **unsigned** integer up to 16 bits (U8/USINT, U16/UINT, BYTE,
    /// WORD). Never a signed value: see the type-level note above.
    U16(u16),
    /// A **signed** integer up to 32 bits — the lane for every signed
    /// field regardless of its width on the wire (I8, I16, I32), and the
    /// carrier for unsigned 32-bit fields, whose bit pattern round-trips
    /// even though the numeric view may be negative.
    I32(i32),
    /// IEEE-754 single — analog values (OPC UA Float tags, EtherCAT REAL
    /// PDOs, scaled 4-20 mA). Carried as a real float so fractional
    /// process values (12.7 m³/h) survive the trip to a REAL PLC var.
    Real(f32),
    /// IEEE-754 double — OPC UA Double tags and any other 64-bit analog
    /// source, bound to LREAL PLC vars without precision loss. (`Real`
    /// keeps its historical name = f32.)
    F64(f64),
}

impl ChannelValue {
    /// Coerce to a *numeric* i32 (floats are truncated). Display/legacy
    /// lane — for feeding the VM use `to_vm_bits`, which respects the
    /// target variable's type.
    pub fn to_i32(self) -> i32 {
        match self {
            Self::Bool(b) => b as i32,
            Self::U16(v) => v as i32,
            Self::I32(v) => v,
            Self::Real(f) => f as i32,
            Self::F64(f) => f as i32,
        }
    }

    /// Numeric f32 view (integers convert by value, doubles narrow).
    pub fn to_f32(self) -> f32 {
        match self {
            Self::Bool(b) => b as i32 as f32,
            Self::U16(v) => v as f32,
            Self::I32(v) => v as f32,
            Self::Real(f) => f,
            Self::F64(f) => f as f32,
        }
    }

    /// Numeric f64 view — the widening lane for LREAL variables: every
    /// other variant converts by value without loss.
    pub fn to_f64(self) -> f64 {
        match self {
            Self::Bool(b) => b as i32 as f64,
            Self::U16(v) => v as f64,
            Self::I32(v) => v as f64,
            Self::Real(f) => f as f64,
            Self::F64(f) => f,
        }
    }

    /// Encode for `write_variable` on the ironplc VM, which takes an i32
    /// whose meaning depends on the *target variable's* IEC type: REAL
    /// variables reinterpret the i32 as IEEE-754 bits, integer variables
    /// take it by value. Mismatched pairs convert numerically first, so
    /// an integer channel bound to a REAL var (or vice versa) does the
    /// right thing instead of smuggling a bit pattern. LREAL targets use
    /// `to_f64().to_bits()` with `write_variable_raw` instead — see the
    /// bridge's input phase.
    pub fn to_vm_bits(self, var_is_real: bool) -> i32 {
        if var_is_real {
            self.to_f32().to_bits() as i32
        } else {
            self.to_i32()
        }
    }
}

#[derive(Debug, Error)]
pub enum IoError {
    #[error("unknown channel '{0}'")]
    UnknownChannel(String),
    #[error("type mismatch: channel '{channel}' cannot accept {value:?}")]
    TypeMismatch {
        channel: String,
        value: ChannelValue,
    },
    #[error("connect: {0}")]
    Connect(String),
    #[error("transport: {0}")]
    Transport(String),
    /// The peer replied but rejected the operation. This is not evidence
    /// that the connection is lost, or that other channels were skipped.
    #[error("protocol: {0}")]
    Protocol(String),
}

/// A fieldbus device — read/write a logical channel by name. Implementations
/// own their connection / runtime / cache.
#[async_trait]
pub trait IoDevice: Send {
    fn name(&self) -> &str;
    async fn read_channel(&mut self, channel: &str) -> Result<ChannelValue, IoError>;
    async fn write_channel(&mut self, channel: &str, value: ChannelValue) -> Result<(), IoError>;

    /// Whether the device's transport is currently believed good.
    ///
    /// Adapters with a background transfer loop (Modbus poll task,
    /// EtherCAT cyclic thread) flip this to `false` after a run of
    /// consecutive transfer failures and back to `true` on recovery —
    /// see [`HealthTracker`]. While unhealthy, reads typically keep
    /// serving last-known values; this flag is how the scan loop /
    /// monitor layer can tell "live data" from "stale mirror".
    ///
    /// Default `true` for devices without a background link to track
    /// (sim adapters, synchronous one-shot transports).
    fn is_healthy(&self) -> bool {
        true
    }

    /// Drive all writable outputs to a known-safe state (zero / "off").
    ///
    /// Called by the bridge scan loop in three situations:
    ///   1. **Panic** during a scan round — the run-loop catches the
    ///      unwind and triggers failsafe before the thread exits.
    ///   2. **Consecutive scan-deadline overruns** above a threshold —
    ///      "the simulation is no longer real-time, freeze the plant".
    ///   3. **Graceful shutdown** — explicit stop request.
    ///
    /// Industrial PLCs do this via a hardware watchdog; we don't have
    /// hardware here, so the bridge orchestrates the equivalent in
    /// software. Implementations should:
    ///   - Write a zero/safe value to every output channel they know
    ///     about. Read-only channels are skipped.
    ///   - Best-effort: continue past per-channel protocol rejections.
    ///     An unusable transport may abort the remaining writes to keep
    ///     shutdown bounded. Return an error if any output was not
    ///     confirmed, preserving protocol vs transport classification.
    ///     Success is an adapter acknowledgement, not physical readback.
    ///
    /// Default impl is a no-op so devices that genuinely have no
    /// writable surface (e.g. a read-only sensor adapter) need no
    /// extra code.
    async fn enter_failsafe(&mut self) -> Result<(), IoError> {
        Ok(())
    }

    /// Wind the device down for a clean process exit. Called once by the
    /// bridge on graceful shutdown, AFTER `enter_failsafe`, so an
    /// implementation can flush its now-safe outputs and join any
    /// background I/O thread it owns before the process goes away.
    ///
    /// This is what lets the in-runtime failsafe actually reach the wire:
    /// e.g. the EtherCAT adapter runs its cyclic exchange on a dedicated
    /// thread, so it signals + joins that thread here to guarantee the
    /// zeroed outputs (controlword = 0) are transmitted before teardown,
    /// rather than relying on the drive's own watchdog after the master
    /// is killed.
    ///
    /// Implementations MUST be bounded — the runtime only has a few
    /// seconds before the service supervisor force-kills it. Default impl
    /// is a no-op for devices with no background work to wind down (e.g.
    /// sim, or Modbus whose `enter_failsafe` already wrote synchronously).
    async fn shutdown(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lane contract, pinned: `U16` is unsigned and widens without a
    /// sign; a signed field of any width belongs on `I32`. Adapters were
    /// choosing differently (EtherCAT and OPC UA parked two's-complement
    /// patterns in `U16`), so the rule is asserted here rather than left
    /// as a comment in whichever decoder happened to get it right.
    #[test]
    fn u16_is_the_unsigned_lane_and_i32_carries_the_sign() {
        // 0xFFFE is -2 as a 16-bit two's-complement pattern.
        let smuggled = ChannelValue::U16(0xFFFE);
        assert_eq!(smuggled.to_i32(), 65534, "U16 widens unsigned, by design");
        assert_eq!(smuggled.to_f64(), 65534.0);

        let correct = ChannelValue::I32(-2);
        assert_eq!(correct.to_i32(), -2);
        assert_eq!(correct.to_f32(), -2.0);
        assert_eq!(correct.to_f64(), -2.0);
        // And it reaches a REAL variable as the float -2.0, not 65534.0.
        assert_eq!(correct.to_vm_bits(true), (-2.0f32).to_bits() as i32);
    }

    #[test]
    fn vm_bits_for_real_vars_are_ieee754() {
        // Real → REAL var: bit pattern.
        assert_eq!(
            ChannelValue::Real(12.7).to_vm_bits(true),
            12.7f32.to_bits() as i32
        );
        // Integer channel → REAL var: convert by value first.
        assert_eq!(
            ChannelValue::U16(42).to_vm_bits(true),
            42.0f32.to_bits() as i32
        );
        // Real → integer var: numeric truncation, not bits.
        assert_eq!(ChannelValue::Real(12.7).to_vm_bits(false), 12);
        // Integer → integer: identity.
        assert_eq!(ChannelValue::I32(-7).to_vm_bits(false), -7);
    }
}
