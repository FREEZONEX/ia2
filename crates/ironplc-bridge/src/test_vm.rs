//! Test-only: run compiled ST in the VM and read variables back by name.
//!
//! The transpiler tests used to stop at `crate::check`, which runs the
//! analyzer only. ironplc's codegen rejects some shapes the analyzer
//! accepts, so "checks clean" did not mean "runs" — and nothing asserted
//! what the generated program actually did.

use std::collections::HashMap;

use ironplc_container::debug_format::VariableRenderer;
use ironplc_container::{Container, VarIndex};
use ironplc_vm::{Vm, VmBuffers};

/// Compile `st` through the same analyze + codegen path the runtime uses,
/// run `rounds` scans 100 ms apart, and return every named variable's raw
/// slot value.
pub(crate) fn run_st(st: &str, rounds: u32) -> HashMap<String, i64> {
    let container = crate::compile(st).unwrap_or_else(|e| panic!("does not compile: {e}\n{st}"));
    run_container(&container, rounds)
}

pub(crate) fn run_container(container: &Container, rounds: u32) -> HashMap<String, i64> {
    let names = VariableRenderer::new(container);
    let mut bufs = VmBuffers::from_container(container);
    let mut vm = Vm::new()
        .load(container, &mut bufs)
        .start()
        .expect("vm starts");
    for r in 0..rounds {
        vm.run_round(u64::from(r) * 100_000).expect("scan runs");
    }
    (0..vm.num_variables())
        .filter_map(|i| {
            let raw = vm.read_variable_raw(VarIndex::new(i)).ok()?;
            names.var(i).map(|info| (info.name.clone(), raw as i64))
        })
        .collect()
}
