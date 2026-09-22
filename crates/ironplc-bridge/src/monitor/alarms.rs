//! Alarm engine — evaluates `alarms.toml` definitions against sampled
//! snapshots and keeps the ISA-18.2-shaped state machine + journal.
//!
//! States per alarm (the four that matter on a small panel):
//!   Normal ──cond holds `delay_ms`──▶ Active/unacked
//!   Active/unacked ──ack──▶ Active/acked
//!   Active/acked ──cond clears (deadband)──▶ Normal        (+ `returned`)
//!   Active/unacked ──cond clears──▶ Cleared/unacked        (+ `returned`)
//!   Cleared/unacked ──ack──▶ Normal
//!
//! "Cleared but unacknowledged" is deliberately kept: an alarm that
//! fired at 03:00 and self-cleared must still be visible until a human
//! acknowledges it happened.

use std::collections::{HashMap, VecDeque};

use serde::Serialize;
use ts_rs::TS;

use super::typed_value;
use crate::runtime::VarSnapshot;
use project::{AlarmCondition, AlarmDef, AlarmSeverity};

/// Bounded journal length — oldest entries drop first. Enough for a
/// shift's worth of events on a busy line without unbounded memory.
const JOURNAL_CAP: usize = 1000;
/// Suppress short reconnect blips, but report sustained loss without setup.
const DEVICE_LOSS_DELAY_MS: u32 = 1000;

/// Live state of one alarm definition, as served by GET /alarms.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct AlarmState {
    pub id: String,
    pub severity: AlarmSeverity,
    pub message: String,
    pub variable: String,
    /// Condition currently true (post-deadband).
    pub active: bool,
    /// Acknowledged by an operator since it last raised.
    pub acked: bool,
    /// Micros timestamp of the most recent raise; 0 = never raised.
    pub raised_at_us: u64,
    /// Variable value at the most recent raise.
    pub value_at_raise: f64,
    /// How many times this alarm has raised since the engine started.
    pub count: u32,
}

impl AlarmState {
    /// Needs operator attention: active, or cleared-but-unacked.
    pub fn standing(&self) -> bool {
        self.active || !self.acked
    }
}

/// One journal line. `event` is `raised` / `acked` / `returned`.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct AlarmJournalEntry {
    pub t_us: u64,
    pub id: String,
    pub event: String,
    pub severity: AlarmSeverity,
    pub message: String,
    /// Variable value when the event happened (0 for `acked`).
    pub value: f64,
}

#[derive(Debug, Default)]
struct AlarmRuntime {
    /// Condition raw-true since this time (pre-delay); None = not held.
    pending_since_us: Option<u64>,
    active: bool,
    acked: bool, // meaningful while active or cleared-unacked
    raised_at_us: u64,
    value_at_raise: f64,
    count: u32,
    /// Cleared while unacked — stays visible until acked.
    cleared_unacked: bool,
}

/// The engine. One per running program; rebuild (`AlarmEngine::new`)
/// when the project's alarm config changes.
#[derive(Debug, Default)]
pub struct AlarmEngine {
    defs: Vec<AlarmDef>,
    device_defs: HashMap<String, AlarmDef>,
    states: HashMap<String, AlarmRuntime>,
    journal: VecDeque<AlarmJournalEntry>,
}

