//! `cs` — agent-first command-line interface for IA2.
//!
//! The surface is deliberately small, bash-style: a few META-PRIMITIVES
//! that cover every resource, plus the handful of verbs that carry real
//! domain semantics. See `MEMORY/principles.md` § "CLI is the headline
//! agent interface".
//!
//!   Meta-primitives (any resource, present and future):
//!     cs ls [path]              enumerate (no arg = kinds overview)
//!     cs get <path>             read      (POU paths print raw source)
//!     cs set <path> [--from]    create-or-replace (upsert)
//!     cs rm  <path>             delete    (trailing `/` = folder)
//!     cs api <METHOD> <path>    raw escape hatch — full API parity
//!
//!   Domain verbs (semantics a generic verb shouldn't blur):
//!     check · transpile · symbols · run · stop · runtime … ·
//!     deploy · probe · hmi op/generate · library import ·
//!     project check/info/create/open/close · agent run/enter/leave
//!
//! Contract (every command):
//!   * exit 0 — success
//!   * exit 1 — ran fine but found problems in YOUR content
//!     (diagnostics, failed probe, remote deploy failure)
//!   * exit 2 — the request was wrong (usage, bad path, HTTP 4xx);
//!     the server's error body is printed verbatim on stderr
//!   * exit ≥3 — infrastructure failure (server down, 5xx, I/O)
//!   * `--json` (global) switches to machine output; commands whose
//!     output is inherently JSON (get/api/snapshot/…) always emit JSON
//!   * `--project` / `--server` (global) route every request

use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod announce;
mod cmd;
mod http;
mod resource;

use crate::announce::{announce_agent, announce_label};
use crate::cmd::actions::{cmd_hmi_generate, cmd_hmi_op, cmd_library_import};
use crate::cmd::agent::cmd_agent;
use crate::cmd::analysis::{cmd_check, cmd_explain, cmd_symbols, cmd_transpile};
use crate::cmd::api::cmd_api;
use crate::cmd::edge::{cmd_deploy, cmd_probe};
use crate::cmd::project::{
    cmd_project_check, cmd_project_close, cmd_project_create, cmd_project_info, cmd_project_open,
};
use crate::cmd::quartet::{cmd_get, cmd_ls, cmd_rm, cmd_set, parse_query, SetArgs};
use crate::cmd::runtime::{cmd_run, cmd_runtime, cmd_stop};
use crate::cmd::sim::{cmd_sim_run, SimArgs};
use crate::http::{print_json, url_encode, ApiError, Client, UsageError};

// =================================================================
//   Top-level command surface
// =================================================================

/// `cs` — IA2 CLI. A resource quartet (`ls/get/set/rm`), a raw API
/// escape hatch (`api`), and a few domain verbs. Everything else the
/// server can do is one `cs api` away.
#[derive(Parser, Debug)]
#[command(
    name = "cs",
    version,
    about = "IA2 CLI — agent-first automation tools",
    long_about = "\
IA2 CLI (`cs`) — agent-first PLC engineering tools.

The mental model is bash-sized:
  cs ls                      what kinds of resources exist?
  cs ls pous                 what POUs does the project have?
  cs get devices/plc1        read any resource (JSON, or raw POU source)
  cs set devices/plc1 --from cfg.json    create-or-replace (get → edit → set)
  cs rm  hmi/overview        delete
  cs api POST /api/edges/pi/attach       anything else in docs/api.md

Domain verbs with real semantics keep their own commands: check,
transpile, symbols, run, stop, runtime (pause/step/force/…), deploy,
probe, hmi op/generate, library import, project, agent run.

Exit codes: 0 success · 1 problems in your content (diagnostics /
failed probe / remote deploy failure) · 2 bad request (usage, 4xx —
server's reason printed) · ≥3 infrastructure failure.

`--json` on any command yields machine output. Wrap multi-step work in
`cs agent run --label \"...\" -- <cmd>` so the IDE shows one steady
takeover banner.
"
)]
struct Cli {
    /// Target a specific open project on a multi-project server (adds
    /// the X-IA2-Project header to every request). When absent, the
    /// server routes to its active project.
    #[arg(long, global = true)]
    project: Option<String>,

