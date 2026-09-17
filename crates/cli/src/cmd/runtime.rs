//! Runtime lifecycle (`cs run` / `cs stop`) and the online debug verbs
//! (`cs runtime pause/resume/step/status/snapshot/force/unforce/write`),
//! plus the value-encoding helpers the force/write paths use. These
//! stay porcelain — force/write carry type-aware bit-packing and the
//! whole family has safety semantics a generic verb shouldn't blur.

use std::path::Path;

use anyhow::{Context, Result};

use crate::http::{print_json, url_encode, Client};
use crate::RuntimeCmd;

pub(crate) fn cmd_run(client: &Client, program: Option<&str>, file: Option<&Path>) -> Result<i32> {
    let body = match (program, file) {
        (None, None) => serde_json::json!({ "kind": "project" }),
        (Some(name), None) => serde_json::json!({
            "kind": "isolated",
            "program": name,
        }),
        (Some(name), Some(path)) => {
            let abs = path
                .canonicalize()
                .with_context(|| format!("resolving {}", path.display()))?;
            serde_json::json!({
                "kind": "isolated",
                "program": name,
                "file_path": abs.display().to_string(),
            })
        }
        (None, Some(_)) => {
            anyhow::bail!("--file requires --program to name the PROGRAM inside it")
        }
    };
    let resp = client.post("/api/run", &body)?;
    print_json(&resp)
}

pub(crate) fn cmd_stop(client: &Client) -> Result<i32> {
    let resp = client.post("/api/stop", &serde_json::json!({}))?;
    print_json(&resp)
}

pub(crate) fn cmd_runtime(client: &Client, cmd: RuntimeCmd, json: bool) -> Result<i32> {
    match cmd {
        RuntimeCmd::Pause { edge } => {
            let resp = post_op(client, edge.as_deref(), "pause", &serde_json::json!({}))?;
            print_json(&resp)
        }
        RuntimeCmd::Resume { edge } => {
            let resp = post_op(client, edge.as_deref(), "resume", &serde_json::json!({}))?;
            print_json(&resp)
        }
        RuntimeCmd::Step { cycles, edge } => {
            let body = serde_json::json!({ "cycles": cycles });
            let resp = post_op(client, edge.as_deref(), "step", &body)?;
            print_json(&resp)
        }
        RuntimeCmd::Status { edge } => {
            let status = match &edge {
                Some(e) => client.get(&format!("/api/edges/{}/status", url_encode(e)))?,
                None => client.get("/api/runtime/status")?,
            };
            if json {
                return print_json(&status);
            }
            let mode = status
                .get("mode")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let forces = status
                .get("forces")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            // Edge /status has no `running` bool — derive from mode.
            let running = status
                .get("running")
                .and_then(|v| v.as_bool())
                .unwrap_or_else(|| mode.get("kind").and_then(|k| k.as_str()) == Some("running"));
            println!(
                "running: {running}  mode: {}  forces: {}",
                serde_json::to_string(&mode)?,
                forces.len(),
            );
            for f in &forces {
                if let (Some(n), Some(v)) = (f.get("name").and_then(|v| v.as_str()), f.get("value"))
                {
                    println!("  {n} := {v}");
                }
            }
            Ok(0)
        }
        RuntimeCmd::Snapshot { vars, edge } => {
            // The one runtime READ agents need most: current values.
            // Local: /api/runtime/snapshot. Edge: last_snapshot off the
            // proxied /status (the edge monitor keeps it fresh).
            let snap = match &edge {
                Some(e) => {
                    let status = client.get(&format!("/api/edges/{}/status", url_encode(e)))?;
                    status
                        .get("last_snapshot")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null)
                }
                None => client.get("/api/runtime/snapshot")?,
            };
            let filtered = match &vars {
                Some(list) => filter_snapshot(&snap, list),
                None => snap,
            };
            // Snapshot output is inherently machine data — always JSON.
            print_json(&filtered)
        }
        RuntimeCmd::Force { name, value, edge } => {
            let resp = match &edge {
                Some(e) => {
                    let encoded =
                        pack_value(&name, edge_var_type(client, e, &name).as_deref(), &value)
                            .map_err(crate::http::UsageError::wrap)?;
                    client.post(
                        &format!("/api/edges/{}/runtime/force", url_encode(e)),
                        &serde_json::json!({ "name": name, "value": encoded }),
                    )?
                }
                None => {
                    let encoded = parse_value(client, &name, &value)?;
                    client.post(
                        &format!("/api/runtime/forces/{}", url_encode(&name)),
                        &serde_json::json!({ "value": encoded }),
                    )?
                }
            };
            print_json(&resp)
        }
        RuntimeCmd::Unforce { name, edge } => {
            let resp = match &edge {
                Some(e) => client.post(
                    &format!("/api/edges/{}/runtime/unforce", url_encode(e)),
                    &serde_json::json!({ "name": name }),
                )?,
                None => client.delete(&format!("/api/runtime/forces/{}", url_encode(&name)))?,
            };
            print_json(&resp)
        }
        RuntimeCmd::Ack { id } => {
            let resp = client.post(
                &format!("/api/runtime/alarms/{}/ack", url_encode(&id)),
                &serde_json::json!({}),
            )?;
            print_json(&resp)
        }
        RuntimeCmd::Write { name, value, edge } => {
            let resp = match &edge {
                Some(e) => {
                    let encoded =
                        pack_value(&name, edge_var_type(client, e, &name).as_deref(), &value)
                            .map_err(crate::http::UsageError::wrap)?;
                    client.post(
                        &format!("/api/edges/{}/runtime/write", url_encode(e)),
                        &serde_json::json!({ "name": name, "value": encoded }),
                    )?
                }
                None => {
                    let encoded = parse_value(client, &name, &value)?;
                    client.post(
                        &format!("/api/runtime/variables/{}", url_encode(&name)),
                        &serde_json::json!({ "value": encoded }),
                    )?
                }
            };
            print_json(&resp)
        }
    }
}

