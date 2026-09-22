//! `cs ls / get / set / rm` — the generic resource quartet.
//!
//! All four verbs run through `resource.rs`'s path grammar, one HTTP
//! client, and one output policy:
//!   * `--json` (global) always passes the server payload through;
//!   * human mode renders small tables for `ls` and a handful of
//!     shapes worth summarising (edge logs / scan / system);
//!   * `get pous/<x>` prints the POU SOURCE raw, so
//!     `cs get pous/motor.st > motor.st` round-trips.

use anyhow::{anyhow, Context, Result};

use crate::http::{print_json, read_blob, url_encode, Body, Client, UsageError};
use crate::resource::{self, Plan, Render, SetPlan, KINDS};

/// Usage-shaped failure: exits 2 (caller can fix the invocation).
fn usage(msg: String) -> anyhow::Error {
    UsageError::wrap(anyhow!(msg))
}

fn as_str<'v>(v: &'v serde_json::Value, key: &str) -> &'v str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("?")
}

// ---------------------------------------------------------------- ls

pub(crate) fn cmd_ls(client: &Client, path: Option<&str>, json: bool) -> Result<i32> {
    let Some(plan) = resource::resolve_ls(path).map_err(UsageError::wrap)? else {
        return ls_root(client, json);
    };
    let Plan::Ls { kind, prefix } = plan else {
        unreachable!()
    };

    // Collection → (rows, secondary column) — every row is (name, info).
    let rows: Vec<(String, String)> = match kind {
        "projects" => {
            let v = client.get("/api/projects/open-list")?;
            if json {
                return print_json(&v);
            }
            let active = as_str(&v, "active").to_string();
            let list = v
                .get("projects")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default();
            list.iter()
                .map(|p| {
                    let name = as_str(p, "name");
                    let mark = if name == active { "* " } else { "  " };
                    (format!("{mark}{name}"), as_str(p, "path").to_string())
                })
                .collect()
        }
        "pous" | "devices" | "edges" => {
            let tree = client.get("/api/project")?;
            let arr = tree
                .get(kind)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if json {
                let filtered: Vec<_> = arr
                    .into_iter()
                    .filter(|e| {
                        prefix.is_empty()
                            || as_str(e, "path").starts_with(&prefix)
                            || as_str(e, "name").starts_with(&prefix)
                    })
                    .collect();
                return print_json(&filtered);
            }
            arr.iter()
                .map(|e| match kind {
                    "pous" => {
                        // PouFile: {path, declarations:[{name, type, language}]}
                        let decls = e
                            .get("declarations")
                            .and_then(|d| d.as_array())
                            .cloned()
                            .unwrap_or_default();
                        let info = match decls.first() {
                            Some(d) => format!(
                                "{}  {} {}",
                                as_str(d, "language"),
                                as_str(d, "type"),
                                as_str(d, "name")
                            ),
                            None => String::new(),
                        };
                        (as_str(e, "path").to_string(), info)
                    }
                    "devices" => (
                        as_str(e, "name").to_string(),
                        as_str(e, "protocol").to_string(),
                    ),
                    _ => (as_str(e, "name").to_string(), as_str(e, "host").to_string()),
                })
                .filter(|(name, _)| prefix.is_empty() || name.starts_with(&prefix))
                .collect()
        }
        "hmi" => {
            let v = client.get("/api/hmi")?;
            if json {
                return print_json(&v);
            }
            v.as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|s| {
                    let level = s
                        .get("level")
                        .and_then(|l| l.as_u64())
                        .map(|l| format!("L{l}"))
                        .unwrap_or_default();
                    (
                        as_str(s, "path").to_string(),
                        format!("{}  {}", level, as_str(s, "title")),
                    )
                })
                .filter(|(name, _)| prefix.is_empty() || name.starts_with(&prefix))
                .collect()
        }
        "library" => {
            let v = client.get("/api/library")?;
            if json {
                return print_json(&v);
            }
            v.as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|l| {
                    let name = as_str(l, "name").to_string();
                    let version = as_str(l, "version");
                    let files = l
                        .get("imported_files")
                        .and_then(|f| f.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    let state = match l.get("imported_version").and_then(|v| v.as_str()) {
                        Some(iv) => format!("v{version}  imported(v{iv}, {files} blocks)"),
                        None => format!("v{version}  (not imported)"),
                    };
                    (name, state)
                })
                .collect()
        }
        "device-catalog" => {
            let v = client.get("/api/device-catalog")?;
            if json {
                return print_json(&v);
            }
            v.as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|d| {
                    (
                        as_str(d, "id").to_string(),
                        format!("{}  {}", as_str(d, "protocol"), as_str(d, "title")),
                    )
                })
                .collect()
        }
        _ => unreachable!(),
    };

    if rows.is_empty() {
        eprintln!("(empty)");
        return Ok(0);
    }
    let width = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, info) in &rows {
        if info.trim().is_empty() {
            println!("{name}");
        } else {
            println!("{name:<width$}  {info}");
        }
    }
    Ok(0)
}

