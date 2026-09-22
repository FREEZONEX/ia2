//! Keep upgrades honest at IA2's compiler / VM boundary, not just type-checking.
use std::time::Duration;

use ironplc_bridge::{compile, compile_project_units, spawn_units, ProgramUnit, VarSnapshot};
use ironplc_container::TaskType;
use project::{ProgramInstance, ProjectStore, Task, Tasks};

const COUNTER: &str = "PROGRAM main
VAR ticks : DINT := 0; END_VAR
ticks := ticks + 1;
END_PROGRAM
";

fn tasks() -> Tasks {
    Tasks {
        tasks: vec![Task {
            name: "slow".into(),
            interval_ms: 5000,
            priority: 1,
        }],
        programs: vec![ProgramInstance {
            instance: "main_inst".into(),
            program: "main".into(),
            task: "slow".into(),
        }],
    }
}

#[test]
fn source_hashes_identify_the_exact_parser_input() {
    let source = format!("{COUNTER}(* preserve bytes, including trailing whitespace *)  \n");
    let container = compile(&source).unwrap();
    let files = &container.debug_section.as_ref().unwrap().source_files;
    assert!(files
        .iter()
        .any(|file| file.content_hash == *blake3::hash(source.as_bytes()).as_bytes()));

    let dir = tempfile::tempdir().unwrap();
    let store = ProjectStore::create(dir.path().to_path_buf(), "hashes").unwrap();
    store.write_pou_source("main", &source).unwrap();
    let units = compile_project_units(&store, &tasks()).unwrap();
    let files = &units[0]
        .container
        .debug_section
        .as_ref()
        .unwrap()
        .source_files;
    let file = files
        .iter()
        .find(|file| file.path == "main")
        .unwrap_or_else(|| panic!("real file id missing in {files:?}"));
    assert_eq!(
        file.content_hash,
        *blake3::hash(source.as_bytes()).as_bytes()
    );
    assert!(!units[0]
        .container
        .debug_section
        .as_ref()
        .unwrap()
        .line_map
        .is_empty());
}

#[test]
fn fb_output_conditions_and_literal_len_execute_in_the_vm() {
    let container = compile(
        "PROGRAM main
VAR edge : R_TRIG; hits : INT; size : INT; END_VAR
edge(CLK := TRUE);
IF edge.Q THEN hits := hits + 1; END_IF;
size := LEN('literal');
END_PROGRAM",
    )
    .unwrap();
    let mut buffers = ironplc_vm::VmBuffers::from_container(&container);
    let mut vm = ironplc_vm::Vm::new()
        .load(&container, &mut buffers)
        .start()
        .unwrap();
    vm.run_round(0).unwrap();
    vm.run_round(10_000).unwrap();
    let vars = &container.debug_section.as_ref().unwrap().var_names;
    for (name, expected) in [("hits", 1), ("size", 7)] {
        let entry = vars.iter().find(|v| v.name == name).unwrap();
        assert_eq!(vm.read_variable_raw(entry.var_index).unwrap(), expected);
    }
}

#[test]
fn historical_globals_empty_vars_and_untyped_bit_literals_still_check_and_run() {
    let source = "VAR_GLOBAL g : INT := 7; END_VAR
PROGRAM main
VAR END_VAR
VAR control_word : WORD; result : INT; END_VAR
control_word := 16#0006;
control_word := control_word OR 16#0008;
result := g;
END_PROGRAM";
    assert!(ironplc_bridge::check(source).is_empty());
    let container = compile(source).unwrap();
    let mut buffers = ironplc_vm::VmBuffers::from_container(&container);
    let mut vm = ironplc_vm::Vm::new()
        .load(&container, &mut buffers)
        .start()
        .unwrap();
    vm.run_round(0).unwrap();
    let vars = &container.debug_section.as_ref().unwrap().var_names;
    for (name, expected) in [("control_word", 14), ("result", 7)] {
        let entry = vars.iter().find(|v| v.name == name).unwrap();
        assert_eq!(vm.read_variable_raw(entry.var_index).unwrap(), expected);
    }
}

