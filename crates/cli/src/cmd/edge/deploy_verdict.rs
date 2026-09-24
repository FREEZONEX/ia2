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
    let rollback = value.get("rollback").filter(|r| !r.is_null());
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
            lines.push(format!(
                "✗ deployed version {v} to '{name}', but its program is not running: {detail}"
            ));
            match rollback {
                Some(r) => {
                    let what = r.get("detail").and_then(|d| d.as_str()).unwrap_or("");
                    lines.push(format!("  automatic rollback: {what}"));
                    if r.get("to").is_none_or(|t| t.is_null()) {
                        lines.push(
                            "  the new version is still current — fix the program and deploy \
                             again"
                                .into(),
                        );
                    }
                }
                None => lines.push(
                    "  the new version is current — fix the program and deploy again, or roll \
                     back (the log names the previous version)"
                        .into(),
                ),
            }
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
    fn a_rolled_back_deploy_says_where_the_edge_is_now() {
        let lines = deploy_verdict(
            "pi",
            &report(json!({"ok": false,
                "health": {"state": "faulted", "detail": "the program stopped: VM trap in main_inst: X",
                    "unhealthy_devices": []},
                "rollback": {"to": "2026-09-24T01-00-00Z.good",
                    "detail": "rolled back to 2026-09-24T01-00-00Z.good: the program is running (40 scans)",
                    "health": null}})),
        );
        assert!(lines[0].starts_with('✗'), "{lines:?}");
        assert_eq!(
            lines[1],
            "  automatic rollback: rolled back to 2026-09-24T01-00-00Z.good: the program is running (40 scans)"
        );
        assert_eq!(lines.len(), 2, "{lines:?}");
    }

    #[test]
    fn a_rollback_that_could_not_happen_leaves_the_new_version_current() {
        let lines = deploy_verdict(
            "pi",
            &report(json!({"ok": false,
                "health": {"state": "not_running", "detail": "ran no scan within 30 s",
                    "unhealthy_devices": []},
                "rollback": {"to": null,
                    "detail": "nothing to roll back to — there was no previous version (a first install)",
                    "health": null}})),
        );
        assert!(lines[1].contains("no previous version"), "{lines:?}");
        assert!(lines[2].contains("still current"), "{lines:?}");
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
}
