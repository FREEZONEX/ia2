//! The ESI document model.
//!
//! Two layers, on purpose:
//!   * private `raw::*` structs mirror the XML 1:1 with every number as a
//!     `String`, because vendors mix `#x`/`0x`/decimal and sprinkle
//!     optional elements unpredictably — `quick-xml`'s serde path
//!     tolerates that when fields are `Option`/`Vec`/`default`;
//!   * the public typed model ([`Esi`] etc.) has real `u16`/`u8` fields and
//!     normalized enums, produced by validating the raw layer in [`parse`].
//!
//! Keeping the messy parse and the clean model separate means a vendor
//! quirk shows up as one localized conversion error, not a deserialize
//! failure three layers deep.

/// A parsed ESI document, reduced to what a master needs to build a
/// modular process image: the device(s) it describes and the module table
/// keyed by `ModuleIdent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Esi {
    pub devices: Vec<EsiDevice>,
    pub modules: Vec<EsiModule>,
}

impl Esi {
    /// Look a module up by the ident a coupler reports in `0xF050`.
    pub fn module(&self, ident: u32) -> Option<&EsiModule> {
        self.modules.iter().find(|m| m.module_ident == ident)
    }
}

/// One `<Device>`, reduced to its Sync-Manager declarations.
///
/// `<Fmmu>` and `<Slots>` are not parsed: the FMMU register values are
/// derived during assembly rather than read from the file, and nothing
/// enforces slot rules — the detected module list is trusted as given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EsiDevice {
    pub name: String,
    pub product_code: Option<u32>,
    pub revision: Option<u32>,
    /// Sync managers in declaration order (SM0..SMn).
    pub sm: Vec<Sm>,
}

impl EsiDevice {
    /// The Sync-Manager carrying outputs (RxPDO, master→device) — SM2 on a
    /// conventional coupler, classified by its ESI label.
    pub fn output_sm(&self) -> Option<&Sm> {
        self.sm.iter().find(|s| s.kind == SmKind::Outputs)
    }

    /// The Sync-Manager carrying inputs (TxPDO, device→master) — SM3 on a
    /// conventional coupler.
    pub fn input_sm(&self) -> Option<&Sm> {
        self.sm.iter().find(|s| s.kind == SmKind::Inputs)
    }
}

/// One `<Sm>` entry: where the SyncManager window sits in the ESC and what
/// it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sm {
    /// Physical start address in the ESC (e.g. 0x1100 for SM2).
    pub start_address: u16,
    /// SyncManager control byte (buffer type / direction / watchdog bits).
    pub control_byte: u8,
    /// Role, classified from the ESI text label.
    pub kind: SmKind,
}

/// What a SyncManager carries, classified from the ESI `<Sm>` text label
/// (`MBoxOut` / `MBoxIn` / `Outputs` / `Inputs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmKind {
    MailboxOut,
    MailboxIn,
    /// Process-data outputs — RxPDO, master writes (SM2).
    Outputs,
    /// Process-data inputs — TxPDO, master reads (SM3).
    Inputs,
    /// Unrecognized label — kept so the SM list stays index-aligned.
    Unknown,
}

impl SmKind {
    fn classify(label: &str) -> Self {
        let l = label.trim().to_ascii_lowercase();
        match l.as_str() {
            "mboxout" | "mailboxout" => SmKind::MailboxOut,
            "mboxin" | "mailboxin" => SmKind::MailboxIn,
            "outputs" => SmKind::Outputs,
            "inputs" => SmKind::Inputs,
            _ => SmKind::Unknown,
        }
    }
}

/// One `<Module>` — a swappable slice with its own fixed PDO mapping,
/// selected at runtime by the ident the coupler reports in `0xF050`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EsiModule {
    pub name: String,
    /// The `ModuleIdent` — matched against the `0xF050` detected-module
    /// list to pick this module's layout.
    pub module_ident: u32,
    /// Inputs: device→master. Concatenated into SM3.
    pub tx_pdo: Vec<Pdo>,
    /// Outputs: master→device. Concatenated into SM2.
    pub rx_pdo: Vec<Pdo>,
}

