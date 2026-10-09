//! `cs sim run` — scenario-driven simulation harness.
//!
//! Closes the loop agent-generated logic was missing: after writing a
//! program, an agent (or CI) can PROVE it behaves before any hardware
//! is touched. The program runs on the server against the simulated
//! device layer (demo Modbus slave, `_sim` EtherCAT/CANopen NICs, fake
//! OPC UA); the scenario plays the plant — setting inputs, waiting,
//! and asserting on outputs and alarms — through the same public API
//! a human debugger uses.
//!
//! Scenario file (TOML), executed top to bottom:
//!
//! ```toml
//! description = "tank fills, high alarm raises and acks"
//!
//! [[steps]]                      # let the scan loop settle
//! wait_ms = 300
//!
//! [[steps]]                      # play the plant: set an input
//! set = { var = "inlet_cmd", value = true }
//!
//! [[steps]]                      # assert with a deadline
//! expect = { var = "level", op = "gt", value = 10.0, within_ms = 5000 }
//!
//! [[steps]]                      # safety property over a window
//! expect_never = { var = "overflow", op = "is_true", during_ms = 2000 }
//!
//! [[steps]]                      # alarms are first-class assertables
//! expect_alarm = { id = "level_high", active = true, within_ms = 3000 }
//!
//! [[steps]]                      # fault injection: stall N scans so the
//! inject = { scan_stall_ms = 25 } # scan watchdog trips through its real path
//!
//! [[steps]]                      # assert on the watchdog latch itself:
//! expect_watchdog = { tripped = true, within_ms = 3000 }   # reached by deadline
//! # …or `{ tripped = false, during_ms = 800 }` — HOLDS for the whole window
//! # (the right shape for a negative control before inject)
//!
//! [[steps]]                      # terminal failure, including an infinite loop
//! expect_fault = { contains = "VM execution budget exceeded", within_ms = 5000 }
//! ```
//!
//! Exit codes follow the CLI contract: 0 = every expectation held,
//! 1 = an expectation failed (the report names step, deadline and the
//! last observed value), 2/3 = usage / infrastructure.
//!
//! `--trace out.jsonl` records every polled sample for post-mortem;
//! `--keep-running` leaves the program running after the scenario.

use std::io::Write as _;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::http::{url_encode, Client, UsageError};

const POLL_MS: u64 = 100;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    steps: Vec<Step>,
}

/// Exactly one of the fields must be present per step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    wait_ms: Option<u64>,
    set: Option<SetStep>,
    expect: Option<ExpectStep>,
    expect_never: Option<NeverStep>,
    expect_alarm: Option<AlarmStep>,
    inject: Option<InjectStep>,
    expect_watchdog: Option<WatchdogStep>,
    expect_fault: Option<FaultStep>,
}

