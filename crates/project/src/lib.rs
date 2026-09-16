//! Disk-backed project store.
//!
//! A project lives in a single directory:
//!
//! ```text
//! my_project/
//! ├── project.toml          # manifest
//! ├── applications/         # one .st file per POU
//! │   └── main.st
//! ├── devices/              # one .toml per device (protocol inside)
//! │   └── tank.toml
//! └── iomap.toml             # variable ↔ channel bindings
//! ```

mod alarm_check;
mod device_check;
mod errors;
mod fbd;
pub mod hmi;
mod iomap_check;
mod ld;
mod paths;
mod sfc;
mod store;
mod types;

pub use alarm_check::{validate_alarms, AlarmIssue};
pub use device_check::{duplicate_channel_names, validate_devices, DeviceIssue};
pub use errors::StoreError;
pub use fbd::{
    FbdBlock, FbdInputBinding, FbdInputSource, FbdOutputBinding, FbdPosition, FbdProgram,
};
pub use hmi::{
    apply_hmi_ops, hmi_nav_targets, hmi_variables, hmi_write_variables, validate_hmi, HmiAction,
    HmiBinding, HmiBindingSpec, HmiDoc, HmiGrid, HmiIssue, HmiLayout, HmiMapEntry, HmiNode,
    HmiNodeKind, HmiOp, HmiSeries, HmiShapeKind, HmiTextStyle, HMI_SYMBOLS,
};
pub use iomap_check::{validate_iomap, IomapIssue, IomapIssueSeverity};
pub use ld::{
    LdCoil, LdCoilKind, LdComparator, LdFbInput, LdNode, LdOperand, LdPouType, LdProgram, LdRung,
    LdVarSection, LdVariable,
};
pub use paths::{
    default_projects_dir, default_state_path, home_dir, load_last_opened, migrate_legacy_dirs,
    save_last_opened,
};
pub use sfc::{SfcAction, SfcProgram, SfcQualifier, SfcStep, SfcTransition};
pub use store::{is_library_slug, MigrationReport, ProjectStore, LIBRARY_SLUG_PREFIX};
pub use types::{
    AlarmCondition, AlarmConfig, AlarmDef, AlarmSeverity, CanopenAccess, CanopenChannel,
    CanopenConfig, CanopenDataType, CanopenTransport, Device, Direction, Edge, EthercatBringup,
    EthercatChannel, EthercatConfig, EthercatDataType, EthercatDcSync, EthercatGear,
    EthercatPdoDirection, EthercatSdoInit, EthercatSlave, GearMaster, IoMap, Mapping, ModbusAccess,
    ModbusChannel, ModbusChannelKind, ModbusConfig, ModbusDataBits, ModbusDataType, ModbusParity,
    ModbusRs485, ModbusRtuParams, ModbusStopBits, ModbusTcpParams, ModbusTransport,
    ModbusWordOrder, MqttNorthbound, NorthboundConfig, OpcuaAccess, OpcuaAuth, OpcuaChannel,
    OpcuaConfig, OpcuaDataType, Pou, PouDecl, PouFile, PouFileSource, PouLanguage, PouType,
    ProgramInstance, ProjectListing, ProjectManifest, ProjectTree, ProjectTreeSkeleton, Protocol,
    ProtocolConfig, Task, Tasks, WriteGovernance, WriteMode, WriteRule,
};