/// One PDO object (`<TxPdo>` / `<RxPdo>`) and its entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pdo {
    /// CoE object index (e.g. 0x1A00).
    pub index: u16,
    pub name: String,
    pub entries: Vec<Entry>,
}

/// One PDO entry — a single mapped object that occupies `bit_len` bits of
/// the process image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Mapped object index (e.g. 0x6000). 0 = a gap/padding entry.
    pub index: u16,
    pub sub_index: u8,
    pub bit_len: u8,
    pub name: String,
    /// ESI data-type name verbatim (e.g. "UINT", "BOOL", "INT16"). The
    /// boundary layer maps this onto its own type enum; kept as a string
    /// here so this crate stays agnostic of any consumer's type set.
    pub data_type: String,
}

/// Parse an ESI XML document into the typed model.
pub fn parse(xml: &str) -> Result<Esi, ParseError> {
    let raw: raw::EtherCatInfo =
        quick_xml::de::from_str(xml).map_err(|e| ParseError::Xml(e.to_string()))?;
    raw.into_typed()
}

/// Failure parsing or normalizing an ESI document.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("ESI XML parse error: {0}")]
    Xml(String),
    #[error("ESI value error: {0}")]
    Value(String),
}

// ---------------------------------------------------------------------------
//  Raw XML layer (private) — numbers as strings, everything optional.
// ---------------------------------------------------------------------------

mod raw {
    use serde::Deserialize;

    use super::{Entry, Esi, EsiDevice, EsiModule, ParseError, Pdo, Sm, SmKind};
    use crate::num::{parse_u16, parse_u32, parse_u8};

    #[derive(Debug, Deserialize)]
    pub(super) struct EtherCatInfo {
        #[serde(rename = "Descriptions", default)]
        descriptions: Descriptions,
    }

    #[derive(Debug, Default, Deserialize)]
    struct Descriptions {
        #[serde(rename = "Devices", default)]
        devices: Devices,
        #[serde(rename = "Modules", default)]
        modules: Modules,
    }

    #[derive(Debug, Default, Deserialize)]
    struct Devices {
        #[serde(rename = "Device", default)]
        device: Vec<Device>,
    }

    #[derive(Debug, Default, Deserialize)]
    struct Modules {
        #[serde(rename = "Module", default)]
        module: Vec<Module>,
    }

    #[derive(Debug, Deserialize)]
    struct Device {
        #[serde(rename = "Type", default)]
        ty: TypeTag,
        #[serde(rename = "Sm", default)]
        sm: Vec<SmRaw>,
    }

    #[derive(Debug, Default, Deserialize)]
    struct TypeTag {
        #[serde(rename = "@ProductCode")]
        product_code: Option<String>,
        #[serde(rename = "@RevisionNo")]
        revision: Option<String>,
        #[serde(rename = "@ModuleIdent")]
        module_ident: Option<String>,
        #[serde(rename = "$text", default)]
        text: String,
    }

    #[derive(Debug, Deserialize)]
    struct SmRaw {
        #[serde(rename = "@StartAddress")]
        start_address: Option<String>,
        #[serde(rename = "@ControlByte")]
        control_byte: Option<String>,
        #[serde(rename = "$text", default)]
        label: String,
    }

    #[derive(Debug, Deserialize)]
    struct Module {
        #[serde(rename = "Type", default)]
        ty: TypeTag,
        #[serde(rename = "Name", default)]
        name: Option<String>,
        #[serde(rename = "TxPdo", default)]
        tx_pdo: Vec<PdoRaw>,
        #[serde(rename = "RxPdo", default)]
        rx_pdo: Vec<PdoRaw>,
    }

    #[derive(Debug, Deserialize)]
    struct PdoRaw {
        #[serde(rename = "Index", default)]
        index: String,
        #[serde(rename = "Name", default)]
        name: String,
        #[serde(rename = "Entry", default)]
        entry: Vec<EntryRaw>,
    }

