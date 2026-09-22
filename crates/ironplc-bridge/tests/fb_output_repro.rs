//! Exploratory repro for: custom FUNCTION_BLOCK VAR_OUTPUT reads back 0
//! from ST PROGRAM calls, while FBD library FBs work.
//!
//! Scenarios:
//!   1. PROGRAM + FB in one source, `out => var` binding
//!   2. PROGRAM + FB in one source, `inst.out` dot access
//!   3. Same + a user FUNCTION present (function_id collision suspect)
//!   4. Two-file project via compile_project_units (scheduled path)
//!   5. Two-file project via compile_isolated_in_project_full (isolated path)

use ironplc_container::debug_format::VariableRenderer;
use ironplc_container::Container;
use ironplc_vm::{Vm, VmBuffers};

/// Compile, run `rounds` scan rounds, return (name, value-as-i64) pairs
/// for every debug-visible variable.
fn run_and_dump(container: &Container, rounds: u32) -> Vec<(String, i64)> {
    let debug_map = VariableRenderer::new(container);
    let mut bufs = VmBuffers::from_container(container);
    let mut running = Vm::new()
        .load(container, &mut bufs)
        .start()
        .expect("vm starts");
    for r in 0..rounds {
        running
            .run_round(r as u64 * 100_000)
            .expect("run_round succeeds");
    }
    let mut out = Vec::new();
    for i in 0..running.num_variables() {
        let raw = match running.read_variable_raw(ironplc_container::VarIndex::new(i)) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if let Some(info) = debug_map.var(i) {
            out.push((format!("[{}] {}", i, info.name), raw as i64));
        } else {
            out.push((format!("[{}] <no-debug>", i), raw as i64));
        }
    }
    out
}

fn value_of(dump: &[(String, i64)], name: &str) -> Option<i64> {
    dump.iter()
        .find(|(n, _)| n.ends_with(&format!("] {name}")))
        .map(|(_, v)| *v)
}

const FB_COUNTER: &str = "FUNCTION_BLOCK fb_counter\n\
    VAR_INPUT enable : BOOL; END_VAR\n\
    VAR_OUTPUT cnt : INT; END_VAR\n\
    VAR n : INT; END_VAR\n\
    IF enable THEN\n\
        n := n + 1;\n\
    END_IF;\n\
    cnt := n;\n\
END_FUNCTION_BLOCK\n";

#[test]
fn same_file_output_binding() {
    let src = format!(
        "{FB_COUNTER}\
        PROGRAM fbtest\n\
            VAR c : fb_counter; cnt_out : INT; END_VAR\n\
            c(enable := TRUE, cnt => cnt_out);\n\
        END_PROGRAM\n"
    );
    let container = ironplc_bridge::compile(&src).expect("compiles");
    let dump = run_and_dump(&container, 3);
    eprintln!("same_file_output_binding dump: {dump:#?}");
    assert_eq!(value_of(&dump, "cnt_out"), Some(3), "cnt_out after 3 scans");
}

#[test]
fn same_file_dot_access() {
    let src = format!(
        "{FB_COUNTER}\
        PROGRAM fbtest\n\
            VAR c : fb_counter; cnt_out : INT; END_VAR\n\
            c(enable := TRUE);\n\
            cnt_out := c.cnt;\n\
        END_PROGRAM\n"
    );
    let container = ironplc_bridge::compile(&src).expect("compiles");
    let dump = run_and_dump(&container, 3);
    eprintln!("same_file_dot_access dump: {dump:#?}");
    assert_eq!(value_of(&dump, "cnt_out"), Some(3), "cnt_out after 3 scans");
}

#[test]
fn with_user_function_present() {
    let src = format!(
        "FUNCTION add_one : INT\n\
            VAR_INPUT x : INT; END_VAR\n\
            add_one := x + 1;\n\
        END_FUNCTION\n\
        {FB_COUNTER}\
        PROGRAM fbtest\n\
            VAR c : fb_counter; cnt_out : INT; fn_out : INT; END_VAR\n\
            c(enable := TRUE, cnt => cnt_out);\n\
            fn_out := add_one(x := 41);\n\
        END_PROGRAM\n"
    );
    let container = ironplc_bridge::compile(&src).expect("compiles");
    let dump = run_and_dump(&container, 3);
    eprintln!("with_user_function_present dump: {dump:#?}");
    assert_eq!(value_of(&dump, "fn_out"), Some(42), "user FUNCTION result");
    assert_eq!(value_of(&dump, "cnt_out"), Some(3), "cnt_out after 3 scans");
}

