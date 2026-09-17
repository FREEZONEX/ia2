//! EtherCAT Slave Information (ESI / ETG.2000) parsing + modular
//! process-image assembly.
//!
//! ## Why this crate exists
//!
//! ia2's EtherCAT layer discovers process data by reading a SubDevice's
//! *runtime* CoE PDO-assignment objects (`0x1C12`/`0x1C13` → `0x16xx`/
//! `0x1Axx`). That works for fixed-PDO servos and slices, but **fails for
//! ESI-driven modular bus couplers**: those expect the *master* to build
//! the process image from their ESI file + the modules it detects at
//! runtime (object `0xF050`, the Scanned Module Ident List). They do not
//! publish the assembled module PDOs over runtime CoE — `0x1C12` is
//! read-only, `0xF030` is absent — so auto-discovery has nothing to read.
//!
//! This crate is the master-side half of that model: parse the vendor ESI
//! once, then — given the module idents a coupler reports at `0xF050` —
//! [`assemble`] the exact byte layout of the input (TxPDO) and output
//! (RxPDO) process images, plus the Sync-Manager and FMMU register values
//! the master must program. The fieldbus layer (`iomap-ethercat`) takes
//! that layout and drives the bus.
//!
//! ## Deliberately dependency-free
//!
//! Nothing here touches a fieldbus crate. Parsing and assembly are pure
//! data transforms over plain structs, so the whole thing is unit-testable
//! **without hardware** and reusable by any EtherCAT master — not just
//! ia2's ethercrab-based one. That isolation is intentional: it keeps the
//! ESI logic decoupled from ethercrab's evolving API and from ia2's own
//! `project` types, which the boundary in `iomap-ethercat` maps onto.
//!
//! ## Scope
//!
//! The ESI schema is large; this parses the subset a master needs to bring
//! a modular coupler to OP: the device's `<Sm>` declarations, the module
//! table (`<Module>` keyed by `ModuleIdent`) and each module's
//! `<TxPdo>`/`<RxPdo>` entries. `<Fmmu>` is **not** parsed — the
//! [`FmmuConfig`] values are derived from the assembled image (outputs at
//! logical 0, inputs after them, each pointing at its SyncManager's start
//! address), not read from the file. Nor are `<Slots>`: nothing here
//! bounds which module may occupy which slot, so the detected ident list
//! is trusted as given. Profile
//! details unrelated to the process image (diagnostics objects, ESC
//! vendor metadata, distributed-clock opmodes) are skipped — they can be
//! added without breaking the parsed model because every field is
//! `#[serde(default)]`-tolerant of absence.

mod assemble;
mod model;
mod num;

pub use assemble::{
    assemble, assemble_for_device, AssembleError, EsiChannel, FmmuConfig, PdoDirection,
    ProcessImage, SmConfig,
};
pub use model::{parse, Entry, Esi, EsiDevice, EsiModule, ParseError, Pdo, Sm, SmKind};