impl AlarmEngine {
    pub fn new(defs: Vec<AlarmDef>) -> Self {
        let states = defs
            .iter()
            .map(|d| (d.id.clone(), AlarmRuntime::default()))
            .collect();
        Self {
            defs,
            device_defs: HashMap::new(),
            states,
            journal: VecDeque::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty() && self.device_defs.is_empty()
    }

    /// Evaluate every definition against a sampled snapshot. Returns
    /// true when any alarm changed state (callers use it to invalidate
    /// UI caches / publish events).
    ///
    /// Two time bases, deliberately: the STATE MACHINE (delay debounce)
    /// runs on `snap.timestamp_us` — monotonic, scan-derived — while
    /// the HUMAN-facing stamps (`raised_at_us`, journal `t_us`) take
    /// `wall_now_us`, so "when did it fire" survives restarts and
    /// aligns with the operator's clock.
    pub fn note_snapshot(&mut self, snap: &VarSnapshot, wall_now_us: u64) -> bool {
        let now_us = snap.timestamp_us;
        let mut changed = false;
        let device_health = snap.device_health.as_deref().unwrap_or_default();
        for health in device_health {
            self.device_defs.entry(health.name.clone()).or_insert_with(|| AlarmDef {
                id: format!("{}{}", project::DEVICE_ALARM_PREFIX, health.name),
                variable: format!("device:{}", health.name),
                condition: AlarmCondition::IsFalse,
                limit: None, deadband: 0.0, delay_ms: DEVICE_LOSS_DELAY_MS,
                severity: AlarmSeverity::High,
                message: format!("Device '{}' is unavailable — check its power and fieldbus connection; input values are stale", health.name),
            });
        }
        let values = self
            .defs
            .iter()
            .map(|def| {
                let value = snap
                    .vars
                    .iter()
                    .find(|v| v.name == def.variable)
                    .filter(|v| !v.input.as_ref().is_some_and(|q| q.stale))
                    .and_then(|v| {
                        let value = typed_value(&v.type_name, v.bits, &v.value);
                        value
                            .as_f64()
                            .or_else(|| value.as_bool().map(|b| u8::from(b) as f64))
                    });
                (def, value)
            })
            .chain(self.device_defs.iter().map(|(name, def)| {
                (
                    def,
                    device_health
                        .iter()
                        .find(|h| h.name == *name)
                        .map(|h| u8::from(h.healthy) as f64),
                )
            }));
        for (def, value) in values {
            let st = self.states.entry(def.id.clone()).or_default();
            let Some(value) = value else {
                // Unknown input cannot raise OR clear a process alarm. A
                // missing interval also cannot count toward its debounce.
                st.pending_since_us = None;
                continue;
            };

            let raw = eval_condition(def, value, st.active);
            if raw {
                let held_since = *st.pending_since_us.get_or_insert(now_us);
                let held_long_enough =
                    now_us.saturating_sub(held_since) >= def.delay_ms as u64 * 1000;
                if !st.active && held_long_enough {
                    st.active = true;
                    st.acked = false;
                    st.cleared_unacked = false;
                    st.raised_at_us = wall_now_us;
                    st.value_at_raise = value;
                    st.count += 1;
                    changed = true;
                    push_journal(
                        &mut self.journal,
                        journal_entry(def, wall_now_us, "raised", value),
                    );
                }
            } else {
                st.pending_since_us = None;
                if st.active {
                    st.active = false;
                    if !st.acked {
                        st.cleared_unacked = true;
                    }
                    changed = true;
                    push_journal(
                        &mut self.journal,
                        journal_entry(def, wall_now_us, "returned", value),
                    );
                }
            }
        }
        changed
    }

    /// Operator acknowledgement. Unknown id → Err. Idempotent on an
    /// already-acked alarm.
    pub fn ack(&mut self, id: &str, now_us: u64) -> Result<AlarmState, String> {
        let def = self
            .defs
            .iter()
            .chain(self.device_defs.values())
            .find(|d| d.id == id)
            .ok_or_else(|| format!("no alarm '{id}'"))?
            .clone();
        let st = self.states.entry(def.id.clone()).or_default();
        if !st.acked || st.cleared_unacked {
            st.acked = true;
            st.cleared_unacked = false;
            push_journal(&mut self.journal, journal_entry(&def, now_us, "acked", 0.0));
        }
        Ok(state_of(&def, st))
    }

    /// Current state of every defined alarm, severity-major order
    /// (critical first), standing alarms before quiet ones.
    pub fn states(&self) -> Vec<AlarmState> {
        let mut out: Vec<AlarmState> = self
            .defs
            .iter()
            .chain(self.device_defs.values())
            .map(|d| state_of(d, self.states.get(&d.id).unwrap_or(&DEFAULT_RT)))
            .collect();
        out.sort_by(|a, b| {
            b.standing()
                .cmp(&a.standing())
                .then(b.severity.cmp(&a.severity))
                .then(b.raised_at_us.cmp(&a.raised_at_us))
        });
        out
    }

    /// Most-recent-first journal slice.
    pub fn journal(&self, limit: usize) -> Vec<AlarmJournalEntry> {
        self.journal.iter().rev().take(limit).cloned().collect()
    }
}

static DEFAULT_RT: AlarmRuntime = AlarmRuntime {
    pending_since_us: None,
    active: false,
    acked: false,
    raised_at_us: 0,
    value_at_raise: 0.0,
    count: 0,
    cleared_unacked: false,
};

fn state_of(def: &AlarmDef, rt: &AlarmRuntime) -> AlarmState {
    AlarmState {
        id: def.id.clone(),
        severity: def.severity,
        message: def.message.clone(),
        variable: def.variable.clone(),
        active: rt.active,
        // A never-raised alarm reads as acked (nothing outstanding).
        acked: if rt.active || rt.cleared_unacked {
            rt.acked && !rt.cleared_unacked
        } else {
            true
        },
        raised_at_us: rt.raised_at_us,
        value_at_raise: rt.value_at_raise,
        count: rt.count,
    }
}

fn journal_entry(def: &AlarmDef, t_us: u64, event: &str, value: f64) -> AlarmJournalEntry {
    AlarmJournalEntry {
        t_us,
        id: def.id.clone(),
        event: event.to_string(),
        severity: def.severity,
        message: def.message.clone(),
        value,
    }
}

fn push_journal(journal: &mut VecDeque<AlarmJournalEntry>, entry: AlarmJournalEntry) {
    if journal.len() >= JOURNAL_CAP {
        journal.pop_front();
    }
    journal.push_back(entry);
}

/// Raw condition test. `active` feeds the deadband: a numeric alarm
/// that is currently active only CLEARS once the value crosses back
/// past `limit ∓ deadband`.
fn eval_condition(def: &AlarmDef, value: f64, active: bool) -> bool {
    use AlarmCondition::*;
    let limit = def.limit.unwrap_or(0.0);
    let db = def.deadband.abs();
    match def.condition {
        IsTrue => value != 0.0,
        IsFalse => value == 0.0,
        Gt => {
            if active {
                value > limit - db
            } else {
                value > limit
            }
        }
        Ge => {
            if active {
                value >= limit - db
            } else {
                value >= limit
            }
        }
        Lt => {
            if active {
                value < limit + db
            } else {
                value < limit
            }
        }
        Le => {
            if active {
                value <= limit + db
            } else {
                value <= limit
            }
        }
        Eq => value == limit,
        Ne => value != limit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{VarSnapshot, VarValue};

    fn snap(t_us: u64, level: f32) -> VarSnapshot {
        VarSnapshot {
            timestamp_us: t_us,
            scan_count: t_us,
            device_health: None,
            vars: vec![VarValue {
                name: "level".into(),
                type_name: "REAL".into(),
                value: format!("{level}"),
                bits: level.to_bits() as u64,
                input: None,
            }],
        }
    }

    fn high_alarm(deadband: f64, delay_ms: u32) -> AlarmEngine {
        AlarmEngine::new(vec![AlarmDef {
            id: "level_high".into(),
            variable: "level".into(),
            condition: AlarmCondition::Gt,
            limit: Some(90.0),
            deadband,
            delay_ms,
            severity: AlarmSeverity::High,
            message: "Tank level high".into(),
        }])
    }

    #[test]
    fn raises_acks_and_returns() {
        let mut eng = high_alarm(0.0, 0);
        assert!(!eng.note_snapshot(&snap(1_000_000, 50.0), 11));
        assert!(eng.note_snapshot(&snap(2_000_000, 95.0), 22), "raise");
        let st = &eng.states()[0];
        assert!(st.active && !st.acked && st.standing());
        assert_eq!(st.value_at_raise, 95.0);

        eng.ack("level_high", 3_000_000).unwrap();
        assert!(eng.states()[0].acked);

        assert!(eng.note_snapshot(&snap(4_000_000, 50.0), 44), "return");
        let st = &eng.states()[0];
        assert!(!st.active && !st.standing(), "acked + returned = quiet");

        let events: Vec<String> = eng.journal(10).iter().map(|j| j.event.clone()).collect();
        assert_eq!(events, ["returned", "acked", "raised"]);
    }

    #[test]
    fn cleared_unacked_stays_standing_until_ack() {
        let mut eng = high_alarm(0.0, 0);
        eng.note_snapshot(&snap(1_000_000, 95.0), 11);
        eng.note_snapshot(&snap(2_000_000, 50.0), 22); // self-clears, never acked
        let st = &eng.states()[0];
        assert!(!st.active && st.standing(), "03:00 alarm must stay visible");
        eng.ack("level_high", 3_000_000).unwrap();
        assert!(!eng.states()[0].standing());
    }

    #[test]
    fn deadband_stops_chatter() {
        let mut eng = high_alarm(2.0, 0);
        eng.note_snapshot(&snap(1_000_000, 91.0), 11); // raise (>90)
        assert!(eng.states()[0].active);
        // Dips to 89 — inside the deadband window (>88 keeps it active).
        assert!(
            !eng.note_snapshot(&snap(2_000_000, 89.0), 22),
            "no state change"
        );
        assert!(eng.states()[0].active, "still active inside deadband");
        // Below 88 — now it clears.
        eng.note_snapshot(&snap(3_000_000, 87.5), 33);
        assert!(!eng.states()[0].active);
    }

    #[test]
    fn delay_debounces_spikes() {
        let mut eng = high_alarm(0.0, 500);
        eng.note_snapshot(&snap(1_000_000, 95.0), 11); // spike starts
        assert!(!eng.states()[0].active, "not yet — needs 500ms");
        eng.note_snapshot(&snap(1_200_000, 50.0), 12); // spike ends at 200ms
        assert!(
            !eng.states()[0].active,
            "spike shorter than delay never raises"
        );
        eng.note_snapshot(&snap(2_000_000, 95.0), 22);
        eng.note_snapshot(&snap(2_600_000, 95.0), 26); // held 600ms
        assert!(eng.states()[0].active, "sustained condition raises");
    }

    #[test]
    fn ack_unknown_id_is_an_error() {
        let mut eng = high_alarm(0.0, 0);
        assert!(eng.ack("nope", 0).is_err());
    }

    fn bus_snap(t_us: u64, healthy: bool) -> VarSnapshot {
        let mut snap = snap(t_us, 0.0);
        snap.device_health = Some(vec![crate::DeviceHealth {
            name: "bus0".into(),
            healthy,
        }]);
        snap
    }

    #[test]
    fn device_loss_raises_without_config_and_recovery_still_needs_ack() {
        let mut eng = AlarmEngine::default();
        assert!(!eng.note_snapshot(&bus_snap(0, false), 1));
        assert!(!eng.note_snapshot(&bus_snap(999_999, false), 2));
        assert!(eng.note_snapshot(&bus_snap(1_000_000, false), 123_456));
        let state = &eng.states()[0];
        assert_eq!(state.id, "__device/bus0");
        assert_eq!(state.severity, AlarmSeverity::High);
        assert_eq!(state.raised_at_us, 123_456, "journal uses wall clock");
        assert!(state.active && !state.acked);
        assert!(eng.note_snapshot(&bus_snap(2_000_000, true), 234_567));
        assert!(!eng.states()[0].active && eng.states()[0].standing());
        eng.ack("__device/bus0", 345_678).unwrap();
        assert!(!eng.states()[0].standing());
        let events: Vec<_> = eng.journal(10).into_iter().map(|j| j.event).collect();
        assert_eq!(events, ["acked", "returned", "raised"]);
    }

    #[test]
    fn short_device_blip_does_not_accumulate_across_recovery() {
        let mut eng = AlarmEngine::default();
        for (t, healthy) in [
            (0, false),
            (900_000, true),
            (1_000_000, false),
            (1_900_000, false),
            (2_000_000, true),
        ] {
            assert!(!eng.note_snapshot(&bus_snap(t, healthy), t));
        }
        assert!(eng.journal(10).is_empty());
        assert!(!eng.states()[0].standing());
    }

    #[test]
    fn stale_process_value_neither_raises_nor_clears_and_breaks_debounce() {
        let mut eng = high_alarm(0.0, 500);
        let stale = |t, value| {
            let mut snap = snap(t, value);
            snap.vars[0].input = Some(crate::InputQuality {
                device: "bus0".into(),
                channel: "level".into(),
                stale: true,
            });
            snap
        };
        assert!(!eng.note_snapshot(&snap(0, 95.0), 0));
        assert!(!eng.note_snapshot(&stale(600_000, 95.0), 1));
        assert!(!eng.note_snapshot(&snap(700_000, 95.0), 2));
        assert!(!eng.note_snapshot(&snap(1_199_999, 95.0), 3));
        assert!(eng.note_snapshot(&snap(1_200_000, 95.0), 4));
        assert!(!eng.note_snapshot(&stale(2_000_000, 0.0), 5));
        assert!(eng.states()[0].active, "unknown cannot confirm a return");
        assert!(eng.note_snapshot(&snap(3_000_000, 0.0), 6));
        assert!(!eng.states()[0].active);
    }

    #[test]
    fn bool_alarm_via_is_true() {
        let mut eng = AlarmEngine::new(vec![AlarmDef {
            id: "estop".into(),
            variable: "estop_hit".into(),
            condition: AlarmCondition::IsTrue,
            limit: None,
            deadband: 0.0,
            delay_ms: 0,
            severity: AlarmSeverity::Critical,
            message: "E-stop engaged".into(),
        }]);
        let snap = VarSnapshot {
            timestamp_us: 1,
            scan_count: 1,
            device_health: None,
            vars: vec![VarValue {
                name: "estop_hit".into(),
                type_name: "BOOL".into(),
                value: "TRUE".into(),
                bits: 1,
                input: None,
            }],
        };
        assert!(eng.note_snapshot(&snap, 1));
        assert!(eng.states()[0].active);
    }
}