/// Program first in source (the order compile_project_units produces:
/// hoisted PROGRAM, then shared elements).
#[test]
fn program_before_fb_in_source() {
    let src = format!(
        "PROGRAM fbtest\n\
            VAR c : fb_counter; cnt_out : INT; END_VAR\n\
            c(enable := TRUE, cnt => cnt_out);\n\
        END_PROGRAM\n\
        {FB_COUNTER}"
    );
    let container = ironplc_bridge::compile(&src).expect("compiles");
    let dump = run_and_dump(&container, 3);
    eprintln!("program_before_fb_in_source dump: {dump:#?}");
    assert_eq!(value_of(&dump, "cnt_out"), Some(3), "cnt_out after 3 scans");
}

// ---- project-store scenarios (two files, real run paths) ----

fn two_file_store(dir: &std::path::Path) -> (project::ProjectStore, project::Tasks) {
    let store =
        project::ProjectStore::create(dir.to_path_buf(), "fbrepro").expect("create project");
    // main.st is seeded by create() with the counter/blink template.
    store
        .write_pou_source(
            "fbtest.st",
            "PROGRAM fbtest\n\
                VAR c : fb_counter; cnt_out : INT; END_VAR\n\
                c(enable := TRUE, cnt => cnt_out);\n\
            END_PROGRAM\n",
        )
        .expect("write fbtest.st");
    store
        .write_pou_source("fb_counter.st", FB_COUNTER)
        .expect("write fb_counter.st");
    let tasks = project::Tasks {
        tasks: vec![project::Task {
            name: "plc_task".into(),
            interval_ms: 100,
            priority: 1,
        }],
        programs: vec![project::ProgramInstance {
            instance: "fbtest_inst".into(),
            program: "fbtest".into(),
            task: "plc_task".into(),
        }],
    };
    store.write_tasks(&tasks).expect("write tasks");
    (store, tasks)
}

#[test]
fn project_units_two_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (store, tasks) = two_file_store(dir.path());
    let units = ironplc_bridge::compile_project_units(&store, &tasks).expect("units compile");
    assert_eq!(units.len(), 1);
    let dump = run_and_dump(&units[0].container, 3);
    eprintln!("project_units_two_files dump: {dump:#?}");
    assert_eq!(value_of(&dump, "cnt_out"), Some(3), "cnt_out after 3 scans");
}

#[test]
fn isolated_two_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (store, tasks) = two_file_store(dir.path());
    let (container, _meta) =
        ironplc_bridge::compile_isolated_in_project_full(&store, "fbtest.st", &tasks)
            .expect("isolated compile");
    let dump = run_and_dump(&container, 3);
    eprintln!("isolated_two_files dump: {dump:#?}");
    assert_eq!(value_of(&dump, "cnt_out"), Some(3), "cnt_out after 3 scans");
}

/// Both the template main_inst and fbtest_inst scheduled — the layout a
/// user most likely has after adding a POU without removing the seed.
#[test]
fn project_units_both_instances() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (store, mut tasks) = two_file_store(dir.path());
    tasks.programs.push(project::ProgramInstance {
        instance: "main_inst".into(),
        program: "main".into(),
        task: "plc_task".into(),
    });
    let units = ironplc_bridge::compile_project_units(&store, &tasks).expect("units compile");
    assert_eq!(units.len(), 2);
    let fbtest_unit = units
        .iter()
        .find(|u| u.instance == "fbtest_inst")
        .expect("fbtest unit");
    let dump = run_and_dump(&fbtest_unit.container, 3);
    eprintln!("project_units_both_instances dump: {dump:#?}");
    assert_eq!(value_of(&dump, "cnt_out"), Some(3), "cnt_out after 3 scans");
}

