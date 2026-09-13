//! Wraps vendored ironplc parser + analyzer + codegen + VM into a single
//! `compile(source) -> Container` / `spawn_units(units) -> ProgramHandle`
//! API intended for downstream consumption by the server crate.

mod errors;
mod fbd_transpile;
mod ld_transpile;
pub mod monitor;
mod problem_docs;
mod retain;
mod runtime;
mod sfc_transpile;

pub use problem_docs::{lookup_problem_doc, lookup_problem_explanation};

pub use fbd_transpile::{
    transpile_to_st_with_map as transpile_fbd_to_st_with_map, FbdLocation, FbdSourceMap,
};
pub use ld_transpile::{
    transpile_to_st_with_map as transpile_ld_to_st_with_map, LdLocation, LdSourceMap,
};
pub use sfc_transpile::{
    transpile_to_st_with_map as transpile_sfc_to_st_with_map, SfcLocation, SfcSourceMap,
};

pub use errors::BridgeError;
pub use runtime::{
    spawn_units, DeviceHealth, DeviceReport, DeviceSpec, DiscoveredSlave, ProgramHandle,
    ProgramUnit, RuntimeMode, RuntimeWriteError, VarSnapshot, VarValue, DEFAULT_SCAN_INTERVAL_MS,
    RETAIN_FLUSH_INTERVAL, WATCHDOG_OVERRUN_THRESHOLD,
};

// Re-exported so downstream crates (server / runtime) can name the
// bytecode type when constructing `ProgramUnit`s without depending on
// the vendored ironplc-container crate directly.
pub use ironplc_container::Container;

use ironplc_dsl::common::{
    DeclarationQualifier, InitialValueAssignmentKind, Library, LibraryElementKind, VarDecl,
    VariableType,
};
use ironplc_dsl::core::FileId;
use ironplc_dsl::diagnostic::{Diagnostic, LineColumn};
use ironplc_parser::options::CompilerOptions;
use serde::Serialize;
use ts_rs::TS;

/// Per-program metadata derived from the source's AST that the runtime
/// needs but the bytecode `Container` itself doesn't preserve.
///
/// Right now this is just the set of `VAR RETAIN`-qualified variable
/// names — `ironplc`'s codegen flattens qualifiers away once it lowers
/// to bytecode (see `vendor/ironplc/compiler/container/src/debug_format.rs`),
/// so we re-derive them from the parsed `Library` for the persistence
/// layer to consume.
#[derive(Debug, Clone, Default)]
pub struct ProgramMetadata {
    /// IEC variable names declared with `RETAIN` (or with `retain="true"`
    /// in PLCopen XML). Lower-cased to match `VarDebugInfo.name` which
    /// is what the runtime looks up against. Stable across compilations
    /// of the same source.
    pub retain_vars: Vec<String>,
}

/// Compile an IEC 61131-3 Structured Text source string into an executable
/// ironplc bytecode `Container`. Uses dialect Ed2 with no vendor extensions.
///
/// Thin convenience wrapper around `compile_with_metadata` that drops the
/// metadata; for code paths that *do* need retain info (the run path),
/// call `compile_with_metadata` directly.
pub fn compile(source: &str) -> Result<Container, BridgeError> {
    compile_with_metadata(source).map(|(c, _)| c)
}

/// Like `compile` but also returns the `ProgramMetadata` extracted from
/// the parsed AST (retain vars and anything else the codegen drops).
pub fn compile_with_metadata(source: &str) -> Result<(Container, ProgramMetadata), BridgeError> {
    let file_id = FileId::default();
    let options = parser_options();

    let library = ironplc_parser::parse_program(source, &file_id, &options)
        .map_err(|d| BridgeError::Parse(format!("{d:?}")))?;

    // Capture retain vars BEFORE we hand the library to the analyzer,
    // which moves/consumes parts of it. The walk is read-only and cheap.
    let metadata = ProgramMetadata {
        retain_vars: extract_retain_vars(&library),
    };

    let container = compile_library(&library)?;
    Ok((container, metadata))
}

/// The one `CompilerOptions` shape the bridge uses everywhere.
/// `allow_empty_var_blocks` mirrors the ironplc CLI flag — POU templates
/// we ship intentionally start with empty VAR blocks.
fn parser_options() -> CompilerOptions {
    CompilerOptions {
        allow_empty_var_blocks: true,
        ..Default::default()
    }
}

/// Analyze + codegen a parsed `Library` into a bytecode container —
/// the shared tail of `compile_with_metadata` and
/// `compile_project_units`.
fn compile_library(library: &Library) -> Result<Container, BridgeError> {
    let options = parser_options();
    let (analyzed, context) = ironplc_analyzer::stages::analyze(&[library], &options)
        .map_err(|ds| BridgeError::Analyze(format!("{ds:?}")))?;

    if context.has_diagnostics() {
        return Err(BridgeError::Analyze(format!("{:?}", context.diagnostics())));
    }

    let codegen_options = ironplc_codegen::CodegenOptions::default();
    ironplc_codegen::compile(&analyzed, &context, &codegen_options)
        .map_err(|d| BridgeError::Codegen(format!("{d:?}")))
}

