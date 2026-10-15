//! `cs deploy` / `cs probe` — the two edge orchestration verbs. Edge
//! CRUD is `cs set/get/rm edges/<name>`; edge sub-reads are
//! `cs get edges/<name>/{probe,status,logs,scan,system}`; attach /
//! detach are `cs api POST /api/edges/<name>/attach|detach`.

use anyhow::Result;

use crate::http::{url_encode, Body, Client};

mod deploy_verdict;
use deploy_verdict::deploy_verdict;

pub(crate) fn cmd_deploy(client: &Client, name: &str, json: bool) -> Result<i32> {
    // The server's /api/edges/{name}/deploy route owns the SSH+tar
    // dance. Bigger timeout than the default (30 s) because the
    // tar+ssh round-trip can take minutes on a slow link.
    let value = client.request(
        "POST",
        &format!("/api/edges/{}/deploy", url_encode(name)),
        Body::Json(&serde_json::json!({})),
        Some(std::time::Duration::from_secs(600)),
    )?;

    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        // Human-readable: the streamed deploy log, then the verdict.
        let log = value.get("log").and_then(|v| v.as_str()).unwrap_or("");
        if !log.is_empty() {
            eprintln!("{log}");
        }
        for line in deploy_verdict(name, &value) {
            eprintln!("{line}");
        }
    }
    // ok=false: the remote script failed, or the new version is installed
    // but its program is not running.
    let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(if ok { 0 } else { 1 })
}

pub(crate) fn cmd_probe(client: &Client, name: &str, json: bool) -> Result<i32> {
    let value = client.get(&format!("/api/edges/{}/probe", url_encode(name)))?;
    let reachable = value
        .get("reachable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if reachable {
        for line in reachable_report(name, &value) {
            println!("{line}");
        }
    } else {
        let err = value
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unreachable");
        eprintln!("✗ {name}: {err}");
    }
    Ok(if reachable { 0 } else { 1 })
}