    /// Base URL of the IA2 server. Point it at an edge box (via an
    /// SSH-forwarded port) to reach a remote runtime monitor.
    #[arg(long, global = true, default_value = "http://127.0.0.1:3001")]
    server: String,

    /// Machine output: pass the server's JSON through untouched.
    /// Human-oriented rendering is the default.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// List resources. No argument prints the resource-kind overview —
    /// the self-discovery entry point. `cs ls pous`, `cs ls devices`,
    /// `cs ls edges`, `cs ls hmi`, `cs ls library`, `cs ls projects`,
    /// `cs ls device-catalog`; an extra segment filters by prefix
    /// (`cs ls pous/lib`).
    Ls {
        /// Resource collection (omit for the overview).
        path: Option<String>,
    },

    /// Read one resource by path. `cs get pous/motor.st` prints the
    /// RAW SOURCE (redirect it to a file); everything else prints
    /// JSON. Sub-reads: `edges/<n>/{probe,status,logs,scan,system}`,
    /// `pous/<p>/variables`, `runtime/{status,snapshot,forces}`,
    /// `project{,/variables,/pous}`, `iomap`, `tasks`, `northbound`,
    /// `hmi/<slug>`, `hmi-symbols`.
    Get {
        path: String,
        /// Write this read's quoted ETag to a file, for `set --if-match @FILE`.
        #[arg(long)]
        etag_file: Option<PathBuf>,
        /// Extra query params, repeatable: `--query tail=500`.
        #[arg(long = "query", value_name = "K=V")]
        query: Vec<String>,
    },