    #[derive(Debug, Deserialize)]
    struct EntryRaw {
        #[serde(rename = "Index", default)]
        index: String,
        #[serde(rename = "SubIndex", default)]
        sub_index: String,
        #[serde(rename = "BitLen", default)]
        bit_len: String,
        #[serde(rename = "Name", default)]
        name: String,
        #[serde(rename = "DataType", default)]
        data_type: String,
    }

    impl EtherCatInfo {
        pub(super) fn into_typed(self) -> Result<Esi, ParseError> {
            let devices = self
                .descriptions
                .devices
                .device
                .into_iter()
                .map(Device::into_typed)
                .collect::<Result<Vec<_>, _>>()?;
            let modules = self
                .descriptions
                .modules
                .module
                .into_iter()
                .map(Module::into_typed)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Esi { devices, modules })
        }
    }

    fn v(s: String) -> ParseError {
        ParseError::Value(s)
    }

    impl Device {
        fn into_typed(self) -> Result<EsiDevice, ParseError> {
            let product_code = self
                .ty
                .product_code
                .as_deref()
                .map(parse_u32)
                .transpose()
                .map_err(v)?;
            let revision = self
                .ty
                .revision
                .as_deref()
                .map(parse_u32)
                .transpose()
                .map_err(v)?;
            let sm = self
                .sm
                .into_iter()
                .map(SmRaw::into_typed)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(EsiDevice {
                name: self.ty.text.trim().to_string(),
                product_code,
                revision,
                sm,
            })
        }
    }

    impl SmRaw {
        fn into_typed(self) -> Result<Sm, ParseError> {
            // Outputs/Inputs SMs always carry start + control; mailbox SMs
            // may be terse. Default the address to 0 when truly absent so a
            // partial ESI still classifies rather than failing the parse.
            let start_address = self
                .start_address
                .as_deref()
                .map(parse_u16)
                .transpose()
                .map_err(v)?
                .unwrap_or(0);
            let control_byte = self
                .control_byte
                .as_deref()
                .map(parse_u8)
                .transpose()
                .map_err(v)?
                .unwrap_or(0);
            Ok(Sm {
                start_address,
                control_byte,
                kind: SmKind::classify(&self.label),
            })
        }
    }

    impl Module {
        fn into_typed(self) -> Result<EsiModule, ParseError> {
            let module_ident = self
                .ty
                .module_ident
                .as_deref()
                .map(parse_u32)
                .transpose()
                .map_err(v)?
                .ok_or_else(|| ParseError::Value("module missing ModuleIdent".into()))?;
            // Name: prefer the dedicated <Name>, fall back to <Type> text.
            let name = self
                .name
                .filter(|n| !n.trim().is_empty())
                .unwrap_or(self.ty.text)
                .trim()
                .to_string();
            let tx_pdo = self
                .tx_pdo
                .into_iter()
                .map(PdoRaw::into_typed)
                .collect::<Result<Vec<_>, _>>()?;
            let rx_pdo = self
                .rx_pdo
                .into_iter()
                .map(PdoRaw::into_typed)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(EsiModule {
                name,
                module_ident,
                tx_pdo,
                rx_pdo,
            })
        }
    }

    impl PdoRaw {
        fn into_typed(self) -> Result<Pdo, ParseError> {
            let index = parse_u16(&self.index).map_err(v)?;
            let entries = self
                .entry
                .into_iter()
                .map(EntryRaw::into_typed)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Pdo {
                index,
                name: self.name.trim().to_string(),
                entries,
            })
        }
    }

    impl EntryRaw {
        fn into_typed(self) -> Result<Entry, ParseError> {
            // A gap/padding entry has Index 0 and no SubIndex; allow empty.
            let index = if self.index.trim().is_empty() {
                0
            } else {
                parse_u16(&self.index).map_err(v)?
            };
            let sub_index = if self.sub_index.trim().is_empty() {
                0
            } else {
                parse_u8(&self.sub_index).map_err(v)?
            };
            let bit_len = parse_u8(&self.bit_len).map_err(v)?;
            Ok(Entry {
                index,
                sub_index,
                bit_len,
                name: self.name.trim().to_string(),
                data_type: self.data_type.trim().to_string(),
            })
        }
    }
}