/// What `cs probe` prints for a reachable edge. "Reachable" only means the
/// runtime answered: a live scan loop on top of a dead fieldbus, a latched
/// watchdog, or a program that died must not print the same ✓ as a healthy
/// edge — that is the reading that sends people hunting the wrong fault.
/// The exit code stays 0 (it IS reachable); the text tells the truth.
fn reachable_report(name: &str, value: &serde_json::Value) -> Vec<String> {
    let scans = value
        .get("scan_count")
        .and_then(|v| v.as_u64())
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".into());
    let uptime = value
        .get("uptime_secs")
        .and_then(|v| v.as_u64())
        .map(|n| format!("{n}s"))
        .unwrap_or_else(|| "?".into());
    let version = value
        .get("runtime_version")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let down: Vec<&str> = value
        .get("unhealthy_devices")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|d| d.as_str()).collect())
        .unwrap_or_default();
    // A latched watchdog is the harsher version of the same trap: the
    // runtime answers, every bus is healthy, the scan count climbs —
    // and not one output is being driven.
    let latched = value
        .get("watchdog_tripped")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let latch_reason = value.get("latch_reason").and_then(|v| v.as_str());
    // Harsher still: the program itself is gone, and only the reason says so.
    let fault = value.get("fault").and_then(|v| v.as_str());
    // The task that feeds history and process alarms from the scan loop. The
    // program keeps running without it, so only this says that nothing is
    // being recorded or evaluated. `null` is an edge build that predates it.
    let tap = value.get("snapshot_tap").filter(|v| v.is_object());
    let tap_running = tap
        .and_then(|t| t.get("running"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let tap_faults = tap
        .and_then(|t| t.get("faults"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let tap_last = tap
        .and_then(|t| t.get("last_fault"))
        .and_then(|v| v.as_str());
    // Inside one snapshot for longer than a healthy stage takes: a stage is
    // blocked. `running` stays true and `faults` stays 0 then.
    let tap_stalled_ms = tap
        .and_then(|t| t.get("stalled_for_ms"))
        .and_then(|v| v.as_u64());
    let summary = format!("{name} reachable · v{version} · {scans} scans · up {uptime}");
    if down.is_empty()
        && !latched
        && fault.is_none()
        && tap_running
        && tap_faults == 0
        && tap_stalled_ms.is_none()
    {
        return vec![format!("✓ {summary}")];
    }
    let mut lines = vec![format!("⚠ {summary}")];
    if let Some(fault) = fault {
        lines.push(format!(
            "  PROGRAM FAULTED — {fault}; the runtime answers but runs nothing"
        ));
    }
    if latched {
        // One latch flag, whoever set it. When the runtime says why, say that:
        // calling every latch a lost scan deadline sends people to the wrong
        // fault.
        lines.push(match latch_reason {
            Some(why) => format!(
                "  OUTPUTS LATCHED OFF — {why}; zeroed and held off until the \
                 program is restarted"
            ),
            None => "  WATCHDOG LATCHED — scan deadline lost; outputs zeroed and \
                     held off until the program is restarted"
                .into(),
        });
    }
    if !down.is_empty() {
        lines.push(format!(
            "  fieldbus DEGRADED — {} down (inputs frozen, outputs dropped): {}",
            down.len(),
            down.join(", ")
        ));
    }
    if !tap_running {
        lines.push(format!(
            "  SNAPSHOT TAP STOPPED — history is not recording, process alarms are not \
             being evaluated and the scan count is frozen (the program itself keeps \
             running); restart the runtime{}",
            tap_last.map(|t| format!(". Last: {t}")).unwrap_or_default()
        ));
    } else if let Some(ms) = tap_stalled_ms {
        lines.push(format!(
            "  SNAPSHOT TAP STALLED — stuck on one snapshot for {}.{}s: history, process \
             alarms, the scan count and the live feed are not updating (the program itself \
             keeps running); a blocked write is the usual cause",
            ms / 1000,
            (ms % 1000) / 100
        ));
    } else if tap_faults > 0 {
        lines.push(format!(
            "  snapshot tap FAULTED {tap_faults}× since the runtime started (history or \
             alarm evaluation missed snapshots); a count that climbs between probes is \
             failing now{}",
            tap_last
                .map(|t| format!(". Latest: {t}"))
                .unwrap_or_default()
        ));
    }
    // What the runtime will DO about it is what an operator needs next to the
    // warning, so it is said here and not on a healthy edge.
    if !tap_running || tap_stalled_ms.is_some() || tap_faults > 0 {
        lines.push(supervision_line(value.get("supervision")));
    }
    lines
}

/// The alarm-supervision policy the runtime is enforcing, as one line.
fn supervision_line(policy: Option<&serde_json::Value>) -> String {
    let policy = policy.filter(|p| p.is_object());
    let mode = policy
        .and_then(|p| p.get("on_loss"))
        .and_then(|v| v.as_str());
    let grace = policy
        .and_then(|p| p.get("grace_secs"))
        .and_then(|v| v.as_u64());
    // Where it stands now, from the same object: how long it has been lost and,
    // under safe_state, how long is left. `null` is not lost / nothing pending.
    let lost_for = policy
        .and_then(|p| p.get("lost_for_ms"))
        .and_then(|v| v.as_u64());
    let latch_in = policy
        .and_then(|p| p.get("latch_in_ms"))
        .and_then(|v| v.as_u64());
    let standing = match (lost_for, latch_in) {
        (Some(lost), Some(0)) => format!(
            "; lost for {} s, and the outputs are due to be latched off now",
            lost / 1000
        ),
        (Some(lost), Some(left)) => format!(
            "; lost for {} s, the outputs are latched off in {} s unless it comes back",
            lost / 1000,
            left / 1000
        ),
        (Some(lost), None) => format!("; lost for {} s", lost / 1000),
        (None, _) => String::new(),
    };
    match (mode, grace) {
        (Some("safe_state"), Some(grace)) => format!(
            "  policy: on_loss = safe_state — the outputs are latched off if alarm \
             evaluation stays lost for {grace} s{standing}"
        ),
        (Some("report"), _) => {
            format!("  policy: on_loss = report — nothing is stopped automatically{standing}")
        }
        (Some(other), _) => format!("  policy: on_loss = {other}{standing}"),
        (None, _) => "  policy: not reported — this runtime predates `[supervision]`, so a \
                      declared safe_state is NOT in force on this edge"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::reachable_report;
    use serde_json::json;

    fn probe(extra: serde_json::Value) -> serde_json::Value {
        let mut v = json!({
            "reachable": true, "scan_count": 12, "uptime_secs": 30,
            "runtime_version": null, "fieldbus_healthy": true,
            "unhealthy_devices": [], "watchdog_tripped": false,
            "fault": null, "snapshot_tap": null, "supervision": null, "error": null,
        });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v
    }

    #[test]
    fn a_healthy_edge_prints_one_check_line() {
        assert_eq!(
            reachable_report("pi", &probe(json!({}))),
            ["✓ pi reachable · v? · 12 scans · up 30s"]
        );
    }

    /// A runtime whose program died answers every probe; it printed ✓.
    #[test]
    fn a_faulted_edge_prints_the_fault() {
        let lines = reachable_report(
            "pi",
            &probe(json!({"fault": "VM trap in main_inst: DivideByZero"})),
        );
        assert!(lines[0].starts_with('⚠'), "{lines:?}");
        assert!(
            lines[1].contains("PROGRAM FAULTED — VM trap in main_inst: DivideByZero"),
            "{lines:?}"
        );
    }

    /// The task that feeds history and alarms can end while the program runs.
    /// Everything else on the probe is green then; it printed ✓.
    #[test]
    fn an_edge_whose_snapshot_tap_stopped_warns() {
        let lines = reachable_report(
            "pi",
            &probe(json!({"snapshot_tap": {
                "running": false, "faults": 2,
                "last_fault": "fell behind the scan loop: 8 snapshots were skipped"
            }})),
        );
        assert!(lines[0].starts_with('⚠'), "{lines:?}");
        assert!(lines[1].contains("SNAPSHOT TAP STOPPED"), "{lines:?}");
        assert!(lines[1].contains("8 snapshots were skipped"), "{lines:?}");
    }

    #[test]
    fn an_edge_whose_snapshot_tap_faulted_but_runs_says_how_often() {
        let lines = reachable_report(
            "pi",
            &probe(json!({"snapshot_tap": {
                "running": true, "faults": 5,
                "last_fault": "the alarms stage panicked on scan 9: boom"
            }})),
        );
        assert!(lines[0].starts_with('⚠'), "{lines:?}");
        assert!(lines[1].contains("snapshot tap FAULTED 5×"), "{lines:?}");
        assert!(lines[1].contains("alarms stage panicked"), "{lines:?}");
    }

    /// A stage that blocks neither panics nor lags, so only `stalled_for_ms`
    /// says the tap is stuck; `running` is true and `faults` is 0.
    #[test]
    fn an_edge_whose_snapshot_tap_is_stuck_warns() {
        let lines = reachable_report(
            "pi",
            &probe(json!({"snapshot_tap": {
                "running": true, "faults": 0, "last_fault": null, "stalled_for_ms": 4200
            }})),
        );
        assert!(lines[0].starts_with('⚠'), "{lines:?}");
        assert!(lines[1].contains("SNAPSHOT TAP STALLED"), "{lines:?}");
        assert!(lines[1].contains("4.2s"), "{lines:?}");
    }

    /// A warning about alarm evaluation is only half the story: what the runtime
    /// will do about it is the other half, and an older runtime silently ignores
    /// a declared `safe_state`.
    #[test]
    fn a_tap_warning_says_what_the_runtime_will_do_about_it() {
        let stopped = |extra: serde_json::Value| {
            let mut tap = json!({"running": false, "faults": 0, "last_fault": null});
            tap.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            json!({"snapshot_tap": tap})
        };
        let with = |policy: serde_json::Value| {
            let mut v = stopped(json!({}));
            v["supervision"] = policy;
            reachable_report("pi", &probe(v))
        };
        let safe = with(json!({"on_loss": "safe_state", "grace_secs": 45}));
        assert!(
            safe.last().unwrap().contains("safe_state") && safe.last().unwrap().contains("45 s"),
            "{safe:?}"
        );
        let report = with(json!({"on_loss": "report", "grace_secs": 30}));
        assert!(
            report
                .last()
                .unwrap()
                .contains("nothing is stopped automatically"),
            "{report:?}"
        );
        // And how long it has been lost and what is still ahead, when it says.
        let counting = with(json!({
            "on_loss": "safe_state", "grace_secs": 45,
            "lost_for_ms": 12_400, "latch_in_ms": 32_600
        }));
        assert!(
            counting.last().unwrap().contains("lost for 12 s")
                && counting.last().unwrap().contains("latched off in 32 s"),
            "{counting:?}"
        );
        let due = with(json!({
            "on_loss": "safe_state", "grace_secs": 45,
            "lost_for_ms": 45_100, "latch_in_ms": 0
        }));
        assert!(
            due.last().unwrap().contains("due to be latched off now"),
            "{due:?}"
        );
        let reporting = with(json!({
            "on_loss": "report", "grace_secs": 30, "lost_for_ms": 90_000, "latch_in_ms": null
        }));
        assert!(
            reporting.last().unwrap().contains("lost for 90 s")
                && !reporting.last().unwrap().contains("latched off in"),
            "{reporting:?}"
        );
        let older = reachable_report("pi", &probe(stopped(json!({}))));
        assert!(older.last().unwrap().contains("NOT in force"), "{older:?}");
        // A healthy edge says nothing about it.
        let healthy = reachable_report("pi", &probe(json!({})));
        assert_eq!(healthy.len(), 1, "{healthy:?}");
    }

    /// An edge build that predates the field, or a tap that never faulted,
    /// is not a warning.
    #[test]
    fn a_missing_or_clean_snapshot_tap_is_not_a_warning() {
        for extra in [
            json!({}),
            json!({"snapshot_tap": null}),
            json!({"snapshot_tap": {"running": true, "faults": 0, "last_fault": null}}),
        ] {
            let lines = reachable_report("pi", &probe(extra.clone()));
            assert!(lines[0].starts_with('✓'), "{extra}: {lines:?}");
        }
    }

    /// The supervision policy can latch the outputs; it is not a lost scan
    /// deadline, and the probe must not say it is.
    #[test]
    fn a_latch_the_runtime_explains_is_not_called_a_watchdog_trip() {
        let lines = reachable_report(
            "pi",
            &probe(json!({
                "watchdog_tripped": true,
                "latch_reason": "alarm supervision lost for 30 s: the snapshot task has stopped"
            })),
        );
        assert!(lines[0].starts_with('⚠'), "{lines:?}");
        assert!(
            lines[1].contains("OUTPUTS LATCHED OFF — alarm supervision lost"),
            "{lines:?}"
        );
        assert!(!lines[1].contains("WATCHDOG"), "{lines:?}");
    }

    #[test]
    fn a_latched_or_degraded_edge_still_warns() {
        let lines = reachable_report(
            "pi",
            &probe(json!({"watchdog_tripped": true, "unhealthy_devices": ["coupler"]})),
        );
        assert!(lines[0].starts_with('⚠'), "{lines:?}");
        assert!(lines[1].contains("WATCHDOG LATCHED"), "{lines:?}");
        assert!(lines[2].contains("fieldbus DEGRADED — 1 down"), "{lines:?}");
    }
}