    /// Create-or-replace a resource (upsert; the write half of
    /// get → edit → set).
    ///
    /// `cs set pous/motor.st --from - --if-match @motor.etag`  replace source
    /// `cs set pous/seal.ld.json`             scaffold a new LD POU
    /// `cs set devices/plc1 --protocol modbus`  create empty device
    /// `cs set devices/plc1 --from cfg.json --if-match @plc1.etag`  replace config
    /// `cs set edges/pi --host pi@plc.local`  create edge
    /// `cs set iomap --from iomap.json --if-match @iomap.etag`  single-doc config
    /// `cs set pous/util/`                    create a folder
    ///
    /// New POUs take their language from the path's extension
    /// (.st / .ld.json / .fbd.json / .sfc.json).
    /// Capture the version with `cs get <path> --etag-file FILE` BEFORE
    /// editing. Existing targets need --if-match or intentional --force.
    #[command(verbatim_doc_comment)]
    Set {
        path: String,
        /// Replace only the version read earlier: quoted ETag or @FILE.
        #[arg(long, conflicts_with = "force")]
        if_match: Option<String>,
        /// Deliberately replace an existing document without checking its version.
        #[arg(long)]
        force: bool,
        /// Content file, or `-` for stdin. POU paths take raw source;
        /// everything else takes the JSON shape `cs get` returns.
        #[arg(long)]
        from: Option<String>,
        /// (devices, on create) modbus | ethercat | opcua | canopen.
        #[arg(long)]
        protocol: Option<String>,
        /// (edges, on create) SSH host — anything ssh(1) accepts.
        #[arg(long)]
        host: Option<String>,
        /// (hmi, on create) operator-facing screen title.
        #[arg(long)]
        title: Option<String>,
        /// (pous, on create) IEC POU type.
        #[arg(long = "type", default_value = "program",
              value_parser = ["program", "function_block", "function"])]
        pou_type: String,
    },

    /// Delete a resource. A trailing slash removes a FOLDER
    /// (`cs rm pous/old/`); `cs rm library/<name>` drops an imported
    /// library.
    Rm { path: String },

    /// Raw API escape hatch: any endpoint in docs/api.md.
    ///
    /// `cs api GET  /api/runtime/status`
    /// `cs api POST /api/edges/pi/attach`
    /// `cs api POST /api/devices/rio/esi-assemble --from body.json`
    ///
    /// Everything the GUI can do is reachable here by construction.
    /// Prefer the porcelain when one exists — better output and exit
    /// semantics.
    #[command(verbatim_doc_comment)]
    Api {
        /// HTTP method: GET / POST / PUT / DELETE.
        method: String,
        /// Absolute API path starting with `/`.
        path: String,
        /// JSON body file, or `-` for stdin.
        #[arg(long)]
        from: Option<String>,
        /// Extra query params, repeatable: `--query tail=500`.
        #[arg(long = "query", value_name = "K=V")]
        query: Vec<String>,
    },

    /// Validate sources. Primary tool for the edit-validate-fix loop —
    /// cheap (no codegen), call it liberally.
    ///
    /// Targets, mixable:
    ///   * POU files (`pous/*.st`, `motor.ld.json`) — checked TOGETHER,
    ///     so cross-file FUNCTION_BLOCK references resolve like a
    ///     project compile;
    ///   * `hmi/<slug>` — server-side screen check (structure +
    ///     variable existence);
    ///   * a problem code (`P0002`) — prints its full explanation.
    ///
    /// `--explain` appends each diagnostic's explanation in human mode
    /// (JSON always carries it). Exit 1 if anything has errors.
    #[command(verbatim_doc_comment)]
    Check {
        /// Files, `hmi/<slug>` paths, or problem codes.
        #[arg(required = true)]
        targets: Vec<String>,
        /// Append full explanations to human diagnostics.
        #[arg(long)]
        explain: bool,
    },

    /// Show the Structured Text a graphical POU compiles to (LD / FBD /
    /// SFC are lowered to ST before reaching ironplc). `--with-map`
    /// adds the line→element source map as JSON.
    Transpile {
        /// Graphical POU (`.ld.json` / `.fbd.json` / `.sfc.json`).
        file: PathBuf,
        /// Emit `{ "st": "...", "source_map": [...] }` instead.
        #[arg(long)]
        with_map: bool,
    },

    /// List the symbols declared in a POU file — variables, FB
    /// instances, program declarations. Same extraction the editor's
    /// hover uses.
    Symbols {
        /// POU file (`.st`, `.ld.json`, `.fbd.json`, `.sfc.json`).
        file: PathBuf,
        /// Filter to names containing this substring.
        #[arg(long)]
        name: Option<String>,
    },

    /// Start the compiled project (everything in tasks.toml), or one
    /// PROGRAM: `--program NAME` (from the project) / `--program NAME
    /// --file path.st` (isolated file run). Watch values with
    /// `cs runtime snapshot`.
    Run {
        /// PROGRAM name (must be in tasks.toml or in `--file`).
        #[arg(long)]
        program: Option<String>,
        /// File for an isolated, off-task run. Requires `--program`.
        #[arg(long)]
        file: Option<PathBuf>,
    },

    /// Stop the running runtime. No-op if nothing is running.
    Stop,

    /// Online debug controls against the local server (or `--edge NAME`
    /// for a deployed runtime): pause / resume / step / status /
    /// snapshot / force / unforce / write / ack.
    #[command(subcommand)]
    Runtime(RuntimeCmd),

    /// Scenario-driven simulation: run the project against the sim
    /// device layer and PROVE behaviour before hardware.
    ///
    /// `cs sim run scenarios/fill.toml` starts the program, then plays
    /// the scenario top to bottom: `set` writes inputs (the scenario is
    /// the plant), `expect` polls a variable until a condition holds or
    /// its deadline passes, `expect_never` watches a safety property
    /// over a window, `expect_alarm` asserts on the alarm engine.
    /// Exit 0 = every expectation held; 1 = a step failed (report says
    /// which, and the last observed value) — CI-ready.
    #[command(subcommand)]
    Sim(SimCmd),

    /// Push the open project to a configured edge over SSH: tar →
    /// versioned extract → atomic symlink swap → systemd restart.
    /// Exit 0 success · 1 remote failure (read the log) · 2/3 local.
    Deploy {
        /// Edge name (entry in the open project's edge list).
        name: String,
    },

    /// Quick reachability probe of an edge (ssh + curl). Exit 0 if
    /// reachable, 1 if not.
    Probe {
        /// Edge name.
        name: String,
    },

    /// Operator-screen authoring actions. CRUD is the quartet
    /// (`cs ls hmi`, `cs get/set/rm hmi/<slug>`); these two carry the
    /// authoring semantics: `generate` lays a deterministic baseline
    /// from project truth, `op` applies incremental structured edits
    /// that animate live in any open canvas.
    #[command(subcommand)]
    Hmi(HmiCmd),

    /// FB-library actions. Browse with `cs ls library`; remove with
    /// `cs rm library/<name>`.
    #[command(subcommand)]
    Library(LibraryCmd),

    /// Project lifecycle + offline project-directory tools.
    #[command(subcommand)]
    Project(ProjectCmd),

    /// Wrap multi-step work in an explicit takeover session — the IDE
    /// banner stays on with `--label` for the whole run instead of
    /// flickering between every `cs` call.
    ///
    ///   cs agent run --label "rebuilding tank" -- bash -c '...'
    #[command(subcommand)]
    Agent(AgentCmd),
}