async fn snapshot_at(
    rx: &mut tokio::sync::broadcast::Receiver<VarSnapshot>,
    scan: u64,
) -> VarSnapshot {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = rx.recv().await.unwrap();
            if snapshot.scan_count >= scan {
                return snapshot;
            }
        }
    })
    .await
    .expect("snapshot at requested scan")
}

#[tokio::test]
async fn cyclic_container_steps_and_resumes_without_silently_skipping_scans() {
    // Deliberately keep a cyclic container as input. Normalization must cover
    // direct compile callers as well as synthesized project configurations.
    let source = format!(
        "{COUNTER}
CONFIGURATION config
RESOURCE plc_res ON PLC
TASK slow(INTERVAL := T#5000ms, PRIORITY := 1);
PROGRAM main_inst WITH slow : main;
END_RESOURCE
END_CONFIGURATION"
    );
    let container = compile(&source).unwrap();
    assert!(container
        .task_table
        .tasks
        .iter()
        .any(|task| task.task_type == TaskType::Cyclic));
    let handle = spawn_units(
        vec![ProgramUnit {
            instance: "main_inst".into(),
            task_name: "slow".into(),
            interval_ms: 5000,
            priority: 1,
            container,
            retain_vars: vec![],
        }],
        vec![],
        vec![],
        None,
        Default::default(),
    );
    let mut rx = handle.subscribe();
    let initial = snapshot_at(&mut rx, 1).await;
    handle.pause();
    // Observe a paused-loop snapshot, so both subsequent steps start from pause.
    let _ = rx.recv().await.unwrap();
    let mut snapshots = vec![initial];
    for _ in 0..2 {
        let next = snapshots.last().unwrap().scan_count + 1;
        handle.step(1);
        snapshots.push(snapshot_at(&mut rx, next).await);
    }
    handle.resume();
    snapshots.push(snapshot_at(&mut rx, snapshots.last().unwrap().scan_count + 1).await);
    handle.shutdown().await;
    for snapshot in snapshots {
        let ticks = snapshot
            .vars
            .iter()
            .find(|var| var.name == "ticks")
            .unwrap()
            .bits;
        assert_eq!(
            ticks, snapshot.scan_count,
            "every reported scan must actually execute"
        );
    }
}

#[tokio::test]
async fn snapshots_use_upstream_renderer_without_reinterpreting_raw_slots() {
    let container = compile(
        "TYPE color : (red, green, blue); END_TYPE
PROGRAM main
VAR narrow : STRING := 'café'; wide : WSTRING := \"中\"; shade : color := green;
duration : TIME := T#1500ms; precise : LREAL := 1.25; timer : TON;
END_VAR
END_PROGRAM",
    )
    .unwrap();
    let handle = spawn_units(
        vec![ProgramUnit {
            instance: "main".into(),
            task_name: "slow".into(),
            interval_ms: 10,
            priority: 1,
            container,
            retain_vars: vec![],
        }],
        vec![],
        vec![],
        None,
        Default::default(),
    );
    let snapshot = snapshot_at(&mut handle.subscribe(), 1).await;
    handle.shutdown().await;
    let value = |name: &str| snapshot.vars.iter().find(|v| v.name == name).unwrap();
    assert_eq!(value("narrow").value, "'caf$E9'");
    assert_eq!(value("wide").value, "\"$4E2D\"");
    assert!(value("shade").value.eq_ignore_ascii_case("green (1)"));
    assert_eq!(value("duration").value, "T#1500ms");
    assert_eq!(value("duration").bits, 1500);
    assert_eq!(value("precise").value, "1.25");
    assert_eq!(value("precise").bits, 1.25_f64.to_bits());
    assert_eq!(value("timer").value, "<TON>");
}
