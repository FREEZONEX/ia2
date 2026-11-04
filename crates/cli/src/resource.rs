//! The resource-path grammar behind `cs ls / get / set / rm`.
//!
//! One meta-primitive instead of a subcommand per noun: a project
//! resource is addressed by the same slash-path it has on disk and in
//! the HTTP API, and the four verbs map onto plain REST:
//!
//!   cs ls  pous                → enumerate a collection
//!   cs get devices/plc1        → GET  /api/devices/plc1
//!   cs set devices/plc1 --from → PUT  /api/devices/plc1  (create on 404)
//!   cs rm  hmi/overview        → DELETE /api/hmi/overview
//!
//! Adding a new resource family to the server extends this table by
//! one match arm (or zero, for `cs api`) — never a new subcommand.
//!
//! Grammar notes:
//!   * the first segment names the resource kind;
//!   * a trailing `/` names a FOLDER (pous/devices/edges support them);
//!   * a reserved last segment names a sub-read (`edges/pi/logs`);
//!   * POU paths may carry their on-disk extension (`motor.ld.json`) —
//!     the API's slug is extension-less, and on create the extension is
//!     what picks the language.

use anyhow::{bail, Result};

use crate::http::url_encode;

/// Resource kinds the quartet understands. Everything else is a typo —
/// or a job for `cs api`.
pub(crate) const KINDS: &[(&str, &str)] = &[
    ("projects", "open projects on the server (ls)"),
    ("project", "project tree · project/variables · project/pous"),
    (
        "pous",
        "POU source files (get prints source; set writes it)",
    ),
    (
        "devices",
        "field devices (JSON config docs; + devices/<n>/describe)",
    ),
    (
        "edges",
        "deploy targets (+ edges/<n>/probe|status|logs|scan|system|audit)",
    ),
    ("hmi", "operator screens (JSON docs; see also `cs hmi op`)"),
    ("iomap", "variable ↔ device.channel bindings (one doc)"),
    ("tasks", "task → program schedule (one doc)"),
    ("northbound", "MQTT northbound config (one doc)"),
    ("alarms", "alarm definitions (one doc; alarms.toml)"),
    ("library", "FB library registry (ls; rm library/<name>)"),
    ("device-catalog", "known-device templates (ls)"),
    (
        "runtime",
        "runtime/{status,snapshot,forces,history,alarms,alarms-journal}",
    ),
    ("hmi-symbols", "HMI symbol palette contract (get)"),
];

/// Sub-reads: `<kind>/<name>/<reserved>` → GET on the corresponding
/// API sub-route. Names can't collide because these segments are
/// reserved within their kind (an edge literally named `logs` would
/// need `cs api`).
const EDGE_SUBREADS: &[&str] = &["probe", "status", "logs", "scan", "system", "audit"];

/// Sub-reads for devices — reserved segments like `EDGE_SUBREADS`.
/// `describe` is the deterministic per-device agent reference file
/// (ADR-0002); a device literally named `describe` would need `cs api`.
const DEVICE_SUBREADS: &[&str] = &["describe"];