/// `cs ls` with no path: the self-discovery entry point — resource
/// kinds on the left, one-line description on the right.
fn ls_root(client: &Client, json: bool) -> Result<i32> {
    if json {
        let kinds: Vec<_> = KINDS
            .iter()
            .map(|(k, d)| serde_json::json!({ "kind": k, "about": d }))
            .collect();
        return print_json(&serde_json::json!({ "kinds": kinds }));
    }
    let width = KINDS.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (k, about) in KINDS {
        println!("{k:<width$}  {about}");
    }
    // Orientation bonus: show open projects when a server is up.
    if let Ok(v) = client.get("/api/projects/open-list") {
        if let Some(list) = v.get("projects").and_then(|p| p.as_array()) {
            if !list.is_empty() {
                let active = as_str(&v, "active").to_string();
                eprintln!();
                for p in list {
                    let name = as_str(p, "name");
                    let mark = if name == active { "*" } else { " " };
                    eprintln!("{mark} {name}  {}", as_str(p, "path"));
                }
            }
        }
    }
    Ok(0)
}

// --------------------------------------------------------------- get

pub(crate) fn cmd_get(
    client: &Client,
    path: &str,
    query: &[(String, String)],
    json: bool,
    etag_file: Option<&std::path::Path>,
) -> Result<i32> {
    match resource::resolve_get(path).map_err(UsageError::wrap)? {
        Plan::GetPou { slug } => {
            let v = get_document(
                client,
                &format!("/api/pous/{}", url_encode(&slug)),
                etag_file,
            )?;
            if json {
                return print_json(&v);
            }
            // Raw source: `cs get pous/x.st > x.st` must round-trip.
            match v.get("source").and_then(|s| s.as_str()) {
                Some(src) => {
                    print!("{src}");
                    if !src.ends_with('\n') {
                        println!();
                    }
                    Ok(0)
                }
                None => print_json(&v),
            }
        }
        Plan::Get { path, render } => {
            let full = append_query(&path, query);
            let v = get_document(client, &full, etag_file)?;
            if json || render == Render::Json {
                return print_json(&v);
            }
            match render {
                Render::EdgeLogs => render_edge_logs(&v),
                Render::EdgeScan => render_edge_scan(&v),
                Render::EdgeSystem => render_edge_system(&v),
                Render::Json => unreachable!(),
            }
        }
        _ => unreachable!(),
    }
}

fn get_document(
    client: &Client,
    path: &str,
    etag_file: Option<&std::path::Path>,
) -> Result<serde_json::Value> {
    let (body, etag) = client.request_versioned("GET", path, Body::None, None, &[])?;
    if let Some(file) = etag_file {
        let etag =
            etag.ok_or_else(|| usage("this resource/server does not provide an ETag".into()))?;
        std::fs::write(file, format!("{etag}\n"))
            .map_err(|e| usage(format!("writing {}: {e}", file.display())))?;
    }
    Ok(body)
}

fn append_query(path: &str, query: &[(String, String)]) -> String {
    if query.is_empty() {
        return path.to_string();
    }
    let qs: Vec<String> = query
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect();
    format!("{path}?{}", qs.join("&"))
}

pub(crate) fn parse_query(raw: &[String]) -> Result<Vec<(String, String)>> {
    raw.iter()
        .map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .with_context(|| format!("--query expects key=value, got `{kv}`"))
        })
        .collect()
}

// --------------------------------------------------------------- set

pub(crate) struct SetArgs<'a> {
    pub if_match: Option<&'a str>,
    pub force: bool,
    pub from: Option<&'a str>,
    pub protocol: Option<&'a str>,
    pub host: Option<&'a str>,
    pub title: Option<&'a str>,
    pub pou_type: &'a str,
}