/// An expected terminal runtime fault, read from status rather than a stale
/// variable snapshot. Both the stopped state and its reason must match.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FaultStep {
    contains: String,
    #[serde(default = "default_within")]
    within_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetStep {
    var: String,
    /// Bool / number / string — encoded by the variable's IEC type,
    /// same rules as `cs runtime write`.
    value: toml::Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectStep {
    var: String,
    op: CmpOp,
    #[serde(default)]
    value: f64,
    #[serde(default = "default_within")]
    within_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NeverStep {
    var: String,
    op: CmpOp,
    #[serde(default)]
    value: f64,
    #[serde(default = "default_within")]
    during_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AlarmStep {
    id: String,
    #[serde(default = "default_true")]
    active: bool,
    #[serde(default = "default_within")]
    within_ms: u64,
}

/// Fault injection: stall each of the next `scans` **ticks** by
/// `scan_stall_ms` of wall-clock time, so the scan watchdog trips
/// through its real code path. The only scenario vocabulary that can
/// drive a timing fault deterministically — a CPU-burn program proves
/// nothing on a fast host.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InjectStep {
    scan_stall_ms: u64,
    /// Defaults server-side to one more than the watchdog threshold.
    #[serde(default)]
    scans: Option<u32>,
}

/// Assert on the scan-watchdog latch (`watchdog_tripped` on
/// `/api/runtime/status`). Two windows, same split as expect vs
/// expect_never: `within_ms` = the state is REACHED by the deadline
/// (eventually); `during_ms` = the state HOLDS for the whole window
/// (fails on the first counterexample). A negative control before an
/// inject step must use `during_ms` — with eventually-semantics,
/// `tripped = false` passes trivially on the first poll and guards
/// nothing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchdogStep {
    #[serde(default = "default_true")]
    tripped: bool,
    within_ms: Option<u64>,
    during_ms: Option<u64>,
}

fn default_within() -> u64 {
    5000
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum CmpOp {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
    Ne,
    IsTrue,
    IsFalse,
}

impl CmpOp {
    fn eval(self, actual: f64, expected: f64) -> bool {
        match self {
            CmpOp::Gt => actual > expected,
            CmpOp::Ge => actual >= expected,
            CmpOp::Lt => actual < expected,
            CmpOp::Le => actual <= expected,
            CmpOp::Eq => actual == expected,
            CmpOp::Ne => actual != expected,
            CmpOp::IsTrue => actual != 0.0,
            CmpOp::IsFalse => actual == 0.0,
        }
    }

    fn describe(self, expected: f64) -> String {
        match self {
            CmpOp::IsTrue => "is TRUE".into(),
            CmpOp::IsFalse => "is FALSE".into(),
            CmpOp::Gt => format!("> {expected}"),
            CmpOp::Ge => format!(">= {expected}"),
            CmpOp::Lt => format!("< {expected}"),
            CmpOp::Le => format!("<= {expected}"),
            CmpOp::Eq => format!("== {expected}"),
            CmpOp::Ne => format!("!= {expected}"),
        }
    }
}

pub(crate) struct SimArgs<'a> {
    pub scenario: &'a str,
    pub program: Option<&'a str>,
    pub trace: Option<&'a str>,
    pub keep_running: bool,
    pub no_run: bool,
}

pub(crate) fn cmd_sim_run(client: &Client, args: &SimArgs<'_>, json: bool) -> Result<i32> {
    let text = std::fs::read_to_string(args.scenario)
        .with_context(|| format!("reading {}", args.scenario))
        .map_err(UsageError::wrap)?;
    let scenario: Scenario = toml::from_str(&text)
        .with_context(|| format!("parsing {}", args.scenario))
        .map_err(UsageError::wrap)?;
    validate(&scenario).map_err(UsageError::wrap)?;

    let mut trace = match args.trace {
        Some(path) => Some(
            std::fs::File::create(path)
                .with_context(|| format!("creating trace file {path}"))
                .map_err(UsageError::wrap)?,
        ),
        None => None,
    };

    if let Some(d) = &scenario.description {
        eprintln!("sim: {d}");
    }

    // Start the program (unless the caller is attaching to a run they
    // already started).
    if !args.no_run {
        let body = match args.program {
            Some(p) => serde_json::json!({ "kind": "isolated", "program": p }),
            None => serde_json::json!({ "kind": "project" }),
        };
        client.post("/api/run", &body)?;
    }

    let started = std::time::Instant::now();
    let mut failures: Vec<String> = Vec::new();

    for (i, step) in scenario.steps.iter().enumerate() {
        let n = i + 1;
        let outcome = run_step(client, step, n, &mut trace);
        match outcome {
            Ok(desc) => eprintln!("  ✓ step {n}: {desc}"),
            Err(msg) => {
                eprintln!("  ✗ step {n}: {msg}");
                failures.push(format!("step {n}: {msg}"));
                break; // later steps depend on earlier state — stop here
            }
        }
    }

    if !args.keep_running && !args.no_run {
        let _ = client.post("/api/stop", &serde_json::json!({}));
    }

    let passed = failures.is_empty();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": passed,
                "scenario": args.scenario,
                "steps": scenario.steps.len(),
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "failures": failures,
            }))?
        );
    } else if passed {
        eprintln!(
            "✓ scenario passed — {} step(s) in {:.1}s",
            scenario.steps.len(),
            started.elapsed().as_secs_f64()
        );
    } else {
        eprintln!("✗ scenario FAILED — {}", failures.join("; "));
    }
    Ok(if passed { 0 } else { 1 })
}