/// What a quartet verb resolved to. The caller (cmd/quartet.rs) turns
/// this into HTTP + rendering.
pub(crate) enum Plan {
    /// Plain JSON GET → print (with an optional human renderer key).
    Get { path: String, render: Render },
    /// GET a POU: print `.source` raw by default.
    GetPou { slug: String },
    /// Enumerate a collection.
    Ls { kind: &'static str, prefix: String },
    /// DELETE a resource or folder.
    Rm { path: String, label: String },
}

/// How `cs set` reaches its target.
pub(crate) enum SetPlan {
    /// PUT text source to /api/pous/{slug}; on 404 create first
    /// (language from the extension) then PUT.
    Pou {
        slug: String,
        /// `st` / `ld` / `fbd` / `sfc` — known only when the caller
        /// wrote an extension; needed for create-on-miss.
        language: Option<String>,
    },
    /// PUT JSON to /api/{kind}/{name}; on 404 POST /api/{kind} with a
    /// create body derived from the config (+ convenience flags).
    Doc { kind: &'static str, name: String },
    /// Single-document config: PUT /api/{kind} (iomap / tasks /
    /// northbound). Create never applies.
    Config { kind: &'static str },
    /// POST /api/{kind}/folders {path}
    Folder { kind: &'static str, path: String },
}

/// Which pretty-printer `cs get` should use in human mode. JSON mode
/// always passes the payload through untouched.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Render {
    Json,
    EdgeLogs,
    EdgeScan,
    EdgeSystem,
}

fn split(path: &str) -> (String, Vec<String>) {
    let trimmed = path.trim_matches('/');
    let mut parts = trimmed.split('/').map(str::to_string);
    let kind = parts.next().unwrap_or_default();
    (kind, parts.collect())
}

fn kind_static(kind: &str) -> Option<&'static str> {
    KINDS.iter().map(|(k, _)| *k).find(|k| *k == kind)
}

pub(crate) fn known_kinds_hint() -> String {
    let names: Vec<&str> = KINDS.iter().map(|(k, _)| *k).collect();
    format!("known resource kinds: {}", names.join(", "))
}

/// Strip a known POU extension, returning (slug, language).
fn pou_slug(path: &str) -> (String, Option<String>) {
    for (ext, lang) in [
        (".ld.json", "ld"),
        (".fbd.json", "fbd"),
        (".sfc.json", "sfc"),
        (".st", "st"),
    ] {
        if let Some(stripped) = path.strip_suffix(ext) {
            return (stripped.to_string(), Some(lang.to_string()));
        }
    }
    (path.to_string(), None)
}

/// Resolve `cs ls [path]`.
pub(crate) fn resolve_ls(path: Option<&str>) -> Result<Option<Plan>> {
    let Some(path) = path else {
        return Ok(None); // root overview, handled by the caller
    };
    let (kind, rest) = split(path);
    let Some(kind) = kind_static(&kind) else {
        bail!(
            "`cs ls {path}`: unknown resource kind — {}",
            known_kinds_hint()
        );
    };
    match kind {
        "projects" | "pous" | "devices" | "edges" | "hmi" | "library" | "device-catalog" => {
            Ok(Some(Plan::Ls {
                kind,
                prefix: rest.join("/"),
            }))
        }
        "iomap" | "tasks" | "northbound" | "alarms" | "project" | "runtime" | "hmi-symbols" => {
            bail!("`{kind}` is a single document — use `cs get {kind}`")
        }
        _ => unreachable!(),
    }
}

/// Resolve `cs get <path>`.
pub(crate) fn resolve_get(path: &str) -> Result<Plan> {
    let (kind, rest) = split(path);
    let Some(kind) = kind_static(&kind) else {
        bail!(
            "`cs get {path}`: unknown resource kind — {}",
            known_kinds_hint()
        );
    };
    let joined = rest.join("/");
    let sdo_at = match kind {
        "runtime" if rest.first().map(String::as_str) == Some("sdo") => Some(0),
        "edges" => rest
            .iter()
            .rposition(|s| s == "sdo")
            .filter(|i| *i > 0 && rest.len() == *i + 5),
        _ => None,
    };
    if let Some(i) = sdo_at {
        if rest.len() != i + 5 {
            bail!("expected {kind}/.../sdo/<device>/<slave>/<index>/<subindex>");
        }
        let base = if kind == "runtime" {
            "/api/runtime/sdo".to_string()
        } else {
            format!("/api/edges/{}/sdo", url_encode(&rest[..i].join("/")))
        };
        return Ok(Plan::Get {
            path: format!(
                "{base}?device={}&slave={}&index={}&subindex={}",
                url_encode(&rest[i + 1]),
                url_encode(&rest[i + 2]),
                url_encode(&rest[i + 3]),
                url_encode(&rest[i + 4])
            ),
            render: Render::Json,
        });
    }
    let plan = match kind {
        "iomap" | "tasks" | "northbound" | "alarms" | "hmi-symbols" => {
            if !rest.is_empty() {
                bail!("`{kind}` is a single document — `cs get {kind}`");
            }
            Plan::Get {
                path: format!("/api/{kind}"),
                render: Render::Json,
            }
        }
        "project" => match joined.as_str() {
            "" => Plan::Get {
                path: "/api/project".into(),
                render: Render::Json,
            },
            "variables" | "pous" => Plan::Get {
                path: format!("/api/project/{joined}"),
                render: Render::Json,
            },
            other => bail!("`cs get project/{other}`: expected project, project/variables or project/pous"),
        },
        "runtime" => match joined.as_str() {
            "status" | "snapshot" | "forces" | "history" | "alarms" | "alarms-journal" => {
                Plan::Get {
                    path: format!("/api/runtime/{joined}"),
                    render: Render::Json,
                }
            }
            other => bail!(
                "`cs get runtime/{other}`: expected runtime/status, snapshot, forces, history, alarms or alarms-journal"
            ),
        },
        "pous" => {
            if rest.is_empty() {
                bail!("`cs get pous` lists nothing — use `cs ls pous`, or name one: `cs get pous/<path>`");
            }
            if rest.last().map(String::as_str) == Some("variables") && rest.len() > 1 {
                let name = rest[..rest.len() - 1].join("/");
                let (slug, _) = pou_slug(&name);
                Plan::Get {
                    path: format!("/api/pous/{}/variables", url_encode(&slug)),
                    render: Render::Json,
                }
            } else {
                let (slug, _) = pou_slug(&joined);
                Plan::GetPou { slug }
            }
        }
        "devices" => {
            if rest.is_empty() {
                bail!("name a device: `cs get devices/<name>` (list with `cs ls devices`)");
            }
            let last = rest.last().map(String::as_str).unwrap_or_default();
            if rest.len() > 1 && DEVICE_SUBREADS.contains(&last) {
                let name = rest[..rest.len() - 1].join("/");
                Plan::Get {
                    path: format!("/api/devices/{}/{last}", url_encode(&name)),
                    render: Render::Json,
                }
            } else {
                Plan::Get {
                    path: format!("/api/devices/{}", url_encode(&joined)),
                    render: Render::Json,
                }
            }
        }
        "edges" => {
            if rest.is_empty() {
                bail!("name an edge: `cs get edges/<name>` (list with `cs ls edges`)");
            }
            let last = rest.last().map(String::as_str).unwrap_or_default();
            if rest.len() > 1 && EDGE_SUBREADS.contains(&last) {
                let name = rest[..rest.len() - 1].join("/");
                // `scan` reads the discovery endpoint.
                let (route, render) = match last {
                    "scan" => ("discover".to_string(), Render::EdgeScan),
                    "logs" => ("logs".to_string(), Render::EdgeLogs),
                    "system" => ("system".to_string(), Render::EdgeSystem),
                    other => (other.to_string(), Render::Json),
                };
                Plan::Get {
                    path: format!("/api/edges/{}/{route}", url_encode(&name)),
                    render,
                }
            } else {
                Plan::Get {
                    path: format!("/api/edges/{}", url_encode(&joined)),
                    render: Render::Json,
                }
            }
        }
        "hmi" => {
            if rest.is_empty() {
                bail!("name a screen: `cs get hmi/<slug>` (list with `cs ls hmi`)");
            }
            Plan::Get {
                path: format!("/api/hmi/{}", url_encode(&joined)),
                render: Render::Json,
            }
        }
        "library" | "device-catalog" | "projects" => {
            bail!("`cs get {path}`: use `cs ls {kind}` to enumerate; `cs rm library/<name>` to remove an import")
        }
        _ => unreachable!(),
    };
    Ok(plan)
}

/// Resolve `cs set <path>`.
pub(crate) fn resolve_set(path: &str) -> Result<SetPlan> {
    let (kind, rest) = split(path);
    let folder = path.ends_with('/');
    let Some(kind) = kind_static(&kind) else {
        bail!(
            "`cs set {path}`: unknown resource kind — {}",
            known_kinds_hint()
        );
    };
    let joined = rest.join("/");
    let plan = match kind {
        "iomap" | "tasks" | "northbound" | "alarms" => {
            if !rest.is_empty() {
                bail!("`{kind}` is a single document — `cs set {kind} --from <file>`");
            }
            SetPlan::Config { kind }
        }
        "pous" => {
            if joined.is_empty() {
                bail!("name a POU: `cs set pous/<path>.st --from <file>`");
            }
            if folder {
                SetPlan::Folder { kind, path: joined }
            } else {
                let (slug, language) = pou_slug(&joined);
                SetPlan::Pou { slug, language }
            }
        }
        "devices" | "edges" | "hmi" => {
            if joined.is_empty() {
                bail!("name the resource: `cs set {kind}/<name> …`");
            }
            if folder {
                if kind == "hmi" {
                    bail!("HMI screens don't have folders — slugs may contain slashes directly");
                }
                SetPlan::Folder { kind, path: joined }
            } else {
                SetPlan::Doc { kind, name: joined }
            }
        }
        other => bail!(
            "`cs set {other}/...` isn't writable — {}",
            known_kinds_hint()
        ),
    };
    Ok(plan)
}

/// Resolve `cs rm <path>` to a DELETE URL + a human label.
pub(crate) fn resolve_rm(path: &str) -> Result<Plan> {
    let (kind, rest) = split(path);
    let folder = path.ends_with('/');
    let Some(kind) = kind_static(&kind) else {
        bail!(
            "`cs rm {path}`: unknown resource kind — {}",
            known_kinds_hint()
        );
    };
    let joined = rest.join("/");
    if joined.is_empty() {
        bail!("name what to remove: `cs rm {kind}/<name>`");
    }
    let url = match kind {
        "pous" if folder => format!("/api/pous/folders/{}", url_encode(&joined)),
        "devices" if folder => format!("/api/devices/folders/{}", url_encode(&joined)),
        "edges" if folder => format!("/api/edges/folders/{}", url_encode(&joined)),
        "pous" => {
            let (slug, _) = pou_slug(&joined);
            format!("/api/pous/{}", url_encode(&slug))
        }
        "devices" => format!("/api/devices/{}", url_encode(&joined)),
        "edges" => format!("/api/edges/{}", url_encode(&joined)),
        "hmi" => format!("/api/hmi/{}", url_encode(&joined)),
        "library" => format!("/api/library/{}", url_encode(&joined)),
        other => bail!(
            "`cs rm {other}/...` isn't removable — {}",
            known_kinds_hint()
        ),
    };
    Ok(Plan::Rm {
        path: url,
        label: format!("{kind}/{joined}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_url(p: &str) -> String {
        match resolve_get(p).unwrap() {
            Plan::Get { path, .. } => path,
            Plan::GetPou { slug } => format!("pou:{slug}"),
            _ => panic!("unexpected plan"),
        }
    }

    #[test]
    fn get_maps_single_docs_and_subreads() {
        assert_eq!(get_url("iomap"), "/api/iomap");
        assert_eq!(get_url("tasks"), "/api/tasks");
        assert_eq!(get_url("project/variables"), "/api/project/variables");
        assert_eq!(get_url("runtime/snapshot"), "/api/runtime/snapshot");
        assert_eq!(get_url("edges/pi/logs"), "/api/edges/pi/logs");
        assert_eq!(get_url("edges/pi/scan"), "/api/edges/pi/discover");
        // The write-audit ring reads through the quartet too (ADR-0002).
        assert_eq!(get_url("edges/pi/audit"), "/api/edges/pi/audit");
        assert_eq!(get_url("devices/plc1"), "/api/devices/plc1");
        assert_eq!(
            get_url("devices/plc1/describe"),
            "/api/devices/plc1/describe"
        );
        // Nested device names keep their slashes through the subread.
        assert_eq!(
            get_url("devices/plant a/plc1/describe"),
            "/api/devices/plant%20a%2Fplc1/describe"
        );
        assert_eq!(get_url("hmi/overview"), "/api/hmi/overview");
    }

    #[test]
    fn pou_paths_strip_extensions() {
        assert_eq!(get_url("pous/motor.ld.json"), "pou:motor");
        assert_eq!(get_url("pous/main.st"), "pou:main");
        assert_eq!(get_url("pous/lib/pid/fb_pid.st"), "pou:lib/pid/fb_pid");
        match resolve_set("pous/motor.ld.json").unwrap() {
            SetPlan::Pou { slug, language } => {
                assert_eq!(slug, "motor");
                assert_eq!(language.as_deref(), Some("ld"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn nested_names_percent_encode() {
        assert_eq!(
            get_url("devices/plant a/plc1"),
            "/api/devices/plant%20a%2Fplc1"
        );
    }

    #[test]
    fn rm_maps_folders_and_library() {
        match resolve_rm("pous/old/").unwrap() {
            Plan::Rm { path, .. } => assert_eq!(path, "/api/pous/folders/old"),
            _ => panic!(),
        }
        match resolve_rm("library/process-control").unwrap() {
            Plan::Rm { path, .. } => assert_eq!(path, "/api/library/process-control"),
            _ => panic!(),
        }
        match resolve_rm("pous/motor.st").unwrap() {
            Plan::Rm { path, .. } => assert_eq!(path, "/api/pous/motor"),
            _ => panic!(),
        }
    }

    #[test]
    fn unknown_kind_is_a_usage_error() {
        assert!(resolve_get("gadgets/x").is_err());
        assert!(resolve_set("runtime/status").is_err());
        assert!(resolve_ls(Some("iomap")).is_err());
    }
}