pub(crate) fn cmd_set(client: &Client, path: &str, args: &SetArgs<'_>, json: bool) -> Result<i32> {
    let plan = resource::resolve_set(path).map_err(UsageError::wrap)?;
    let expected = args.if_match.map(|s| {
        let value = if let Some(file) = s.strip_prefix('@') { read_blob(file)? } else { s.to_string() };
        let value = value.trim();
        if !value.starts_with('"') || !value.ends_with('"') || value.len() < 2 {
            return Err(usage("--if-match requires a quoted strong ETag from `get --etag-file FILE`; use --force for an intentional blind replacement".into()));
        }
        Ok(value.to_owned())
    }).transpose()?;
    let resp = match plan {
        SetPlan::Config { kind } => {
            let from = args.from.ok_or_else(|| {
                usage(format!(
                    "`cs set {kind}` needs `--from <file|->` (JSON document)"
                ))
            })?;
            let body: serde_json::Value = serde_json::from_str(&read_blob(from)?)
                .map_err(|e| usage(format!("parsing JSON from {from}: {e}")))?;
            replace_document(
                client,
                &format!("/api/{kind}"),
                Body::Json(&body),
                expected.as_deref(),
                args.force,
            )?
        }
        SetPlan::Folder { kind, path } => client.post(
            &format!("/api/{kind}/folders"),
            &serde_json::json!({ "path": path }),
        )?,
        SetPlan::Pou { slug, language } => {
            let api = format!("/api/pous/{}", url_encode(&slug));
            let exists = client.exists(&api)?;
            let mut version = expected.clone();
            if !exists {
                if version.is_some() {
                    return Err(usage(
                        "document no longer exists; re-read before editing".into(),
                    ));
                }
                let lang = language.clone().ok_or_else(|| {
                    usage(format!(
                        "POU `{slug}` doesn't exist — write the path with its extension so the \
                         language is known (e.g. `cs set pous/{slug}.st` or `pous/{slug}.ld.json`)"
                    ))
                })?;
                let (_, created_version) = client.request_versioned("POST",
                    "/api/pous",
                    Body::Json(&serde_json::json!({ "path": slug, "language": lang, "type": args.pou_type })),
                    None, &[],
                )?;
                version = created_version;
            }
            match args.from {
                Some(from) => replace_document(
                    client,
                    &api,
                    Body::Text(&read_blob(from)?),
                    version.as_deref(),
                    args.force,
                )?,
                // No body: creating the scaffold was the whole job.
                None if !exists => client.get(&api)?,
                None => {
                    return Err(usage(format!(
                        "POU `{slug}` already exists — pass `--from <file|->` with the new source"
                    )))
                }
            }
        }
        SetPlan::Doc { kind, name } => {
            let api = format!("/api/{kind}/{}", url_encode(&name));
            let body = match args.from {
                Some(from) => Some(
                    serde_json::from_str::<serde_json::Value>(&read_blob(from)?)
                        .map_err(|e| usage(format!("parsing JSON from {from}: {e}")))?,
                ),
                None => None,
            };
            let exists = client.exists(&api)?;
            let mut version = expected.clone();
            if !exists {
                if version.is_some() {
                    return Err(usage(
                        "document no longer exists; re-read before editing".into(),
                    ));
                }
                let create = match kind {
                    "devices" => {
                        let protocol = args
                            .protocol
                            .map(str::to_string)
                            .or_else(|| {
                                body.as_ref()
                                    .and_then(|b| b.get("protocol"))
                                    .and_then(|p| p.as_str())
                                    .map(str::to_string)
                            })
                            .context(
                                "creating a device needs its protocol — pass `--protocol \
                                 modbus|ethercat|opcua|canopen` or include `protocol` in the JSON",
                            )?;
                        serde_json::json!({ "name": name, "protocol": protocol })
                    }
                    "edges" => {
                        let host = args
                            .host
                            .map(str::to_string)
                            .or_else(|| {
                                body.as_ref()
                                    .and_then(|b| b.get("host"))
                                    .and_then(|h| h.as_str())
                                    .map(str::to_string)
                            })
                            .context(
                                "creating an edge needs its SSH host — pass `--host user@box` \
                                 or include `host` in the JSON",
                            )?;
                        serde_json::json!({ "name": name, "host": host })
                    }
                    "hmi" => serde_json::json!({ "path": name, "title": args.title }),
                    _ => unreachable!(),
                };
                let endpoint = format!("/api/{kind}");
                let (_, created_version) =
                    client.request_versioned("POST", &endpoint, Body::Json(&create), None, &[])?;
                version = created_version;
            }
            match body {
                Some(b) => {
                    replace_document(client, &api, Body::Json(&b), version.as_deref(), args.force)?
                }
                None if !exists => client
                    .get(&api)
                    .unwrap_or(serde_json::json!({ "ok": true })),
                None => {
                    return Err(usage(format!(
                        "`{kind}/{name}` already exists — pass `--from <file|->` to replace its \
                         config (get → edit → set)"
                    )))
                }
            }
        }
    };
    if json {
        print_json(&resp)
    } else {
        eprintln!("✓ set {path}");
        Ok(0)
    }
}

