//! What `cs deploy` prints after the remote log: the verdict, from the
//! report's `ok`, `version`, `health` and `warning`.

/// The verdict lines `cs deploy` prints after the log. A restart that
/// systemd accepted is not a running program: the report's `health` says
/// whether the deployed program actually runs, and a deploy whose program
/// does not is a failure even though its files are in place.
pub(super) fn deploy_verdict(name: &str, value: &serde_json::Value) -> Vec<String> {
    let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let version = value
        .get("version")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty());
    let health = value.get("health").filter(|h| !h.is_null());
    let state = health.and_then(|h| h.get("state")).and_then(|s| s.as_str());
    let detail = health
        .and_then(|h| h.get("detail"))
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let mut lines = Vec::new();
    match (ok, version) {
        (true, v) => {
            let v = v.unwrap_or("?");
            lines.push(match state {
                Some("running") => format!("✓ deployed to '{name}' as version {v} — {detail}"),
                _ => format!("✓ deployed to '{name}' as version {v}"),
            });
            if state == Some("not_checked") {
                lines.push(format!("  program state not checked: {detail}"));
            }
        }
        (false, Some(v)) if health.is_some() => {
            let outcome = if matches!(state, Some("faulted" | "not_running")) {
                "its program is not running"
            } else {
                "program state is unconfirmed"
            };
            lines.push(format!(
                "✗ deployed version {v} to '{name}', but {outcome}: {detail}"
            ));
            lines.push(
                "  the new version is current — inspect runtime and plant state before restarting or rolling \
                 back (the log names the previous version)"
                    .into(),
            );
        }
        (false, _) => lines.push(format!("✗ deploy to '{name}' FAILED — read the log above")),
    }
    if let Some(w) = value.get("warning").and_then(|v| v.as_str()) {
        lines.push(format!("⚠ {w}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::deploy_verdict;
    use serde_json::json;

    fn report(extra: serde_json::Value) -> serde_json::Value {
        let mut v = json!({
            "ok": true, "version": "2026-09-24T02-00-00Z.abc", "log": "",
            "warning": null, "health": null,
        });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v
    }

    #[test]
    fn a_deploy_whose_program_runs_says_so() {
        let lines = deploy_verdict(
            "pi",
            &report(json!({"health": {"state": "running",
                "detail": "the program is running (57 scans)", "unhealthy_devices": []}})),
        );
        assert_eq!(
            lines,
            ["✓ deployed to 'pi' as version 2026-09-24T02-00-00Z.abc — the program is running (57 scans)"]
        );
    }

    /// The restart succeeded and the program trapped at once. `cs deploy`
    /// printed ✓ and exited 0.
    #[test]
    fn a_deploy_whose_program_faulted_fails_with_the_reason() {
        let lines = deploy_verdict(
            "pi",
            &report(json!({"ok": false, "health": {"state": "faulted",
                "detail": "the program stopped: VM trap in main_inst: DivideByZero",
                "unhealthy_devices": []}})),
        );
        assert!(
            lines[0].starts_with(
                "✗ deployed version 2026-09-24T02-00-00Z.abc to 'pi', but its program is not \
                 running: the program stopped: VM trap"
            ),
            "{lines:?}"
        );
        assert!(lines[1].contains("roll"), "{lines:?}");
    }

    #[test]
    fn a_deploy_that_failed_before_the_restart_points_at_the_log() {
        let lines = deploy_verdict("pi", &report(json!({"ok": false, "version": ""})));
        assert_eq!(lines, ["✗ deploy to 'pi' FAILED — read the log above"]);
    }

    #[test]
    fn a_deploy_without_a_restart_says_nothing_was_checked() {
        let lines = deploy_verdict(
            "pi",
            &report(json!({"health": {"state": "not_checked",
                "detail": "nothing was restarted", "unhealthy_devices": []}})),
        );
        assert!(lines[0].starts_with('✓'), "{lines:?}");
        assert_eq!(
            lines[1],
            "  program state not checked: nothing was restarted"
        );
    }
    #[test]
    fn an_unknown_state_is_not_reported_as_stopped() {
        let lines = deploy_verdict(
            "pi",
            &report(json!({"ok": false,
            "health": {"state": "unknown", "detail": "status read timed out"}})),
        );
        assert!(lines[0].contains("state is unconfirmed"), "{lines:?}");
        assert!(!lines[0].contains("not running"), "{lines:?}");
        assert!(lines[1].contains("inspect runtime and plant state"));
    }
}