/// Walk a parsed `Library` and collect every variable whose declaration
/// is qualified `RETAIN`. Covers:
///   - `PROGRAM` `VAR RETAIN` blocks
///   - `FUNCTION_BLOCK` `VAR RETAIN` blocks (FB instance state)
///   - Global `VAR_GLOBAL RETAIN` declarations
///
/// We don't currently descend into nested function-block instances
/// (their retain qualifier is on the FB's own declaration, which we
/// already capture). Variable names are lower-cased so the runtime
/// can match directly against `VarDebugInfo.name` (which ironplc's
/// debug section also stores lower-cased).
fn extract_retain_vars(library: &Library) -> Vec<String> {
    let mut out = Vec::new();
    for element in &library.elements {
        match element {
            LibraryElementKind::ProgramDeclaration(p) => {
                for v in &p.variables {
                    if v.qualifier == DeclarationQualifier::Retain {
                        if let Some(id) = v.identifier.symbolic_id() {
                            out.push(id.lower_case().clone());
                        }
                    }
                }
            }
            LibraryElementKind::FunctionBlockDeclaration(fb) => {
                for v in &fb.variables {
                    if v.qualifier == DeclarationQualifier::Retain {
                        if let Some(id) = v.identifier.symbolic_id() {
                            out.push(id.lower_case().clone());
                        }
                    }
                }
            }
            LibraryElementKind::GlobalVarDeclarations(vars) => {
                for v in vars {
                    if v.qualifier == DeclarationQualifier::Retain {
                        if let Some(id) = v.identifier.symbolic_id() {
                            out.push(id.lower_case().clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Compile a whole project against the schedule stored in `tasks.toml`.
/// Thin wrapper over `compile_project_units` that reads the schedule
/// from disk — the natural entry point for "does this project compile"
/// checks. Every scheduled instance gets its own hoisted unit, so codegen
/// covers each of them (a concatenated compile would silently codegen
/// only the first PROGRAM in file order).
pub fn compile_project(store: &project::ProjectStore) -> Result<Vec<ProgramUnit>, BridgeError> {
    let tasks = store
        .read_tasks()
        .map_err(|e| BridgeError::Parse(format!("reading tasks.toml: {e}")))?
        .unwrap_or_default();
    compile_project_units(store, &tasks)
}

/// Compile one container per `tasks.toml` PROGRAM instance — the
/// multi-PROGRAM execution model from ADR-0001 ("one Container + one VM
/// per PROGRAM instance, round-robin scheduled on a single scan thread").
///
/// Each unit's library is assembled at the AST level:
///   1. the instance's own `ProgramDeclaration`, hoisted to the front —
///      ironplc's codegen compiles the FIRST PROGRAM it finds, so this
///      is what makes "which program runs" deterministic even when one
///      `.st` file declares several PROGRAMs;
///   2. every non-PROGRAM declaration from every POU file
///      (FUNCTION_BLOCKs, FUNCTIONs, data types, top-level globals) so
///      cross-file FB references resolve exactly like the whole-project
///      compile;
///   3. a synthesized single-task CONFIGURATION for just this instance.
///
/// Other PROGRAM declarations are excluded from each unit: programs are
/// only instantiable from the CONFIGURATION, and leaving them in would
/// bleed their variables into this unit's debug section (and could even
/// shadow the intended entry program).
///
/// Per-unit containers mean top-level VAR_GLOBALs are NOT shared across
/// instances — each unit gets a private copy. Run paths reject
/// multi-PROGRAM projects that declare globals (see
/// `extract_project_global_vars`); single-PROGRAM projects keep their
/// historical globals behaviour.
pub fn compile_project_units(
    store: &project::ProjectStore,
    tasks: &project::Tasks,
) -> Result<Vec<ProgramUnit>, BridgeError> {
    if tasks.programs.is_empty() {
        return Err(BridgeError::Parse(
            "no PROGRAM instances to run — bind a PROGRAM to a task in the \
             Tasks pane, or click Run from a POU file's editor (which schedules \
             that PROGRAM ad-hoc for one run)"
                .into(),
        ));
    }
    let pou_paths = store
        .list_pou_paths()
        .map_err(|e| BridgeError::Parse(format!("listing pous: {e}")))?;
    let options = parser_options();

    // Parse every POU file once; elements are cloned per unit below.
    // Each file keeps its own FileId so analyzer diagnostics name the
    // real source file instead of an offset into a concatenated blob.
    let mut program_decls: Vec<(String, ironplc_dsl::common::ProgramDeclaration)> = Vec::new();
    let mut shared_elements: Vec<LibraryElementKind> = Vec::new();
    for path in &pou_paths {
        let language = store
            .pou_file_language(path)
            .map_err(|e| BridgeError::Parse(format!("language for '{path}': {e}")))?;
        let source = store
            .read_pou_source(path)
            .map_err(|e| BridgeError::Parse(format!("reading pou '{path}': {e}")))?;
        let st = source_to_st(&source, language)?;
        let cleaned = strip_any_configuration(&st);
        let file_id = FileId::from_string(path);
        let library = ironplc_parser::parse_program(&cleaned, &file_id, &options)
            .map_err(|d| BridgeError::Parse(format!("parsing '{path}': {d:?}")))?;
        for element in library.elements {
            match element {
                LibraryElementKind::ProgramDeclaration(p) => {
                    program_decls.push((p.name.to_string().to_lowercase(), p));
                }
                other => shared_elements.push(other),
            }
        }
    }

    let mut units = Vec::with_capacity(tasks.programs.len());
    for p in &tasks.programs {
        let wanted = sanitise_ident(&p.program).to_lowercase();
        let program_decl = program_decls
            .iter()
            .find(|(name, _)| *name == wanted)
            .map(|(_, decl)| decl.clone())
            .ok_or_else(|| {
                BridgeError::Parse(format!(
                    "PROGRAM '{}' (instance '{}') is scheduled in tasks.toml but not \
                     declared in any POU file",
                    p.program, p.instance
                ))
            })?;

        // Resolve the bound task; a dangling task name degrades to the
        // default cadence with a warning rather than refusing to start
        // (matches the run paths' historical graceful-degrade).
        let task = tasks
            .tasks
            .iter()
            .find(|t| t.name == p.task)
            .cloned()
            .unwrap_or_else(|| {
                tracing::warn!(
                    instance = %p.instance,
                    task = %p.task,
                    "instance references a task not declared in tasks.toml; \
                     defaulting to {DEFAULT_SCAN_INTERVAL_MS} ms / priority 1"
                );
                project::Task {
                    name: p.task.clone(),
                    interval_ms: DEFAULT_SCAN_INTERVAL_MS as u32,
                    priority: 1,
                }
            });

        // Single-instance CONFIGURATION, via the same synthesizer as
        // the whole-project path, then parsed back into AST elements.
        let single = project::Tasks {
            tasks: vec![task.clone()],
            programs: vec![p.clone()],
        };
        let config_text = synthesize_configuration(&single);
        let config_lib = ironplc_parser::parse_program(&config_text, &FileId::default(), &options)
            .map_err(|d| {
                BridgeError::Parse(format!(
                    "synthesized CONFIGURATION for instance '{}': {d:?}",
                    p.instance
                ))
            })?;

        let mut elements =
            Vec::with_capacity(1 + shared_elements.len() + config_lib.elements.len());
        elements.push(LibraryElementKind::ProgramDeclaration(program_decl));
        elements.extend(shared_elements.iter().cloned());
        elements.extend(config_lib.elements);
        let unit_library = Library { elements };

        let retain_vars = extract_retain_vars(&unit_library);
        let container = compile_library(&unit_library).map_err(|e| {
            let tag = |m: String| format!("instance '{}': {m}", p.instance);
            match e {
                BridgeError::Parse(m) => BridgeError::Parse(tag(m)),
                BridgeError::Analyze(m) => BridgeError::Analyze(tag(m)),
                BridgeError::Codegen(m) => BridgeError::Codegen(tag(m)),
            }
        })?;

        units.push(ProgramUnit {
            instance: sanitise_ident(&p.instance).to_string(),
            task_name: p.task.clone(),
            interval_ms: u64::from(task.interval_ms),
            priority: task.priority,
            container,
            retain_vars,
        });
    }
    Ok(units)
}

/// Scan every POU file for top-level `VAR_GLOBAL` declarations and
/// return (file_path, variable_name) pairs. Multi-PROGRAM run paths use
/// this to reject projects that rely on cross-program globals: with one
/// container per instance (ADR-0001) each PROGRAM gets a private copy
/// of every global, so shared state would silently diverge.
///
/// Tolerant by design — unreadable / unparseable files contribute
/// nothing here; the compile path is where their real diagnostics
/// surface. VAR_GLOBAL inside a stray CONFIGURATION block is ignored
/// for the same reason the compile path ignores it: those blocks are
/// stripped before compilation.
/// ADR-0001 gate, in its ONE authoritative form: multi-PROGRAM projects
/// run one container per PROGRAM instance, which isolates the address
/// spaces — a `VAR_GLOBAL` would be duplicated into every instance
/// instead of shared, and the copies would silently diverge. Refuses
/// (with the fix spelled out) when `tasks` schedules 2+ PROGRAMs and the
/// project declares globals; single-PROGRAM projects keep their
/// historical globals behaviour.
///
/// The server's run and validate paths and the headless edge runtime all
/// call this instead of restating the rule, so the invariant and its
/// message cannot drift between surfaces.
pub fn reject_shared_globals(
    store: &project::ProjectStore,
    tasks: &project::Tasks,
) -> Result<(), String> {
    if tasks.programs.len() <= 1 {
        return Ok(());
    }
    let globals = extract_project_global_vars(store);
    if globals.is_empty() {
        return Ok(());
    }
    let list = globals
        .iter()
        .map(|(file, var)| format!("'{var}' in {file}"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "tasks.toml schedules {} PROGRAMs but the project declares VAR_GLOBAL \
         ({list}). Each PROGRAM instance runs in its own isolated container, so \
         globals are NOT shared across PROGRAMs — every instance would get a \
         private copy that silently diverges. Move the shared state behind I/O \
         mappings or FUNCTION_BLOCK parameters, or schedule a single PROGRAM.",
        tasks.programs.len(),
    ))
}

pub fn extract_project_global_vars(store: &project::ProjectStore) -> Vec<(String, String)> {
    let Ok(paths) = store.list_pou_paths() else {
        return Vec::new();
    };
    let options = parser_options();
    let mut out = Vec::new();
    for path in &paths {
        let Ok(language) = store.pou_file_language(path) else {
            continue;
        };
        let Ok(source) = store.read_pou_source(path) else {
            continue;
        };
        let Ok(st) = source_to_st(&source, language) else {
            continue;
        };
        let cleaned = strip_any_configuration(&st);
        let file_id = FileId::from_string(path);
        let Ok(library) = ironplc_parser::parse_program(&cleaned, &file_id, &options) else {
            continue;
        };
        for element in &library.elements {
            if let LibraryElementKind::GlobalVarDeclarations(decls) = element {
                for v in decls {
                    if let Some(id) = v.identifier.symbolic_id() {
                        out.push((path.clone(), id.to_string()));
                    }
                }
            }
        }
    }
    out
}

/// Compile one POU file for an ad-hoc isolated run, returning the
/// AST-derived `ProgramMetadata` alongside the bytecode. Used by the
/// ProgramPane's Run button so opening cascade_pid.st and clicking Run
/// actually runs cascade_pid in isolation — ironplc's debug section only
/// names PROGRAM variables, and no sibling PROGRAM is included, so the
/// Monitor shows exactly the target file's variables (unlike
/// `compile_project`, which concatenates every PROGRAM).
///
/// "Isolated" scopes the *debug surface*, not the type system: every
/// sibling file that declares no PROGRAM (imported library blocks,
/// helper FUNCTION_BLOCKs / FUNCTIONs) is appended as type context, so
/// an FBD program wired to FB_PID from the library runs exactly like it
/// compiles project-wide. Sibling files that fail to read or parse are
/// skipped — their absence resurfaces honestly as P2008 on the
/// reference. Sibling files that mix a PROGRAM with FBs stay excluded
/// (the PROGRAM would pollute the debug section); keep shared blocks in
/// their own files.
pub fn compile_isolated_in_project_full(
    store: &project::ProjectStore,
    pou_path: &str,
    tasks: &project::Tasks,
) -> Result<(Container, ProgramMetadata), BridgeError> {
    if tasks.programs.is_empty() {
        return Err(BridgeError::Parse("no PROGRAM instance to run".into()));
    }
    let language = store
        .pou_file_language(pou_path)
        .map_err(|e| BridgeError::Parse(format!("language for '{pou_path}': {e}")))?;
    let source = store
        .read_pou_source(pou_path)
        .map_err(|e| BridgeError::Parse(format!("reading pou '{pou_path}': {e}")))?;
    let st = source_to_st(&source, language)?;
    let mut combined = String::with_capacity(st.len() + 1024);
    combined.push_str(&strip_any_configuration(&st));
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    if let Ok(paths) = store.list_pou_paths() {
        for path in &paths {
            if path == pou_path {
                continue;
            }
            let Ok(lang) = store.pou_file_language(path) else {
                continue;
            };
            let Ok(raw) = store.read_pou_source(path) else {
                continue;
            };
            let decls = extract_pou_declarations(&raw, lang);
            if decls.is_empty() || decls.iter().any(|d| d.type_ == project::PouType::Program) {
                continue;
            }
            let Ok(sibling_st) = source_to_st(&raw, lang) else {
                continue;
            };
            combined.push_str(&strip_any_configuration(&sibling_st));
            if !combined.ends_with('\n') {
                combined.push('\n');
            }
        }
    }
    combined.push_str(&synthesize_configuration(tasks));
    compile_with_metadata(&combined)
}

/// Lower an arbitrary POU source into ST, ready for ironplc to parse.
/// `St` is the identity. `Ld` parses the JSON via the LD schema and
/// runs the transpiler. Returns a descriptive parse error if the input
/// shape doesn't match the declared language.
fn source_to_st(source: &str, language: project::PouLanguage) -> Result<String, BridgeError> {
    match language {
        project::PouLanguage::St => Ok(source.to_string()),
        project::PouLanguage::Ld => {
            let prog: project::LdProgram = serde_json::from_str(source)
                .map_err(|e| BridgeError::Parse(format!("LD JSON parse: {e}")))?;
            ld_transpile::transpile_to_st(&prog)
        }
        project::PouLanguage::Fbd => {
            let prog: project::FbdProgram = serde_json::from_str(source)
                .map_err(|e| BridgeError::Parse(format!("FBD JSON parse: {e}")))?;
            fbd_transpile::transpile_to_st(&prog)
        }
        project::PouLanguage::Sfc => {
            let prog: project::SfcProgram = serde_json::from_str(source)
                .map_err(|e| BridgeError::Parse(format!("SFC JSON parse: {e}")))?;
            sfc_transpile::transpile_to_st(&prog)
        }
    }
}

/// Build a single CONFIGURATION block from the project's task / program
/// bindings. Currently emits one fixed RESOURCE — multi-RESOURCE projects
/// aren't supported yet (the IEC standard allows them; not a frequent need
/// for single-edge deployments).
fn synthesize_configuration(tasks: &project::Tasks) -> String {
    let mut s = String::new();
    s.push_str("CONFIGURATION config\n");
    s.push_str("    RESOURCE plc_res ON PLC\n");
    for t in &tasks.tasks {
        s.push_str(&format!(
            "        TASK {}(INTERVAL := T#{}ms, PRIORITY := {});\n",
            sanitise_ident(&t.name),
            t.interval_ms.max(1),
            t.priority,
        ));
    }
    for p in &tasks.programs {
        s.push_str(&format!(
            "        PROGRAM {} WITH {} : {};\n",
            sanitise_ident(&p.instance),
            sanitise_ident(&p.task),
            sanitise_ident(&p.program),
        ));
    }
    s.push_str("    END_RESOURCE\n");
    s.push_str("END_CONFIGURATION\n");
    s
}

/// IEC identifiers are letters/digits/underscore. The project tree allows
/// `/` in POU names (folder paths); use just the leaf when referring to
/// program / task / instance names in the generated IEC source.
fn sanitise_ident(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// Find every `CONFIGURATION … END_CONFIGURATION` block in `source` and
/// remove it. Tolerant of case + whitespace; used to defend against POU
/// files that still carry a pre-migration scheduling block.
fn strip_any_configuration(source: &str) -> String {
    let lower = source.to_ascii_lowercase();
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    while let Some(start_rel) = lower[cursor..].find("configuration ") {
        let abs = cursor + start_rel;
        out.push_str(&source[cursor..abs]);
        let Some(end_rel) = lower[abs..].find("end_configuration") else {
            out.push_str(&source[abs..]);
            cursor = source.len();
            break;
        };
        let abs_end = abs + end_rel + "end_configuration".len();
        cursor = abs_end;
        if source.as_bytes().get(cursor).copied() == Some(b'\n') {
            cursor += 1;
        }
    }
    out.push_str(&source[cursor..]);
    out
}

/// One diagnostic in source-positioned form (1-indexed line/column for the
/// Monaco editor). Severity is "error" for now — ironplc emits a single
/// diagnostic stream without warning/info distinction at this layer.
///
/// For graphical-language POUs, `ld_location` / `fbd_location` / `sfc_location`
/// carry the resolved element the diagnostic originated from. Exactly one of
/// them is populated (or neither, for ST POUs / boilerplate lines).
///
/// **The "thick" diagnostic fields** — `context`, `related`, `explanation` —
/// come from `ironplc::Diagnostic`'s richer model. ironplc attaches
/// "describing" strings (`variable=foo`), secondary labels (`did you mean:
/// bar?`), and ships an RST doc per problem code. We expose all three so
/// human users and agents both see what to do next, not just "syntax error".
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct CheckDiagnostic {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    /// Extra context fragments (e.g. `"variable=ghost"`). Comes from
    /// ironplc's `Diagnostic.described`. Empty for diagnostics without
    /// context (most syntax errors).
    #[serde(default)]
    pub context: Vec<String>,
    /// Secondary labels pointing at related source locations — the
    /// canonical "did you mean: counter?" / "first declared here"
    /// pattern. Empty for diagnostics without related info.
    #[serde(default)]
    pub related: Vec<DiagnosticRelated>,
    /// Embedded explanation from ironplc's problem-code documentation
    /// (RST body, title stripped). `None` if the code isn't in
    /// ironplc's registry — true for our synthetic
    /// LD-PARSE / FBD-TRANSPILE etc. codes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    /// For LD POUs only: which LD element this diagnostic originated
    /// from. `None` for ST / FBD / SFC POUs and for diagnostics on
    /// boilerplate lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ld_location: Option<ld_transpile::LdLocation>,
    /// For FBD POUs only: which block / output binding / variable
    /// declaration this diagnostic originated from. `None` for other
    /// languages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fbd_location: Option<fbd_transpile::FbdLocation>,
    /// For SFC POUs only: which step / action / transition / variable
    /// the diagnostic originated from. `None` for other languages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sfc_location: Option<sfc_transpile::SfcLocation>,
}

/// One secondary label on a diagnostic. Both Monaco's
/// `relatedInformation` and our graphical-editor banners render these
/// as clickable jumps from the primary diagnostic to the related
/// source location — that's what makes "did you mean: foo?" useful
/// instead of just informative.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct DiagnosticRelated {
    pub message: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    /// Same dispatch as the parent diagnostic. The related label
    /// usually lives on the SAME source as the primary, but we still
    /// run it through the source map so jumps from the editor land in
    /// the right graphical element.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ld_location: Option<ld_transpile::LdLocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fbd_location: Option<fbd_transpile::FbdLocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sfc_location: Option<sfc_transpile::SfcLocation>,
}

/// Parse + analyse a Structured Text source and return all diagnostics
/// (syntax errors, type errors, undeclared identifiers, etc.). Does NOT run
/// codegen — this is the fast path for editor squiggles.
pub fn check(source: &str) -> Vec<CheckDiagnostic> {
    check_inner(source, SourceMapKind::None, &[])
}

/// Like `check`, but for a graphical-language POU: parses the source,
/// transpiles to ST, runs the ironplc pipeline, then maps each
/// diagnostic's line back to the originating LD / FBD element.
/// ST-language POUs should still go through `check` — this entry
/// dispatches on the supplied `language`.
///
/// Returns a single synthetic diagnostic if the JSON itself doesn't
/// parse (so the editor still gets a squiggle pointing at the broken
/// document rather than silent failure).
pub fn check_pou_source(source: &str, language: project::PouLanguage) -> Vec<CheckDiagnostic> {
    check_pou_source_with_context(source, language, &[])
}

/// Like `check_pou_source`, but analyses the buffer together with every
/// other POU in the project, so references to FUNCTION_BLOCKs / FUNCTIONs
/// declared in sibling files resolve instead of false-positiving
/// (P2008 "cannot determine kind of type" + the P4012 cascade). This is
/// what makes a library FB usable from the editor: per-file `cs check`
/// can't cross-resolve FBs — only a project-wide view can.
///
/// Diagnostics are filtered to the checked buffer; context files never
/// contribute squiggles (the Run/Deploy project compile is the place
/// that surfaces *their* problems). Context files that fail to read or
/// parse are skipped — a broken sibling then simply leaves its FBs
/// undeclared, which honestly re-surfaces as P2008 here.
///
/// `buffer_path` is the store slug the buffer was loaded from, used to
/// keep its on-disk copy from double-declaring the same POUs. When the
/// caller can't name it (older clients), context files declaring any of
/// the buffer's own POU names are skipped instead.
pub fn check_pou_in_project(
    store: &project::ProjectStore,
    source: &str,
    language: project::PouLanguage,
    buffer_path: Option<&str>,
) -> Vec<CheckDiagnostic> {
    let context = project_context_libraries(store, source, language, buffer_path);
    check_pou_source_with_context(source, language, &context)
}

/// Check a set of loose files TOGETHER: each file is analysed with every
/// other file as declaration context, so FUNCTION_BLOCKs / FUNCTIONs
/// declared in sibling files resolve exactly as they would in a project
/// compile. This is `cs check a.st b.st …`'s group mode — the offline
/// counterpart of `check_pou_in_project` for directories that aren't
/// projects (the FB library, a folder of loose sources). Diagnostics
/// come back per input file, in input order; context files contribute
/// declarations only, never squiggles.
///
/// Inputs are `(display_id, source, language)`; the display id becomes
/// the diagnostic `FileId` so tooling can attribute results.
pub fn check_sources_together(
    files: &[(String, String, project::PouLanguage)],
) -> Vec<Vec<CheckDiagnostic>> {
    let options = CompilerOptions {
        allow_empty_var_blocks: true,
        ..Default::default()
    };
    // Parse every file once up front; a file that doesn't parse simply
    // contributes no declarations (its own slot still gets checked and
    // reports the parse error there).
    let libs: Vec<Option<Library>> = files
        .iter()
        .map(|(id, raw, language)| {
            let st = source_to_st(raw, *language).ok()?;
            let cleaned = strip_any_configuration(&st);
            let file_id = FileId::from_string(id);
            ironplc_parser::parse_program(&cleaned, &file_id, &options).ok()
        })
        .collect();
    files
        .iter()
        .enumerate()
        .map(|(i, (_, raw, language))| {
            let context: Vec<Library> = libs
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .filter_map(|(_, l)| l.clone())
                .collect();
            check_pou_source_with_context(raw, *language, &context)
        })
        .collect()
}

fn check_pou_source_with_context(
    source: &str,
    language: project::PouLanguage,
    context: &[Library],
) -> Vec<CheckDiagnostic> {
    match language {
        project::PouLanguage::St => check_inner(source, SourceMapKind::None, context),
        project::PouLanguage::Ld => {
            let prog: project::LdProgram = match serde_json::from_str(source) {
                Ok(p) => p,
                Err(e) => return vec![synthetic_parse_diag("LD-PARSE", "LD", &e)],
            };
            let (st, map) = match ld_transpile::transpile_to_st_with_map(&prog) {
                Ok(pair) => pair,
                Err(e) => return vec![synthetic_transpile_diag("LD-TRANSPILE", &e)],
            };
            check_inner(&st, SourceMapKind::Ld(&map), context)
        }
        project::PouLanguage::Fbd => {
            let prog: project::FbdProgram = match serde_json::from_str(source) {
                Ok(p) => p,
                Err(e) => return vec![synthetic_parse_diag("FBD-PARSE", "FBD", &e)],
            };
            let (st, map) = match fbd_transpile::transpile_to_st_with_map(&prog) {
                Ok(pair) => pair,
                Err(e) => return vec![synthetic_transpile_diag("FBD-TRANSPILE", &e)],
            };
            check_inner(&st, SourceMapKind::Fbd(&map), context)
        }
        project::PouLanguage::Sfc => {
            let prog: project::SfcProgram = match serde_json::from_str(source) {
                Ok(p) => p,
                Err(e) => return vec![synthetic_parse_diag("SFC-PARSE", "SFC", &e)],
            };
            let (st, map) = match sfc_transpile::transpile_to_st_with_map(&prog) {
                Ok(pair) => pair,
                Err(e) => return vec![synthetic_transpile_diag("SFC-TRANSPILE", &e)],
            };
            check_inner(&st, SourceMapKind::Sfc(&map), context)
        }
    }
}

/// Parse every project POU except the buffer's own file into `Library`
/// values carrying their store slug as `FileId` — the analyzer keeps
/// that identity on each diagnostic, which is what lets `check_inner`
/// filter results back to just the buffer with no line-offset games.
fn project_context_libraries(
    store: &project::ProjectStore,
    buffer_source: &str,
    buffer_language: project::PouLanguage,
    buffer_path: Option<&str>,
) -> Vec<Library> {
    let Ok(paths) = store.list_pou_paths() else {
        return Vec::new();
    };
    // Identity fallback: with no slug to exclude, skip any context file
    // that re-declares one of the buffer's own POU names (IEC names are
    // case-insensitive).
    let buffer_names: Vec<String> = if buffer_path.is_none() {
        extract_pou_declarations(buffer_source, buffer_language)
            .into_iter()
            .map(|d| d.name.to_lowercase())
            .collect()
    } else {
        Vec::new()
    };
    let options = CompilerOptions {
        allow_empty_var_blocks: true,
        ..Default::default()
    };
    let mut out = Vec::new();
    for path in &paths {
        if buffer_path == Some(path.as_str()) {
            continue;
        }
        let Ok(language) = store.pou_file_language(path) else {
            continue;
        };
        let Ok(raw) = store.read_pou_source(path) else {
            continue;
        };
        let Ok(st) = source_to_st(&raw, language) else {
            continue;
        };
        let cleaned = strip_any_configuration(&st);
        let file_id = FileId::from_string(path);
        let Ok(lib) = ironplc_parser::parse_program(&cleaned, &file_id, &options) else {
            continue;
        };
        if !buffer_names.is_empty() && library_declares_any(&lib, &buffer_names) {
            continue;
        }
        out.push(lib);
    }
    out
}

fn library_declares_any(lib: &Library, lower_names: &[String]) -> bool {
    lib.elements.iter().any(|e| {
        let name = match e {
            LibraryElementKind::ProgramDeclaration(p) => p.name.to_string(),
            LibraryElementKind::FunctionBlockDeclaration(fb) => fb.name.to_string(),
            LibraryElementKind::FunctionDeclaration(f) => f.name.to_string(),
            _ => return false,
        };
        lower_names.contains(&name.to_lowercase())
    })
}

/// One of: no map, or a map for each graphical language. Used by
/// `check_inner` to route diagnostics through the right reverse-mapping
/// helper without growing the public API.
enum SourceMapKind<'a> {
    None,
    Ld(&'a ld_transpile::LdSourceMap),
    Fbd(&'a fbd_transpile::FbdSourceMap),
    Sfc(&'a sfc_transpile::SfcSourceMap),
}

fn synthetic_parse_diag(code: &str, language_name: &str, e: &serde_json::Error) -> CheckDiagnostic {
    CheckDiagnostic {
        severity: "error".into(),
        code: code.into(),
        message: format!("Invalid {language_name} JSON: {e}"),
        start_line: e.line() as u32,
        start_column: e.column() as u32,
        end_line: e.line() as u32,
        end_column: (e.column() as u32).saturating_add(1),
        context: vec![],
        related: vec![],
        explanation: None,
        ld_location: None,
        fbd_location: None,
        sfc_location: None,
    }
}

fn synthetic_transpile_diag(code: &str, e: &BridgeError) -> CheckDiagnostic {
    CheckDiagnostic {
        severity: "error".into(),
        code: code.into(),
        message: format!("{e:?}"),
        start_line: 1,
        start_column: 1,
        end_line: 1,
        end_column: 1,
        context: vec![],
        related: vec![],
        explanation: None,
        ld_location: None,
        fbd_location: None,
        sfc_location: None,
    }
}

/// Shared implementation of `check` / `check_pou_source`. If `map` is
/// present, every produced diagnostic is annotated with the LD / FBD
/// element whose generated ST line was reported. `context` carries the
/// other project files' parsed libraries; when non-empty, diagnostics
/// are filtered to the checked buffer (its `FileId::default()` identity)
/// so a sibling file's problems don't squiggle this one.
fn check_inner(source: &str, map: SourceMapKind<'_>, context: &[Library]) -> Vec<CheckDiagnostic> {
    let file_id = FileId::default();
    // `allow_empty_var_blocks` mirrors the ironplc CLI flag. POU templates
    // we ship intentionally start with empty VAR / VAR_INPUT / VAR_OUTPUT
    // blocks — those should compile, not error.
    let options = CompilerOptions {
        allow_empty_var_blocks: true,
        ..Default::default()
    };

    let library = match ironplc_parser::parse_program(source, &file_id, &options) {
        Ok(l) => l,
        Err(d) => return vec![diag_to_dto_with_map(&d, source, &map, &file_id)],
    };

    let mut sources: Vec<&Library> = Vec::with_capacity(1 + context.len());
    sources.push(&library);
    sources.extend(context.iter());

    // With no context every diagnostic necessarily belongs to the buffer;
    // keep that path unfiltered so `check` behaves exactly as before.
    let keep = |d: &Diagnostic| context.is_empty() || d.primary.file_id == file_id;

    let (_, semantic) = match ironplc_analyzer::stages::analyze(&sources, &options) {
        Ok(t) => t,
        Err(ds) => {
            return ds
                .iter()
                .filter(|d| keep(d))
                .map(|d| diag_to_dto_with_map(d, source, &map, &file_id))
                .collect()
        }
    };

    semantic
        .diagnostics()
        .iter()
        .filter(|d| keep(d))
        .map(|d| diag_to_dto_with_map(d, source, &map, &file_id))
        .collect()
}

/// A single declared variable in a POU, surfaced to the frontend so it can
/// offer auto-complete in the IO Mapping form.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct VariableInfo {
    pub name: String,
    pub type_name: String,
    /// "local" | "input" | "output" | "in_out" | "external" | "global" | "access" | "temp"
    pub direction: String,
}

/// Parse an ST source and return every POU declaration found in it,
/// using the canonical `project::PouDecl` shape (name + type + language).
/// Returns an empty list if parsing fails — partial state isn't useful
/// to surface here; the LSP diagnostics endpoint is the right path for
/// "what's wrong with this source".
///
/// Dispatches on `language`:
///
/// - `St`  — runs the ironplc parser and emits one `PouDecl` per
///   top-level PROGRAM / FUNCTION_BLOCK / FUNCTION.
/// - `Ld`  — parses the `.ld.json` source and emits a single `PouDecl`
///   built from the `LdProgram`'s name + pou_type. LD files are
///   single-declaration by design (see `crates/project/src/ld.rs`).
/// - `Fbd` — same idea as LD but for `.fbd.json`.
/// - `Sfc` — same idea as LD but for `.sfc.json`.
/// - others — returns empty (no parser yet). The new-POU dialog
///   prevents these from existing on disk, but be defensive.
pub fn extract_pou_declarations(
    source: &str,
    language: project::PouLanguage,
) -> Vec<project::PouDecl> {
    use project::{PouDecl, PouLanguage, PouType};
    match language {
        PouLanguage::St => extract_st_declarations(source),
        PouLanguage::Fbd => {
            // Same tolerant-parse pattern as LD: malformed JSON yields
            // an empty decl list rather than blowing up
            // /api/project. Without this branch the IDE never
            // discovers that an `.fbd.json` POU is FBD-language and
            // dispatches the Monaco ST editor — which is wrong.
            match serde_json::from_str::<project::FbdProgram>(source) {
                Ok(prog) => vec![PouDecl {
                    name: prog.name,
                    type_: match prog.pou_type {
                        project::LdPouType::Program => PouType::Program,
                        project::LdPouType::FunctionBlock => PouType::FunctionBlock,
                    },
                    language: PouLanguage::Fbd,
                }],
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse FBD source for declarations");
                    vec![]
                }
            }
        }
        PouLanguage::Sfc => match serde_json::from_str::<project::SfcProgram>(source) {
            Ok(prog) => vec![PouDecl {
                name: prog.name,
                type_: match prog.pou_type {
                    project::LdPouType::Program => PouType::Program,
                    project::LdPouType::FunctionBlock => PouType::FunctionBlock,
                },
                language: PouLanguage::Sfc,
            }],
            Err(e) => {
                tracing::warn!(error = %e, "failed to parse SFC source for declarations");
                vec![]
            }
        },
        PouLanguage::Ld => {
            // Tolerant: if the JSON is malformed (mid-edit save, say)
            // we just return no decls rather than blowing up the
            // entire /api/project response.
            match serde_json::from_str::<project::LdProgram>(source) {
                Ok(prog) => vec![PouDecl {
                    name: prog.name,
                    type_: match prog.pou_type {
                        project::LdPouType::Program => PouType::Program,
                        project::LdPouType::FunctionBlock => PouType::FunctionBlock,
                    },
                    language: PouLanguage::Ld,
                }],
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse LD source for declarations");
                    vec![]
                }
            }
        }
    }
}

fn extract_st_declarations(source: &str) -> Vec<project::PouDecl> {
    use project::{PouDecl, PouLanguage, PouType};
    let file_id = FileId::default();
    let options = CompilerOptions {
        allow_empty_var_blocks: true,
        ..Default::default()
    };
    let Ok(library) = ironplc_parser::parse_program(source, &file_id, &options) else {
        return vec![];
    };
    let mut out = Vec::new();
    for element in &library.elements {
        let (name, type_) = match element {
            LibraryElementKind::ProgramDeclaration(p) => (p.name.to_string(), PouType::Program),
            LibraryElementKind::FunctionBlockDeclaration(fb) => {
                (fb.name.to_string(), PouType::FunctionBlock)
            }
            LibraryElementKind::FunctionDeclaration(f) => (f.name.to_string(), PouType::Function),
            _ => continue,
        };
        out.push(PouDecl {
            name,
            type_,
            language: PouLanguage::St,
        });
    }
    out
}

/// Language-agnostic symbol extraction. Same role as `extract_variables`
/// but works for ST / LD / FBD / SFC and adds FB instances (FBD blocks)
/// as synthetic VAR rows. Drives Monaco hover + variable completion
/// in the editor and the `cs symbols` CLI subcommand.
///
/// Returns `[]` for unsupported languages and for malformed sources —
/// callers shouldn't fall over when the user is mid-edit and the JSON
/// is briefly invalid.
pub fn extract_symbols(source: &str, language: project::PouLanguage) -> Vec<VariableInfo> {
    use project::PouLanguage;
    match language {
        PouLanguage::St => extract_variables(source),
        PouLanguage::Ld => match serde_json::from_str::<project::LdProgram>(source) {
            Ok(prog) => prog
                .variables
                .into_iter()
                .map(|v| VariableInfo {
                    name: v.name,
                    type_name: v.type_name,
                    direction: ld_section_str(v.section).into(),
                })
                .collect(),
            Err(_) => vec![],
        },
        PouLanguage::Fbd => match serde_json::from_str::<project::FbdProgram>(source) {
            Ok(prog) => {
                let mut out: Vec<VariableInfo> = prog
                    .variables
                    .iter()
                    .map(|v| VariableInfo {
                        name: v.name.clone(),
                        type_name: v.type_name.clone(),
                        direction: ld_section_str(v.section).into(),
                    })
                    .collect();
                // FB instances are first-class symbols too — Monaco's
                // hover on `myT1` should reveal `TON`, not pretend it
                // doesn't exist.
                for b in &prog.blocks {
                    out.push(VariableInfo {
                        name: b.instance.clone(),
                        type_name: b.fb_type.clone(),
                        direction: "fb_instance".into(),
                    });
                }
                out
            }
            Err(_) => vec![],
        },
        PouLanguage::Sfc => match serde_json::from_str::<project::SfcProgram>(source) {
            Ok(prog) => prog
                .variables
                .into_iter()
                .map(|v| VariableInfo {
                    name: v.name,
                    type_name: v.type_name,
                    direction: ld_section_str(v.section).into(),
                })
                .collect(),
            Err(_) => vec![],
        },
    }
}

fn ld_section_str(s: project::LdVarSection) -> &'static str {
    match s {
        project::LdVarSection::Input => "input",
        project::LdVarSection::Output => "output",
        project::LdVarSection::Internal => "internal",
    }
}

/// Parse an ST source (no full compile required) and pull out the declared
/// variables of any PROGRAM or FUNCTION_BLOCK at the top level. Returns an
/// empty list if parsing fails — the frontend can fall back to free text.
pub fn extract_variables(source: &str) -> Vec<VariableInfo> {
    let file_id = FileId::default();
    // `allow_empty_var_blocks` mirrors the ironplc CLI flag. POU templates
    // we ship intentionally start with empty VAR / VAR_INPUT / VAR_OUTPUT
    // blocks — those should compile, not error.
    let options = CompilerOptions {
        allow_empty_var_blocks: true,
        ..Default::default()
    };
    let library = match ironplc_parser::parse_program(source, &file_id, &options) {
        Ok(l) => l,
        Err(_) => return vec![],
    };

    let mut out: Vec<VariableInfo> = Vec::new();
    for element in &library.elements {
        let vars = match element {
            LibraryElementKind::ProgramDeclaration(p) => Some(&p.variables),
            LibraryElementKind::FunctionBlockDeclaration(fb) => Some(&fb.variables),
            _ => None,
        };
        if let Some(vars) = vars {
            for v in vars {
                if let Some(info) = var_decl_to_info(v) {
                    out.push(info);
                }
            }
        }
    }
    out
}

fn var_decl_to_info(v: &VarDecl) -> Option<VariableInfo> {
    let name = v.identifier.symbolic_id()?.to_string();
    let type_name = init_type_name(&v.initializer);
    let direction = match v.var_type {
        VariableType::Var => "local",
        VariableType::VarTemp => "temp",
        VariableType::Input => "input",
        VariableType::Output => "output",
        VariableType::InOut => "in_out",
        VariableType::External => "external",
        VariableType::Global => "global",
        VariableType::Access => "access",
    }
    .to_string();
    Some(VariableInfo {
        name,
        type_name,
        direction,
    })
}

fn init_type_name(init: &InitialValueAssignmentKind) -> String {
    use InitialValueAssignmentKind::*;
    match init {
        Simple(s) => s.type_name.to_string(),
        String(s) => s.type_name().to_string(),
        FunctionBlock(fb) => fb.type_name.to_string(),
        LateResolvedType(t) => t.to_string(),
        EnumeratedType(e) => e.type_name.to_string(),
        Structure(s) => s.type_name.to_string(),
        Array(_) => "ARRAY".into(),
        Reference(_) => "REF_TO".into(),
        _ => "?".into(),
    }
}

/// Convert an ironplc diagnostic to our wire DTO, additionally looking
/// up the LD / FBD origin via `map` when present. Diagnostics on lines
/// that don't correspond to a graphical element (boilerplate, synthetic
/// temps) arrive with both `ld_location` and `fbd_location` = None,
/// which the editor renders as a generic file-level error rather than
/// a per-element squiggle.
fn diag_to_dto_with_map(
    d: &Diagnostic,
    source: &str,
    map: &SourceMapKind<'_>,
    buffer_file_id: &FileId,
) -> CheckDiagnostic {
    let start = LineColumn::from_offset(source, d.primary.location.start);
    let end = LineColumn::from_offset(source, d.primary.location.end);
    let start_line = start.line + 1;
    let (ld_location, fbd_location, sfc_location) = lookup_locations(map, start_line as usize);

    // `described` is ironplc's structured context — "variable=foo",
    // "type=BOOL", etc. We pass it through verbatim; the editor /
    // CLI present it as one short line per entry under the primary
    // message.
    let mut context = d.described.clone();

    // Each secondary label resolves to a related-info entry. ironplc
    // emits them with byte offsets, so we run them through the same
    // (offset → line/col → optional graphical location) pipeline as
    // the primary label. With project context in play a label can live
    // in a DIFFERENT file ("first declared here", say) — its offsets
    // mean nothing against this buffer, so it's demoted to a plain
    // context line naming that file instead of a jumpable range.
    let related: Vec<DiagnosticRelated> = d
        .secondary
        .iter()
        .filter_map(|label| {
            if label.file_id != *buffer_file_id {
                let loc = match &label.file_id {
                    FileId::File(p) if !p.is_empty() => format!("see {p}: {}", label.message),
                    _ => label.message.clone(),
                };
                context.push(loc);
                return None;
            }
            let rs = LineColumn::from_offset(source, label.location.start);
            let re = LineColumn::from_offset(source, label.location.end);
            let rline = rs.line + 1;
            let (rl_loc, rf_loc, rs_loc) = lookup_locations(map, rline as usize);
            Some(DiagnosticRelated {
                message: label.message.clone(),
                start_line: rline,
                start_column: rs.column + 1,
                end_line: re.line + 1,
                end_column: re.column + 1,
                ld_location: rl_loc,
                fbd_location: rf_loc,
                sfc_location: rs_loc,
            })
        })
        .collect();

    // Embedded RST doc — only present for ironplc's own problem codes
    // (P0001..P9999). Our synthetic codes (LD-PARSE etc.) won't match.
    let explanation = problem_docs::lookup_problem_explanation(&d.code);

    CheckDiagnostic {
        severity: "error".into(),
        code: d.code.clone(),
        message: d.description(),
        start_line,
        start_column: start.column + 1,
        end_line: end.line + 1,
        end_column: end.column + 1,
        context,
        related,
        explanation,
        ld_location,
        fbd_location,
        sfc_location,
    }
}

/// Resolve a 1-indexed ST line to its (ld, fbd, sfc) graphical
/// origin, using whichever source map is active. Used by both the
/// primary diagnostic and each related label.
fn lookup_locations(
    map: &SourceMapKind<'_>,
    line: usize,
) -> (
    Option<ld_transpile::LdLocation>,
    Option<fbd_transpile::FbdLocation>,
    Option<sfc_transpile::SfcLocation>,
) {
    match map {
        SourceMapKind::None => (None, None, None),
        SourceMapKind::Ld(m) => (m.lookup(line).cloned(), None, None),
        SourceMapKind::Fbd(m) => (None, m.lookup(line).cloned(), None),
        SourceMapKind::Sfc(m) => (None, None, m.lookup(line).cloned()),
    }
}

#[cfg(test)]
mod project_units_tests {
    use super::{
        compile_isolated_in_project_full, compile_project_units, extract_project_global_vars,
    };
    use ironplc_container::debug_format::build_var_debug_map;
    use project::{PouLanguage, PouType, ProgramInstance, ProjectStore, Task, Tasks};

    /// Tempdir-backed project: a cross-file FUNCTION_BLOCK, two
    /// schedulable PROGRAMs in separate files, and one file declaring
    /// TWO PROGRAMs (to prove the entry-program hoisting).
    fn fixture_store(dir: &std::path::Path) -> ProjectStore {
        let store = ProjectStore::create(dir.to_path_buf(), "fixture").expect("create project");
        let write = |path: &str, source: &str| {
            store
                .create_pou_file(path, PouType::Program, PouLanguage::St)
                .expect("create pou");
            store.write_pou_source(path, source).expect("write pou");
        };
        write(
            "fb_util",
            "FUNCTION_BLOCK fb_double\n\
                VAR_INPUT inv : INT; END_VAR\n\
                VAR_OUTPUT outv : INT; END_VAR\n\
                outv := inv * 2;\n\
            END_FUNCTION_BLOCK",
        );
        write(
            "alpha",
            "PROGRAM alpha\n\
                VAR d : fb_double; a_only : INT; END_VAR\n\
                d(inv := 2);\n\
                a_only := d.outv;\n\
            END_PROGRAM",
        );
        write(
            "beta",
            "PROGRAM beta\n\
                VAR b_only : INT; END_VAR\n\
                b_only := b_only + 1;\n\
            END_PROGRAM",
        );
        write(
            "pair",
            "PROGRAM p_first\n\
                VAR first_var : INT; END_VAR\n\
                first_var := 1;\n\
            END_PROGRAM\n\
            PROGRAM p_second\n\
                VAR second_var : INT; END_VAR\n\
                second_var := 2;\n\
            END_PROGRAM",
        );
        store
    }

    fn two_program_tasks() -> Tasks {
        Tasks {
            tasks: vec![
                Task {
                    name: "fast".into(),
                    interval_ms: 10,
                    priority: 1,
                },
                Task {
                    name: "slow".into(),
                    interval_ms: 50,
                    priority: 2,
                },
            ],
            programs: vec![
                ProgramInstance {
                    instance: "a_inst".into(),
                    program: "alpha".into(),
                    task: "fast".into(),
                },
                ProgramInstance {
                    instance: "b_inst".into(),
                    program: "beta".into(),
                    task: "slow".into(),
                },
            ],
        }
    }

    fn debug_names(container: &super::Container) -> Vec<String> {
        build_var_debug_map(container)
            .values()
            .map(|i| i.name.clone())
            .collect()
    }

    #[test]
    fn one_unit_per_instance_with_isolated_containers_and_cross_file_fbs() {
        let dir = tempfile::tempdir().unwrap();
        let store = fixture_store(dir.path());

        let units = compile_project_units(&store, &two_program_tasks()).expect("units compile");
        assert_eq!(units.len(), 2);

        // Scheduling facts come from each instance's bound task.
        assert_eq!(units[0].instance, "a_inst");
        assert_eq!(units[0].interval_ms, 10);
        assert_eq!(units[0].priority, 1);
        assert_eq!(units[1].instance, "b_inst");
        assert_eq!(units[1].interval_ms, 50);
        assert_eq!(units[1].priority, 2);

        // alpha resolves fb_double from a sibling file (cross-file FB)
        // and its container knows its own vars but NOT beta's — other
        // PROGRAM declarations are excluded per unit.
        let a_names = debug_names(&units[0].container);
        assert!(a_names.iter().any(|n| n == "a_only"), "{a_names:?}");
        assert!(!a_names.iter().any(|n| n == "b_only"), "{a_names:?}");

        let b_names = debug_names(&units[1].container);
        assert!(b_names.iter().any(|n| n == "b_only"), "{b_names:?}");
        assert!(!b_names.iter().any(|n| n == "a_only"), "{b_names:?}");
    }

    /// The regression behind the ProgramPane Run button: an isolated run
    /// of one file must resolve FUNCTION_BLOCKs declared in sibling
    /// PROGRAM-less files (imported library blocks), while keeping other
    /// PROGRAMs' variables out of the debug section — "isolated" scopes
    /// the Monitor, not the type system.
    #[test]
    fn isolated_run_resolves_sibling_fbs_without_debug_bleed() {
        let dir = tempfile::tempdir().unwrap();
        let store = fixture_store(dir.path());

        let tasks = Tasks {
            tasks: vec![Task {
                name: "adhoc".into(),
                interval_ms: 10,
                priority: 1,
            }],
            programs: vec![ProgramInstance {
                instance: "alpha_inst".into(),
                program: "alpha".into(),
                task: "adhoc".into(),
            }],
        };
        let (container, _meta) = compile_isolated_in_project_full(&store, "alpha", &tasks)
            .expect("isolated run compiles with sibling FB context");

        let names = debug_names(&container);
        assert!(names.iter().any(|n| n == "a_only"), "{names:?}");
        assert!(
            !names.iter().any(|n| n == "b_only"),
            "sibling PROGRAM vars must not bleed into the isolated debug \
             section: {names:?}"
        );
    }

    /// ironplc's codegen compiles the FIRST ProgramDeclaration it sees.
    /// Scheduling the SECOND program declared in a file must still run
    /// that program — the unit assembly hoists the target declaration
    /// to the front and drops the sibling.
    #[test]
    fn scheduling_the_second_program_in_a_file_compiles_that_program() {
        let dir = tempfile::tempdir().unwrap();
        let store = fixture_store(dir.path());

        let tasks = Tasks {
            tasks: vec![Task {
                name: "t".into(),
                interval_ms: 20,
                priority: 1,
            }],
            programs: vec![ProgramInstance {
                instance: "second_inst".into(),
                program: "p_second".into(),
                task: "t".into(),
            }],
        };
        let units = compile_project_units(&store, &tasks).expect("unit compiles");
        assert_eq!(units.len(), 1);
        let names = debug_names(&units[0].container);
        assert!(names.iter().any(|n| n == "second_var"), "{names:?}");
        assert!(!names.iter().any(|n| n == "first_var"), "{names:?}");
    }

    #[test]
    fn unknown_scheduled_program_errors_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let store = fixture_store(dir.path());

        let tasks = Tasks {
            tasks: vec![Task {
                name: "t".into(),
                interval_ms: 20,
                priority: 1,
            }],
            programs: vec![ProgramInstance {
                instance: "ghost_inst".into(),
                program: "ghost".into(),
                task: "t".into(),
            }],
        };
        let err = compile_project_units(&store, &tasks).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ghost"), "{msg}");
    }

    #[test]
    fn top_level_var_global_is_detected_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = fixture_store(dir.path());
        store
            .create_pou_file("globals", PouType::Program, PouLanguage::St)
            .unwrap();
        store
            .write_pou_source(
                "globals",
                "VAR_GLOBAL\n shared_flag : BOOL;\nEND_VAR\n\
                 PROGRAM gprog\n VAR x : INT; END_VAR\n x := 1;\nEND_PROGRAM",
            )
            .unwrap();

        let globals = extract_project_global_vars(&store);
        assert_eq!(globals.len(), 1, "{globals:?}");
        assert_eq!(globals[0].0, "globals");
        assert_eq!(globals[0].1.to_lowercase(), "shared_flag");
    }
}

#[cfg(test)]
mod retain_tests {
    use super::compile_with_metadata;

    /// Wrapping the program in a CONFIGURATION is required for ironplc
    /// to accept the source; the synthesizer the bridge uses at runtime
    /// does the same thing.
    fn wrap(program: &str) -> String {
        format!(
            "{program}\n\
            CONFIGURATION cfg\n\
                RESOURCE res ON PLC\n\
                    TASK t1(INTERVAL := T#100ms, PRIORITY := 1);\n\
                    PROGRAM p1 WITH t1 : main;\n\
                END_RESOURCE\n\
            END_CONFIGURATION\n"
        )
    }

    #[test]
    fn retain_vars_are_extracted_from_program_var_retain_block() {
        let src = wrap(
            "PROGRAM main\n\
                VAR speed : INT := 0; END_VAR\n\
                VAR RETAIN setpoint : INT := 42; counter : DINT := 0; END_VAR\n\
                speed := setpoint;\n\
            END_PROGRAM",
        );
        let (_container, meta) = compile_with_metadata(&src).unwrap();
        assert_eq!(meta.retain_vars, vec!["counter", "setpoint"]);
    }

    #[test]
    fn programs_without_retain_blocks_produce_empty_metadata() {
        let src = wrap(
            "PROGRAM main\n\
                VAR x : INT := 1; END_VAR\n\
                x := x + 1;\n\
            END_PROGRAM",
        );
        let (_container, meta) = compile_with_metadata(&src).unwrap();
        assert!(meta.retain_vars.is_empty());
    }
}
