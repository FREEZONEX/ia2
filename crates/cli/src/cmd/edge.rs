//! `cs deploy` / `cs probe` — the two edge orchestration verbs. Edge
//! CRUD is `cs set/get/rm edges/<name>`; edge sub-reads are
//! `cs get edges/<name>/{probe,status,logs,scan,system}`; attach /
//! detach are `cs api POST /api/edges/<name>/attach|detach`.

use anyhow::Result;

use crate::http::{url_encode, Body, Client};

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
        let version = value.get("version").and_then(|v| v.as_str()).unwrap_or("?");
        let log = value.get("log").and_then(|v| v.as_str()).unwrap_or("");
        if !log.is_empty() {
            eprintln!("{log}");
        }
        let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if ok {
            eprintln!("✓ deployed to '{name}' as version {version}");
        } else {
            eprintln!("✗ deploy to '{name}' FAILED — read the log above");
        }
        if let Some(w) = value.get("warning").and_then(|v| v.as_str()) {
            eprintln!("⚠ {w}");
        }
    }
    // ok=false means the script ran but exited non-zero (remote failure).
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
    // Harsher still: the program itself is gone, and only the reason says so.
    let fault = value.get("fault").and_then(|v| v.as_str());
    let summary = format!("{name} reachable · v{version} · {scans} scans · up {uptime}");
    if down.is_empty() && !latched && fault.is_none() {
        return vec![format!("✓ {summary}")];
    }
    let mut lines = vec![format!("⚠ {summary}")];
    if let Some(fault) = fault {
        lines.push(format!(
            "  PROGRAM FAULTED — {fault}; the runtime answers but runs nothing"
        ));
    }
    if latched {
        lines.push(
            "  WATCHDOG LATCHED — scan deadline lost; outputs zeroed and \
             held off until the program is restarted"
                .into(),
        );
    }
    if !down.is_empty() {
        lines.push(format!(
            "  fieldbus DEGRADED — {} down (inputs frozen, outputs dropped): {}",
            down.len(),
            down.join(", ")
        ));
    }
    lines
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
            "fault": null, "error": null,
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