#[derive(Subcommand, Debug)]
pub(crate) enum RuntimeCmd {
    /// Halt the scan loop (IO frozen, writes/forces still apply).
    Pause {
        /// Target this edge runtime instead of the local server.
        #[arg(long)]
        edge: Option<String>,
    },
    /// Resume continuous scanning.
    Resume {
        #[arg(long)]
        edge: Option<String>,
    },
    /// Run N scan cycles then auto-pause.
    Step {
        /// Cycles to advance (default 1).
        #[arg(default_value_t = 1)]
        cycles: u32,
        #[arg(long)]
        edge: Option<String>,
    },
    /// Current mode (running / paused / step{N}) + forced variables.
    Status {
        #[arg(long)]
        edge: Option<String>,
    },
    /// Live variable values — the read agents need most. Always JSON.
    /// `--vars a,b` filters to named variables.
    Snapshot {
        /// Comma-separated variable names to keep.
        #[arg(long)]
        vars: Option<String>,
        #[arg(long)]
        edge: Option<String>,
    },
    /// Pin a variable to a value every scan until unforced. Values are
    /// human notation — the CLI bit-packs by the variable's IEC type
    /// (REAL → IEEE-754 bits, BOOL → 0/1).
    ///
    /// The force is applied BEFORE the program runs, so it wins over the
    /// bus but loses to the program: a variable the program assigns every
    /// scan (most outputs) is overwritten by that assignment and the force
    /// never reaches the field. Force works on variables the program only
    /// reads — setpoints, mode requests, jog commands. To override a
    /// program-written output you have to give the program a variable to
    /// read (e.g. an override input it applies last).
    Force {
        name: String,
        value: String,
        #[arg(long)]
        edge: Option<String>,
    },
    /// Release a forced variable.
    Unforce {
        name: String,
        #[arg(long)]
        edge: Option<String>,
    },
    /// One-shot write (the program may overwrite next cycle). Same
    /// value encoding as `force`.
    Write {
        name: String,
        value: String,
        #[arg(long)]
        edge: Option<String>,
    },
    /// Acknowledge an alarm by id (see `cs get runtime/alarms`). A
    /// cleared-but-unacked alarm goes quiet; an active one stays
    /// active-acked until its condition clears.
    Ack { id: String },
}