/// BOOL and REAL outputs — type-specific copy-out behaviour.
#[test]
fn same_file_bool_real_outputs() {
    let src = "FUNCTION_BLOCK fb_mix\n\
        VAR_INPUT tick : BOOL; END_VAR\n\
        VAR_OUTPUT flag : BOOL; ratio : REAL; END_VAR\n\
        VAR n : INT; END_VAR\n\
        IF tick THEN n := n + 1; END_IF;\n\
        flag := (n MOD 2) = 1;\n\
        ratio := INT_TO_REAL(n) * 0.5;\n\
    END_FUNCTION_BLOCK\n\
    PROGRAM fbtest\n\
        VAR m : fb_mix; flag_out : BOOL; ratio_out : REAL; END_VAR\n\
        m(tick := TRUE, flag => flag_out, ratio => ratio_out);\n\
    END_PROGRAM\n";
    let container = ironplc_bridge::compile(src).expect("compiles");
    let dump = run_and_dump(&container, 3);
    eprintln!("same_file_bool_real_outputs dump: {dump:#?}");
    assert_eq!(value_of(&dump, "flag_out"), Some(1), "flag after 3 scans");
    let ratio_bits = value_of(&dump, "ratio_out").expect("ratio_out present");
    let ratio = f32::from_bits((ratio_bits as u64 as u32).to_le());
    assert!(
        (ratio - 1.5).abs() < 1e-6,
        "ratio after 3 scans = {ratio} (raw {ratio_bits:#x})"
    );
}

/// A user FB that itself instantiates another FB (here a stdlib TON) —
/// nested FB instances have no storage model in the vendored codegen yet.
/// Pin the failure mode: a LOUD compile error whose message names the
/// nested instance and suggests the workaround, never a silent misrun.
/// When the vendor gains nested-FB support this test should flip into a
/// behavioural one (done_out fires after the TON elapses).
#[test]
fn fb_with_nested_ton_errors_loudly() {
    let src = "FUNCTION_BLOCK fb_delay\n\
        VAR_INPUT go : BOOL; END_VAR\n\
        VAR_OUTPUT done : BOOL; END_VAR\n\
        VAR t : TON; END_VAR\n\
        t(IN := go, PT := T#50ms);\n\
        done := t.Q;\n\
    END_FUNCTION_BLOCK\n\
    PROGRAM fbtest\n\
        VAR d : fb_delay; done_out : BOOL; END_VAR\n\
        d(go := TRUE, done => done_out);\n\
    END_PROGRAM\n";
    let err = ironplc_bridge::compile(src).expect_err("nested FB must not compile silently");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("P9999"),
        "nested-FB codegen must explicitly reject unsupported code, got: {msg}"
    );
}

/// Isolated run where the FB's file ALSO declares a PROGRAM — the sibling
/// filter drops the whole file. Is the failure loud or silent?
#[test]
fn isolated_fb_file_contains_program() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (store, tasks) = two_file_store(dir.path());
    store
        .write_pou_source(
            "fb_counter.st",
            &format!(
                "{FB_COUNTER}\
                PROGRAM fb_smoke\n\
                    VAR s : fb_counter; smoke : INT; END_VAR\n\
                    s(enable := TRUE, cnt => smoke);\n\
                END_PROGRAM\n"
            ),
        )
        .expect("rewrite fb file with embedded PROGRAM");
    let result = ironplc_bridge::compile_isolated_in_project_full(&store, "fbtest.st", &tasks);
    match result {
        Ok((container, _)) => {
            let dump = run_and_dump(&container, 3);
            eprintln!("isolated_fb_file_contains_program dump: {dump:#?}");
            panic!("expected a loud compile error (fb_counter's file was excluded)");
        }
        Err(e) => eprintln!("isolated_fb_file_contains_program errored (good if loud): {e:?}"),
    }
}

/// `compile_project` (the `cs project check` / "does it compile" entry
/// point) must assemble per-instance hoisted units. The historical
/// whole-project concatenation compiled whichever PROGRAM sorted first
/// (the seeded main.st template) regardless of the schedule — the
/// requested program's FB outputs then read as missing/zero.
#[test]
fn compile_project_hoists_scheduled_instance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (store, _tasks) = two_file_store(dir.path());
    let units = ironplc_bridge::compile_project(&store).expect("project compiles");
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].instance, "fbtest_inst");
    let dump = run_and_dump(&units[0].container, 3);
    eprintln!("compile_project_hoists_scheduled_instance dump: {dump:#?}");
    assert_eq!(
        value_of(&dump, "cnt_out"),
        Some(3),
        "scheduled PROGRAM fbtest is the one compiled; its cnt_out counts"
    );
    assert_eq!(
        value_of(&dump, "counter"),
        None,
        "template main's variables must not bleed into the unit"
    );
}