/// pause/resume/step against the local runtime or an edge proxy.
fn post_op(
    client: &Client,
    edge: Option<&str>,
    op: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value> {
    match edge {
        Some(e) => client.post(&format!("/api/edges/{}/runtime/{op}", url_encode(e)), body),
        None => client.post(&format!("/api/runtime/{op}"), body),
    }
}

/// Keep only the named vars (comma-separated) in a snapshot payload.
fn filter_snapshot(snap: &serde_json::Value, vars: &str) -> serde_json::Value {
    let wanted: Vec<&str> = vars
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let Some(arr) = snap.get("vars").and_then(|v| v.as_array()) else {
        return snap.clone();
    };
    let filtered: Vec<serde_json::Value> = arr
        .iter()
        .filter(|v| {
            v.get("name")
                .and_then(|n| n.as_str())
                .map(|n| wanted.contains(&n))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    let mut out = snap.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("vars".into(), serde_json::Value::Array(filtered));
    }
    out
}

/// Convert a human-typed value into the i32 the runtime wire protocol
/// expects, type-aware via the runtime's snapshot.
///
/// Why: the bridge stores all variables — BOOL, INT, REAL, … — in
/// 32-bit slots and the force/write endpoint takes a raw `i32`. For
/// REAL the i32 is the IEEE-754 bit pattern of the float, NOT the
/// integer value. This helper does the conversion so humans (and
/// agents) can use natural notation.
pub(crate) fn parse_value(client: &Client, name: &str, raw: &str) -> Result<i32> {
    let var_type = snapshot_var_type(client, name).unwrap_or_default();
    // A value that doesn't fit its type is the caller's to fix, so it must
    // exit 2 like every other usage problem. Left as a bare anyhow error it
    // fell through main's downcast chain to 3 — "infrastructure failure" —
    // and a script reading the documented exit codes would retry a typo
    // forever.
    pack_value(name, var_type.as_deref(), raw).map_err(crate::http::UsageError::wrap)
}

/// Resolve an edge variable's type from the edge runtime's `/status`
/// (last snapshot, which carries per-variable `type_name`).
fn edge_var_type(client: &Client, edge: &str, name: &str) -> Option<String> {
    let status = client
        .get(&format!("/api/edges/{}/status", url_encode(edge)))
        .ok()?;
    let vars = status.get("last_snapshot")?.get("vars")?.as_array()?;
    for v in vars {
        if v.get("name").and_then(|n| n.as_str()) == Some(name) {
            return v
                .get("type_name")
                .and_then(|t| t.as_str())
                .map(String::from);
        }
    }
    None
}

/// Inclusive value range of an IEC integer type. The i32 force/write wire
/// carries the BITS, so `UDINT`/`DWORD` legitimately exceed `i32::MAX` and
/// are sent as their two's-complement pattern — lossless precisely because
/// the caller range-checks first.
fn int_range(int_type: &str) -> (i64, i64) {
    match int_type {
        "SINT" => (i8::MIN as i64, i8::MAX as i64),
        "USINT" | "BYTE" => (0, u8::MAX as i64),
        "INT" => (i16::MIN as i64, i16::MAX as i64),
        "UINT" | "WORD" => (0, u16::MAX as i64),
        "UDINT" | "DWORD" => (0, u32::MAX as i64),
        // DINT and anything else routed here by the caller's match.
        _ => (i32::MIN as i64, i32::MAX as i64),
    }
}

/// Bit-pack a human value string into the i32 force/write wire, given the
/// variable's IEC `var_type` (None = unknown → guess from value format).
fn pack_value(name: &str, var_type: Option<&str>, raw: &str) -> Result<i32> {
    // BOOL shortcuts. Case-insensitive because TRUE/FALSE are the IEC
    // canonical form but agents type either.
    match raw.to_ascii_lowercase().as_str() {
        "true" => return Ok(1),
        "false" => return Ok(0),
        _ => {}
    }

    match var_type {
        Some("BOOL") => {
            let n: i32 = raw.parse().with_context(|| {
                format!("value `{raw}` doesn't fit BOOL (expected TRUE/FALSE/1/0)")
            })?;
            Ok(if n != 0 { 1 } else { 0 })
        }
        Some("REAL") => {
            let f: f32 = raw
                .parse()
                .with_context(|| format!("value `{raw}` doesn't parse as REAL (32-bit float)"))?;
            // Rust's f32 parse returns Ok(inf) on overflow and Ok(NaN) for
            // "nan", so both would ride the wire as a bit pattern and land in
            // a plant variable. The runtime already refuses NaN where a
            // governance rule has to clamp it; refuse it here too, before it
            // is anyone else's problem.
            if !f.is_finite() {
                anyhow::bail!(
                    "value `{raw}` is not a finite REAL (parsed as {f}) — \
                     a plant variable has no use for it"
                );
            }
            Ok(f.to_bits() as i32)
        }
        Some("LREAL") => {
            anyhow::bail!(
                "LREAL (64-bit float) doesn't fit the 32-bit force wire — \
                 use a REAL variable, or write the low 32 bits manually"
            )
        }
        Some(int_type)
            if matches!(
                int_type,
                "INT" | "DINT" | "SINT" | "UINT" | "UDINT" | "USINT" | "BYTE" | "WORD" | "DWORD"
            ) =>
        {
            let n: i64 = raw.parse().with_context(|| {
                format!("value `{raw}` doesn't parse as integer for {int_type}")
            })?;
            // `n as i32` alone truncates twice over: once here, and again
            // downstream when the runtime stores an i32 into a narrower IEC
            // slot. 40000 into an INT arrives as -25536 — a wrong setpoint,
            // delivered silently, by the one verb that exists BECAUSE it is
            // type-aware. Refuse instead.
            let (lo, hi) = int_range(int_type);
            if n < lo || n > hi {
                anyhow::bail!(
                    "value `{raw}` is outside {int_type} ({lo}..={hi}) — \
                     it would reach the variable truncated"
                );
            }
            // Unsigned types wider than i32 ride the wire as raw bits; the
            // range check above is what makes that lossless.
            Ok(n as i32)
        }
        Some(other) => {
            anyhow::bail!("don't know how to encode value `{raw}` for type {other} (yet)")
        }
        None => {
            // No type info — guess from format and warn loudly.
            if raw.contains('.') || raw.contains('e') || raw.contains('E') {
                let f: f32 = raw.parse().with_context(|| {
                    format!("value `{raw}` looks like a float but doesn't parse as f32")
                })?;
                eprintln!(
                    "note: runtime didn't expose `{name}`'s type — guessed REAL from value format"
                );
                Ok(f.to_bits() as i32)
            } else {
                let n: i32 = raw.parse().with_context(|| {
                    format!("value `{raw}` doesn't parse as i32; if you meant REAL, use `{raw}.0`")
                })?;
                eprintln!("note: runtime didn't expose `{name}`'s type — assumed INT family");
                Ok(n)
            }
        }
    }
}

/// Best-effort variable type lookup via `/api/runtime/snapshot`.
fn snapshot_var_type(client: &Client, name: &str) -> Result<Option<String>> {
    let snap = match client.get("/api/runtime/snapshot") {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let vars = match snap.get("vars").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Ok(None),
    };
    for v in vars {
        if v.get("name").and_then(|n| n.as_str()) == Some(name) {
            return Ok(v
                .get("type_name")
                .and_then(|t| t.as_str())
                .map(String::from));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::pack_value;

    fn ok(ty: &str, raw: &str) -> i32 {
        pack_value("v", Some(ty), raw).unwrap_or_else(|e| panic!("{ty} {raw}: {e:#}"))
    }
    fn err(ty: &str, raw: &str) -> String {
        format!(
            "{:#}",
            pack_value("v", Some(ty), raw).expect_err("{ty} {raw} must be refused")
        )
    }

    #[test]
    fn bools_accept_both_spellings_and_numbers() {
        assert_eq!(ok("BOOL", "TRUE"), 1);
        assert_eq!(ok("BOOL", "false"), 0);
        assert_eq!(ok("BOOL", "7"), 1);
        assert_eq!(ok("BOOL", "0"), 0);
    }

    #[test]
    fn integers_inside_their_type_pack_unchanged() {
        assert_eq!(ok("INT", "-32768"), -32768);
        assert_eq!(ok("INT", "32767"), 32767);
        assert_eq!(ok("SINT", "-128"), -128);
        assert_eq!(ok("USINT", "255"), 255);
        assert_eq!(ok("DINT", "-2147483648"), i32::MIN);
    }

    /// The defect: `n as i32` truncated, and the runtime truncated again into
    /// the narrower IEC slot. 40000 into an INT arrived as -25536 — a wrong
    /// setpoint, silently, from the verb that exists because it is
    /// type-aware.
    #[test]
    fn an_integer_outside_its_type_is_refused_not_truncated() {
        let e = err("INT", "40000");
        assert!(e.contains("outside INT"), "{e}");
        assert!(e.contains("-32768..=32767"), "{e}");
        assert!(e.contains("truncated"), "{e}");

        assert!(err("SINT", "200").contains("outside SINT"));
        assert!(err("USINT", "-1").contains("outside USINT"));
        assert!(err("UINT", "65536").contains("outside UINT"));
        // i64-parseable but far outside i32 — used to wrap to 705032704.
        assert!(err("DINT", "5000000000").contains("outside DINT"));
    }

    /// `UDINT`/`DWORD` legitimately exceed i32 and ride the wire as bits.
    /// The range check is what makes that lossless rather than a wrap.
    #[test]
    fn wide_unsigned_types_ride_the_wire_as_bits() {
        assert_eq!(ok("UDINT", "4294967295"), -1, "0xFFFFFFFF as i32");
        assert_eq!(ok("DWORD", "2147483648"), i32::MIN, "0x80000000 as i32");
        assert!(err("UDINT", "4294967296").contains("outside UDINT"));
        assert!(err("UDINT", "-1").contains("outside UDINT"));
    }

    #[test]
    fn reals_pack_as_ieee_bits() {
        assert_eq!(ok("REAL", "1.5"), 1.5f32.to_bits() as i32);
        assert_eq!(ok("REAL", "-0.25"), (-0.25f32).to_bits() as i32);
    }

    /// Rust parses "1e40" as +inf and "nan" as NaN, both `Ok`. Neither
    /// belongs in a plant variable.
    #[test]
    fn non_finite_reals_are_refused() {
        assert!(err("REAL", "1e40").contains("not a finite REAL"));
        assert!(err("REAL", "nan").contains("not a finite REAL"));
        assert!(err("REAL", "-inf").contains("not a finite REAL"));
    }

    #[test]
    fn lreal_says_why_it_cannot_ride_the_32_bit_wire() {
        assert!(err("LREAL", "1.5").contains("doesn't fit the 32-bit force wire"));
    }

    #[test]
    fn an_unknown_type_is_named_rather_than_guessed() {
        let e = err("STRING", "hello");
        assert!(e.contains("STRING"), "{e}");
    }

    /// With no type info the packer guesses from the value's shape and says
    /// so on stderr; overflow still fails rather than wrapping.
    #[test]
    fn without_type_info_the_guess_still_refuses_an_overflowing_integer() {
        assert_eq!(pack_value("v", None, "42").unwrap(), 42);
        assert_eq!(
            pack_value("v", None, "1.5").unwrap(),
            1.5f32.to_bits() as i32
        );
        assert!(pack_value("v", None, "5000000000").is_err());
    }
}