#[derive(Subcommand, Debug)]
pub(crate) enum SimCmd {
    /// Execute one scenario file against the running server.
    Run {
        /// Scenario TOML (see `cs sim run --help` head comment or the
        /// skill's sim reference for the step vocabulary).
        scenario: String,
        /// Run one PROGRAM instead of the whole tasks.toml schedule.
        #[arg(long)]
        program: Option<String>,
        /// Record every polled snapshot to this JSONL file.
        #[arg(long)]
        trace: Option<String>,
        /// Leave the program running after the scenario finishes.
        #[arg(long)]
        keep_running: bool,
        /// Don't start (or stop) the program — attach to a run the
        /// caller already started.
        #[arg(long)]
        no_run: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum HmiCmd {
    /// Apply structured edits — the incremental authoring surface.
    /// Body is `{"ops":[...]}` or a bare `[...]`; ops are add_node /
    /// update_node / remove_node / set_meta. Batches apply atomically
    /// and animate in open canvases. Get the symbol contract from
    /// `cs get hmi-symbols`.
    Op {
        /// Screen slug.
        path: String,
        /// Ops JSON file, or `-` for stdin.
        #[arg(long)]
        from: String,
    },
    /// Deterministic first-pass screen from project truth (alarmbar,
    /// per-POU sections, indicators/values/setpoints, one trend).
    /// 409 if the screen exists — `--force` regenerates.
    Generate {
        path: String,
        #[arg(long)]
        force: bool,
        /// Operator-facing title.
        #[arg(long)]
        title: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum LibraryCmd {
    /// Vendor registry blocks into the project under `pous/lib/<name>/`.
    /// Omit `--blocks` for the whole library; re-import to update.
    Import {
        /// Registry library name, e.g. `process-control`.
        library: String,
        /// Comma-separated block files (`fb_pid.st,fb_ramp.st`).
        #[arg(long, value_delimiter = ',')]
        blocks: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum ProjectCmd {
    /// Compile every POU + the synthesised CONFIGURATION (offline, on a
    /// project directory). The strongest "is this shippable?" check.
    Check {
        /// Project root (contains `project.toml`).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// List POUs, devices and edges (offline, on a project directory).
    Info {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Create a new project under `~/Documents/IA2/<name>/`.
    Create { name: String },
    /// Open a project by path; becomes the server's active project.
    Open { path: PathBuf },
    /// Close the active project (stops the runtime).
    Close,
}

#[derive(Subcommand, Debug)]
pub(crate) enum AgentCmd {
    /// Wrap a command in a takeover session (cleans up even on Ctrl-C).
    Run {
        /// Banner text — pick something the human will recognise.
        #[arg(long)]
        label: String,
        /// Command + args after `--`.
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Open a session, print its id (for `IA2_AGENT_SESSION`), exit.
    Enter {
        #[arg(long)]
        label: String,
    },
    /// Close the session in `IA2_AGENT_SESSION` (or `--id`). Idempotent.
    Leave {
        #[arg(long)]
        id: Option<String>,
    },
}

/// `cs check` target routing: problem codes print explanations, hmi/
/// slugs check server-side, everything else is a POU file checked
/// together in one batch.
fn cmd_check_dispatch(
    client: &Client,
    targets: &[String],
    json: bool,
    explain: bool,
) -> anyhow::Result<i32> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut codes: Vec<&str> = Vec::new();
    let mut screens: Vec<&str> = Vec::new();
    for t in targets {
        let is_code = t.len() >= 5
            && t.starts_with('P')
            && t[1..].chars().all(|c| c.is_ascii_digit())
            && !std::path::Path::new(t).exists();
        if is_code {
            codes.push(t);
        } else if let Some(slug) = t.strip_prefix("hmi/") {
            if !std::path::Path::new(t).exists() {
                screens.push(slug);
            } else {
                files.push(PathBuf::from(t));
            }
        } else {
            files.push(PathBuf::from(t));
        }
    }

    let mut exit = 0;
    for code in codes {
        exit = exit.max(cmd_explain(code)?);
    }
    for slug in screens {
        let issues = client.get(&format!("/api/hmi/{}/check", url_encode(slug)))?;
        let has_error = issues
            .as_array()
            .map(|a| {
                a.iter()
                    .any(|i| i.get("severity").and_then(|s| s.as_str()) == Some("error"))
            })
            .unwrap_or(false);
        if json {
            print_json(&issues)?;
        } else if let Some(arr) = issues.as_array() {
            if arr.is_empty() {
                eprintln!("✓ hmi/{slug}: no issues");
            }
            for i in arr {
                let sev = i.get("severity").and_then(|s| s.as_str()).unwrap_or("?");
                let msg = i.get("message").and_then(|s| s.as_str()).unwrap_or("");
                let node = i
                    .get("node_id")
                    .and_then(|s| s.as_str())
                    .map(|n| format!(" [{n}]"))
                    .unwrap_or_default();
                eprintln!("{sev}{node}: {msg}");
            }
        }
        if has_error {
            exit = exit.max(1);
        }
    }
    if !files.is_empty() {
        exit = exit.max(cmd_check(&files, json, explain)?);
    }
    Ok(exit)
}

fn main() {
    let args = Cli::parse();
    let client = Client::new(args.server.clone(), args.project.clone());
    let json = args.json;

    // Heartbeat: mutating commands announce to the IDE *before*
    // dispatching, so the takeover overlay renders while the CLI
    // drives IA2. Reads stay silent — querying isn't operating.
    if let Some(label) = announce_label(&args.command) {
        announce_agent(&client.server, &label);
    }

    let result = match args.command {
        Command::Ls { path } => cmd_ls(&client, path.as_deref(), json),
        Command::Get {
            path,
            query,
            etag_file,
        } => match parse_query(&query).map_err(UsageError::wrap) {
            Ok(q) => cmd_get(&client, &path, &q, json, etag_file.as_deref()),
            Err(e) => Err(e),
        },
        Command::Set {
            path,
            if_match,
            force,
            from,
            protocol,
            host,
            title,
            pou_type,
        } => cmd_set(
            &client,
            &path,
            &SetArgs {
                if_match: if_match.as_deref(),
                force,
                from: from.as_deref(),
                protocol: protocol.as_deref(),
                host: host.as_deref(),
                title: title.as_deref(),
                pou_type: &pou_type,
            },
            json,
        ),
        Command::Rm { path } => cmd_rm(&client, &path, json),
        Command::Api {
            method,
            path,
            from,
            query,
        } => match parse_query(&query).map_err(UsageError::wrap) {
            Ok(q) => cmd_api(&client, &method, &path, from.as_deref(), &q),
            Err(e) => Err(e),
        },
        Command::Check { targets, explain } => cmd_check_dispatch(&client, &targets, json, explain),
        Command::Transpile { file, with_map } => cmd_transpile(&file, with_map),
        Command::Symbols { file, name } => cmd_symbols(&file, name.as_deref(), json),
        Command::Run { program, file } => cmd_run(&client, program.as_deref(), file.as_deref()),
        Command::Stop => cmd_stop(&client),
        Command::Runtime(r) => cmd_runtime(&client, r, json),
        Command::Sim(SimCmd::Run {
            scenario,
            program,
            trace,
            keep_running,
            no_run,
        }) => cmd_sim_run(
            &client,
            &SimArgs {
                scenario: &scenario,
                program: program.as_deref(),
                trace: trace.as_deref(),
                keep_running,
                no_run,
            },
            json,
        ),
        Command::Deploy { name } => cmd_deploy(&client, &name, json),
        Command::Probe { name } => cmd_probe(&client, &name, json),
        Command::Hmi(HmiCmd::Op { path, from }) => cmd_hmi_op(&client, &path, &from),
        Command::Hmi(HmiCmd::Generate { path, force, title }) => {
            cmd_hmi_generate(&client, &path, force, title.as_deref())
        }
        Command::Library(LibraryCmd::Import { library, blocks }) => {
            cmd_library_import(&client, &library, &blocks)
        }
        Command::Project(ProjectCmd::Check { path }) => cmd_project_check(&path, json),
        Command::Project(ProjectCmd::Info { path }) => cmd_project_info(&path, json),
        Command::Project(ProjectCmd::Create { name }) => cmd_project_create(&client, &name),
        Command::Project(ProjectCmd::Open { path }) => cmd_project_open(&client, &path),
        Command::Project(ProjectCmd::Close) => cmd_project_close(&client),
        Command::Agent(a) => cmd_agent(&client, a),
    };

    match result {
        Ok(exit) => std::process::exit(exit),
        Err(e) => {
            // The server's error body (ApiError's Display) is the
            // message — surface it verbatim, then map the exit code:
            // 4xx / usage → 2, infra → 3.
            let _ = writeln!(std::io::stderr(), "error: {e:#}");
            let code = if let Some(api) = e.downcast_ref::<ApiError>() {
                api.exit_code()
            } else if e.downcast_ref::<UsageError>().is_some() {
                2
            } else {
                3
            };
            std::process::exit(code);
        }
    }
}