/// One step. Ok(human description) / Err(failure message).
fn run_step(
    client: &Client,
    step: &Step,
    n: usize,
    trace: &mut Option<std::fs::File>,
) -> std::result::Result<String, String> {
    if let Some(ms) = step.wait_ms {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        return Ok(format!("wait {ms}ms"));
    }
    if let Some(set) = &step.set {
        let raw = toml_value_string(&set.value);
        let encoded = super::runtime::parse_value(client, &set.var, &raw)
            .map_err(|e| format!("set {}: {e:#}", set.var))?;
        client
            .post(
                &format!("/api/runtime/variables/{}", url_encode(&set.var)),
                &serde_json::json!({ "value": encoded }),
            )
            .map_err(|e| format!("set {}: {e:#}", set.var))?;
        return Ok(format!("set {} := {raw}", set.var));
    }
    if let Some(exp) = &step.expect {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(exp.within_ms);
        let mut last: Option<f64> = None;
        loop {
            let val = read_var(client, &exp.var, trace, n)
                .map_err(|e| format!("expect {}: {e:#}", exp.var))?;
            if let Some(v) = val {
                last = Some(v);
                if exp.op.eval(v, exp.value) {
                    return Ok(format!(
                        "{} {} (observed {v})",
                        exp.var,
                        exp.op.describe(exp.value)
                    ));
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "expected {} {} within {}ms — last observed {}",
                    exp.var,
                    exp.op.describe(exp.value),
                    exp.within_ms,
                    last.map(|v| v.to_string())
                        .unwrap_or_else(|| "nothing (variable absent from snapshot)".into()),
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        }
    }
    if let Some(never) = &step.expect_never {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(never.during_ms);
        loop {
            let val = read_var(client, &never.var, trace, n)
                .map_err(|e| format!("expect_never {}: {e:#}", never.var))?;
            if let Some(v) = val {
                if never.op.eval(v, never.value) {
                    return Err(format!(
                        "{} {} observed (value {v}) inside the {}ms window",
                        never.var,
                        never.op.describe(never.value),
                        never.during_ms,
                    ));
                }
            }
            if std::time::Instant::now() >= deadline {
                return Ok(format!(
                    "{} never {} for {}ms",
                    never.var,
                    never.op.describe(never.value),
                    never.during_ms
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        }
    }
    if let Some(alarm) = &step.expect_alarm {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(alarm.within_ms);
        let mut seen = false;
        loop {
            let states = client
                .get("/api/runtime/alarms")
                .map_err(|e| format!("expect_alarm {}: {e:#}", alarm.id))?;
            let found = states
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .find(|s| s.get("id").and_then(|i| i.as_str()) == Some(alarm.id.as_str()))
                })
                .map(|s| s.get("active").and_then(|a| a.as_bool()).unwrap_or(false));
            if let Some(active) = found {
                seen = true;
                if active == alarm.active {
                    return Ok(format!(
                        "alarm {} {}",
                        alarm.id,
                        if alarm.active { "active" } else { "inactive" }
                    ));
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "alarm {} did not become {} within {}ms{}",
                    alarm.id,
                    if alarm.active { "active" } else { "inactive" },
                    alarm.within_ms,
                    if seen {
                        ""
                    } else {
                        " (id not defined in alarms.toml?)"
                    },
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        }
    }
    if let Some(inj) = &step.inject {
        let mut body = serde_json::json!({ "stall_ms": inj.scan_stall_ms });
        if let Some(n) = inj.scans {
            body["scans"] = serde_json::json!(n);
        }
        client
            .post("/api/runtime/inject-scan-stall", &body)
            .map_err(|e| format!("inject: {e:#}"))?;
        return Ok(format!(
            "inject scan stall {}ms x {}",
            inj.scan_stall_ms,
            inj.scans
                .map(|n| n.to_string())
                .unwrap_or_else(|| "default".into())
        ));
    }
    if let Some(wd) = &step.expect_watchdog {
        let read_tripped = || -> std::result::Result<bool, String> {
            let status = client
                .get("/api/runtime/status")
                .map_err(|e| format!("expect_watchdog: {e:#}"))?;
            // A missing field must FAIL, not read as `false`. Against a
            // runtime that predates the latch, `unwrap_or(false)` turned
            // `tripped = false` into a hollow pass: the negative control
            // that guards every scenario below it asserted nothing.
            let tripped = status
                .get("watchdog_tripped")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| {
                    "expect_watchdog: /api/runtime/status carries no boolean \
                     `watchdog_tripped` — this runtime predates the watchdog \
                     latch, so the step cannot be evaluated (upgrade the \
                     runtime, or drop the step)"
                        .to_string()
                })?;
            // Nothing running ⇒ the latch means nothing. Without this a
            // scenario whose program never started still "passed" its
            // `tripped = false` control.
            let running = status
                .get("running")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !running {
                return Err("expect_watchdog: no program is running — the \
                            watchdog state is not meaningful (did the program \
                            fail to start?)"
                    .to_string());
            }
            Ok(tripped)
        };
        let word = |b: bool| {
            if b {
                "tripped (latched)"
            } else {
                "not tripped"
            }
        };
        if let Some(during_ms) = wd.during_ms {
            // Hold semantics: the state must match at EVERY poll in the
            // window. First counterexample fails the step.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(during_ms);
            loop {
                let now = read_tripped()?;
                if now != wd.tripped {
                    return Err(format!(
                        "watchdog became {} inside the {during_ms}ms window (required: stays {})",
                        word(now),
                        word(wd.tripped),
                    ));
                }
                if std::time::Instant::now() >= deadline {
                    return Ok(format!("watchdog {} for {during_ms}ms", word(wd.tripped)));
                }
                std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
            }
        }
        // Eventually semantics (default): reached by the deadline.
        let within_ms = wd.within_ms.unwrap_or_else(default_within);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(within_ms);
        loop {
            let last = read_tripped()?;
            if last == wd.tripped {
                return Ok(format!("watchdog {}", word(wd.tripped)));
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "watchdog_tripped stayed {last} for {within_ms}ms (expected {})",
                    wd.tripped
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        }
    }
    if let Some(expected) = &step.expect_fault {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(expected.within_ms);
        loop {
            let status = client
                .get("/api/runtime/status")
                .map_err(|e| format!("expect_fault: {e:#}"))?;
            let running = status
                .get("running")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| "expect_fault: status carries no boolean running".to_string())?;
            let error = match status.get("last_error") {
                Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(s)) => Some(s.as_str()),
                _ => return Err("expect_fault: status carries no string/null last_error".into()),
            };
            if let Some(file) = trace {
                let line = serde_json::json!({"step": n, "status": status});
                writeln!(file, "{line}").map_err(|e| format!("trace: {e}"))?;
            }
            if !running && error.is_some_and(|message| message.contains(&expected.contains)) {
                return Ok(format!(
                    "runtime stopped with fault: {}",
                    error.unwrap_or_default()
                ));
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("expected stopped runtime with fault containing {:?}; running={running}, last_error={error:?}", expected.contains));
            }
            std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
        }
    }
    Err("empty step (write one of: wait_ms / set / expect / expect_never / expect_alarm / inject / expect_watchdog / expect_fault)".into())
}

/// Read one variable's numeric value off /api/runtime/snapshot, and
/// append the full sample to the trace when tracing. `Ok(None)` =
/// snapshot exists but doesn't carry the variable (yet).
fn read_var(
    client: &Client,
    name: &str,
    trace: &mut Option<std::fs::File>,
    step: usize,
) -> Result<Option<f64>> {
    let snap = client.get("/api/runtime/snapshot")?;
    if let Some(f) = trace.as_mut() {
        let _ = writeln!(
            f,
            "{}",
            serde_json::json!({ "step": step, "snapshot": &snap })
        );
    }
    let Some(vars) = snap.get("vars").and_then(|v| v.as_array()) else {
        return Ok(None);
    };
    for v in vars {
        if v.get("name").and_then(|n| n.as_str()) == Some(name) {
            return Ok(numeric_value(v));
        }
    }
    Ok(None)
}

/// Snapshot var → f64, from the display string (BOOL TRUE/FALSE,
/// `16#` hex, plain decimal) — the same decode surface `cs` uses for
/// force typing.
fn numeric_value(var: &serde_json::Value) -> Option<f64> {
    let display = var.get("value").and_then(|v| v.as_str())?;
    let type_name = var.get("type_name").and_then(|v| v.as_str()).unwrap_or("");
    if type_name.eq_ignore_ascii_case("BOOL") {
        return Some(if display.eq_ignore_ascii_case("TRUE") {
            1.0
        } else {
            0.0
        });
    }
    if let Some(hex) = display.strip_prefix("16#") {
        if let Ok(n) = u64::from_str_radix(hex, 16) {
            return Some(n as f64);
        }
    }
    display.parse::<f64>().ok()
}

fn toml_value_string(v: &toml::Value) -> String {
    match v {
        toml::Value::Boolean(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn validate(s: &Scenario) -> Result<()> {
    if s.steps.is_empty() {
        bail!("scenario has no [[steps]]");
    }
    for (i, step) in s.steps.iter().enumerate() {
        let set_fields = [
            step.wait_ms.is_some(),
            step.set.is_some(),
            step.expect.is_some(),
            step.expect_never.is_some(),
            step.expect_alarm.is_some(),
            step.inject.is_some(),
            step.expect_watchdog.is_some(),
            step.expect_fault.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count();
        if set_fields != 1 {
            bail!(
                "step {}: exactly one of wait_ms / set / expect / expect_never / expect_alarm / inject / expect_watchdog / expect_fault (found {set_fields})",
                i + 1
            );
        }
        if let Some(e) = &step.expect {
            if matches!(e.op, CmpOp::IsTrue | CmpOp::IsFalse) && e.value != 0.0 {
                bail!("step {}: `{:?}` takes no value", i + 1, e.op);
            }
        }
        if let Some(w) = &step.expect_watchdog {
            if w.within_ms.is_some() && w.during_ms.is_some() {
                bail!(
                    "step {}: expect_watchdog takes within_ms (reached by deadline) OR during_ms (holds for the window), not both",
                    i + 1
                );
            }
        }
        if let Some(fault) = &step.expect_fault {
            if fault.contains.trim().is_empty() {
                bail!("step {}: expect_fault.contains must not be empty", i + 1);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expected_fault_requires_a_nonempty_reason_and_its_own_step() {
        for source in [
            "[[steps]]\nexpect_fault = { contains = ' ' }",
            "[[steps]]\nwait_ms = 1\nexpect_fault = { contains = 'budget' }",
        ] {
            let scenario: Scenario = toml::from_str(source).unwrap();
            assert!(validate(&scenario).is_err());
        }
        let scenario: Scenario =
            toml::from_str("[[steps]]\nexpect_fault = { contains = 'budget' }").unwrap();
        validate(&scenario).unwrap();
    }

    #[test]
    fn scenario_parses_and_validates() {
        let s: Scenario = toml::from_str(
            r#"
description = "demo"
[[steps]]
wait_ms = 100
[[steps]]
set = { var = "inlet", value = true }
[[steps]]
expect = { var = "level", op = "gt", value = 10.0, within_ms = 500 }
[[steps]]
expect_never = { var = "overflow", op = "is_true", during_ms = 200 }
[[steps]]
expect_alarm = { id = "level_high", within_ms = 300 }
"#,
        )
        .unwrap();
        validate(&s).unwrap();
        assert_eq!(s.steps.len(), 5);
        assert!(s.steps[4].expect_alarm.as_ref().unwrap().active);
    }

    #[test]
    fn ambiguous_step_rejected() {
        let s: Scenario = toml::from_str(
            r#"
[[steps]]
wait_ms = 100
set = { var = "x", value = 1 }
"#,
        )
        .unwrap();
        assert!(validate(&s).is_err());
    }

    #[test]
    fn unknown_field_rejected_at_parse() {
        let r = toml::from_str::<Scenario>(
            r#"
[[steps]]
expcet = { var = "x", op = "gt", value = 1.0 }
"#,
        );
        assert!(r.is_err(), "typo'd field must not be silently ignored");
    }

    #[test]
    fn cmp_ops_and_value_decode() {
        assert!(CmpOp::Gt.eval(2.0, 1.0));
        assert!(CmpOp::IsTrue.eval(1.0, 0.0));
        assert!(!CmpOp::IsTrue.eval(0.0, 0.0));
        let v = serde_json::json!({ "name": "x", "type_name": "WORD", "value": "16#FF" });
        assert_eq!(numeric_value(&v), Some(255.0));
        let b = serde_json::json!({ "name": "b", "type_name": "BOOL", "value": "TRUE" });
        assert_eq!(numeric_value(&b), Some(1.0));
    }
}
