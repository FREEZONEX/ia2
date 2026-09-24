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
        // "Reachable" only means the runtime answered. A live scan loop on
        // top of a dead fieldbus must not print the same ✓ as a healthy
        // edge — that is the reading that sends people hunting the wrong
        // fault. Exit code stays 0 (it IS reachable); the text tells the
        // truth about the buses.
        let down: Vec<&str> = value
            .get("unhealthy_devices")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|d| d.as_str()).collect())
            .unwrap_or_default();
        // A latched watchdog is the harsher version of the same trap: the
        // runtime answers, every bus is healthy, the scan count climbs —
        // and not one output is being driven. It must never print ✓.
        let latched = value
            .get("watchdog_tripped")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if down.is_empty() && !latched {
            println!("✓ {name} reachable · v{version} · {scans} scans · up {uptime}");
        } else {
            println!("⚠ {name} reachable · v{version} · {scans} scans · up {uptime}");
            if latched {
                println!(
                    "  WATCHDOG LATCHED — scan deadline lost; outputs zeroed and \
                     held off until the program is restarted"
                );
            }
            if !down.is_empty() {
                println!(
                    "  fieldbus DEGRADED — {} down (inputs frozen, outputs dropped): {}",
                    down.len(),
                    down.join(", ")
                );
            }
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