fn replace_document(
    client: &Client,
    path: &str,
    body: Body<'_>,
    etag: Option<&str>,
    force: bool,
) -> Result<serde_json::Value> {
    let headers = match etag {
        Some(tag) => vec![("If-Match", tag)],
        None if force => vec![],
        None => return Err(usage("replacing an existing document requires --if-match <ETag|@FILE> from your original `cs get --etag-file FILE`, or --force for an intentional replacement; re-read and reapply your edit, never fetch a fresh version for a stale copy".into())),
    };
    client
        .request_versioned("PUT", path, body, None, &headers)
        .map(|(body, _)| body)
}

// ---------------------------------------------------------------- rm

pub(crate) fn cmd_rm(client: &Client, path: &str, json: bool) -> Result<i32> {
    let Plan::Rm { path: url, label } = resource::resolve_rm(path).map_err(UsageError::wrap)?
    else {
        unreachable!()
    };
    let resp = client.delete(&url)?;
    if json {
        print_json(&resp)
    } else {
        eprintln!("✓ removed {label}");
        Ok(0)
    }
}

// ------------------------------------------------- human renderers

fn render_edge_logs(v: &serde_json::Value) -> Result<i32> {
    match v.get("lines").and_then(|l| l.as_array()) {
        Some(lines) => {
            for line in lines {
                if let Some(s) = line.as_str() {
                    println!("{s}");
                }
            }
            Ok(0)
        }
        None => print_json(v),
    }
}

fn render_edge_scan(v: &serde_json::Value) -> Result<i32> {
    let Some(devs) = v.as_array() else {
        return print_json(v);
    };
    if devs.is_empty() {
        eprintln!("no devices in project");
    }
    for d in devs {
        let name = as_str(d, "name");
        let proto = as_str(d, "protocol");
        let connected = d
            .get("connected")
            .and_then(|c| c.as_bool())
            .unwrap_or(false);
        if !connected {
            let err = d
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("not connected");
            println!("✗ {name} ({proto}) — {err}");
            continue;
        }
        let slaves = d.get("slaves").and_then(|s| s.as_array());
        println!(
            "✓ {name} ({proto}) connected · {} slave(s)",
            slaves.map(|a| a.len()).unwrap_or(0)
        );
        if let Some(arr) = slaves {
            for s in arr {
                let idx = s.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let sn = as_str(s, "name");
                let vid = s.get("vendor_id").and_then(|v| v.as_u64()).unwrap_or(0);
                let pid = s.get("product_id").and_then(|v| v.as_u64()).unwrap_or(0);
                let inb = s.get("input_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                let outb = s.get("output_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                println!(
                    "    [{idx}] {sn}  vendor=0x{vid:08x} product=0x{pid:08x}  in={inb}B out={outb}B"
                );
            }
        }
    }
    Ok(0)
}

fn render_edge_system(v: &serde_json::Value) -> Result<i32> {
    println!("{}/{}", as_str(v, "os"), as_str(v, "arch"));
    if let Some(nics) = v.get("nics").and_then(|n| n.as_array()) {
        println!("NICs:");
        for n in nics {
            let nm = as_str(n, "name");
            let st = as_str(n, "operstate");
            let carrier = n.get("carrier").and_then(|c| c.as_bool()).unwrap_or(false);
            let mac = n.get("mac").and_then(|m| m.as_str()).unwrap_or("");
            let link = if carrier { "carrier" } else { "no-carrier" };
            println!("  {nm:<16} {st:<8} {link:<11} {mac}");
        }
    }
    match v.get("serial_ports").and_then(|p| p.as_array()) {
        Some(ports) if !ports.is_empty() => {
            println!("serial ports:");
            for p in ports {
                if let Some(s) = p.as_str() {
                    println!("  {s}");
                }
            }
        }
        _ => println!("serial ports: (none)"),
    }
    Ok(0)
}
