//! Assemble a modular coupler's process image from its ESI + the modules
//! it detects at runtime.
//!
//! The master-side equivalent of what a config tool (TwinCAT, EC-Engineering)
//! does offline: walk the detected module idents (object `0xF050`) in slot
//! order, look each one up in the ESI module table, and concatenate that
//! module's PDO entries into the output (RxPDO → SM2) and input (TxPDO →
//! SM3) process images — tracking the exact bit/byte offset of every entry.
//! The result carries everything the fieldbus layer needs to program the
//! SyncManagers + FMMUs and to expose named channels for IO mapping.
//!
//! Bit packing follows the EtherCAT convention: entries within a direction
//! pack contiguously bit-by-bit (a 1-bit digital input occupies one bit);
//! vendors insert zero-index padding entries to byte-align where needed, so
//! this code simply advances a bit cursor and trusts the ESI's padding.

use crate::model::{Esi, EsiDevice, EsiModule, SmKind};

/// Process-data direction, named from the master's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdoDirection {
    /// RxPDO — master writes, device consumes (coupler outputs, SM2).
    Output,
    /// TxPDO — device produces, master reads (coupler inputs, SM3).
    Input,
}

/// One named, located process-data point — the unit the IO-map binds to.
/// Offsets are **within the entry's own direction image** (matching
/// ia2's `EthercatChannel.pdi_byte_offset` semantics), not a global PDI
/// offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EsiChannel {
    pub name: String,
    pub direction: PdoDirection,
    /// Byte offset within the direction's process image.
    pub byte_offset: u16,
    /// Bit offset within that byte (0 = LSB). Meaningful for sub-byte
    /// (e.g. 1-bit digital) entries; 0 for byte-aligned ones.
    pub bit_offset: u8,
    pub bit_len: u8,
    /// CoE object index this entry maps (e.g. 0x6000). 0 for padding —
    /// padding never produces a channel, so a channel's object is always
    /// real.
    pub object_index: u16,
    pub sub_index: u8,
    /// The containing PDO object index (e.g. 0x1A00).
    pub pdo_index: u16,
    /// ESI data-type name verbatim; the boundary maps it to its own enum.
    pub data_type: String,
    /// 0-based slot position (index into `detected`) this came from — lets
    /// the UI group channels by module.
    pub slot: usize,
}

/// SyncManager register values the master must program for one direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmConfig {
    pub kind: SmKind,
    pub direction: PdoDirection,
    /// Physical start address in the ESC (from the ESI `<Sm>`).
    pub start_address: u16,
    /// Byte length of this direction's process image.
    pub length: u16,
    /// SyncManager control byte (from the ESI `<Sm>`).
    pub control_byte: u8,
}

/// FMMU register values mapping a logical address range onto a SyncManager
/// window. Outputs are placed at logical 0; inputs follow, packed after
/// the outputs — the layout a single-coupler logical image uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FmmuConfig {
    pub direction: PdoDirection,
    /// Logical start address of the mapped range.
    pub logical_start: u32,
    /// Length in bytes.
    pub length: u16,
    /// Physical (ESC) start address — the SyncManager start for this
    /// direction.
    pub phys_start: u16,
}

/// The fully-assembled image: named channels plus the SM/FMMU register
/// values to bring the coupler to OP, and the per-direction byte counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessImage {
    pub channels: Vec<EsiChannel>,
    pub sm: Vec<SmConfig>,
    pub fmmu: Vec<FmmuConfig>,
    pub output_bytes: u16,
    pub input_bytes: u16,
}

/// Failure assembling a process image.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AssembleError {
    #[error("ESI describes no <Device>")]
    NoDevice,
    #[error("detected module ident {ident:#010x} (slot {slot}) is not in the ESI module table")]
    UnknownModule { slot: usize, ident: u32 },
    #[error("process image exceeds 65535 bytes in the {0:?} direction")]
    ImageTooLarge(PdoDirection),
    #[error("ESI device has no {0:?} SyncManager but the assembled image is non-empty")]
    MissingSyncManager(PdoDirection),
}

/// Assemble the process image for the first device in `esi`, given the
/// module idents the coupler reports in `0xF050` (in slot order).
///
/// Most coupler ESIs describe exactly one device; if yours has several,
/// pick the [`EsiDevice`] yourself and call [`assemble_for_device`].
pub fn assemble(esi: &Esi, detected: &[u32]) -> Result<ProcessImage, AssembleError> {
    let device = esi.devices.first().ok_or(AssembleError::NoDevice)?;
    assemble_for_device(device, &esi.modules, detected)
}

/// Assemble against an explicit device + module table — the form to use
/// when an ESI carries multiple `<Device>` entries.
pub fn assemble_for_device(
    device: &EsiDevice,
    modules: &[EsiModule],
    detected: &[u32],
) -> Result<ProcessImage, AssembleError> {
    let mut channels = Vec::new();
    // Per-direction running bit cursors. EtherCAT packs entries
    // contiguously by bits across all PDOs in the direction.
    let mut out_bits: u32 = 0;
    let mut in_bits: u32 = 0;

    for (slot, &ident) in detected.iter().enumerate() {
        let module = modules
            .iter()
            .find(|m| m.module_ident == ident)
            .ok_or(AssembleError::UnknownModule { slot, ident })?;

        for pdo in &module.rx_pdo {
            for e in &pdo.entries {
                place(
                    &mut channels,
                    &mut out_bits,
                    PdoDirection::Output,
                    slot,
                    pdo.index,
                    e,
                );
            }
        }
        for pdo in &module.tx_pdo {
            for e in &pdo.entries {
                place(
                    &mut channels,
                    &mut in_bits,
                    PdoDirection::Input,
                    slot,
                    pdo.index,
                    e,
                );
            }
        }
    }

    // Names are only knowable as a set once every module is placed.
    disambiguate(&mut channels);

    let output_bytes = bits_to_bytes(out_bits, PdoDirection::Output)?;
    let input_bytes = bits_to_bytes(in_bits, PdoDirection::Input)?;

    // SM + FMMU register values, only for non-empty directions.
    let mut sm = Vec::new();
    let mut fmmu = Vec::new();

    if output_bytes > 0 {
        let s = device
            .output_sm()
            .ok_or(AssembleError::MissingSyncManager(PdoDirection::Output))?;
        sm.push(SmConfig {
            kind: SmKind::Outputs,
            direction: PdoDirection::Output,
            start_address: s.start_address,
            length: output_bytes,
            control_byte: s.control_byte,
        });
        fmmu.push(FmmuConfig {
            direction: PdoDirection::Output,
            logical_start: 0,
            length: output_bytes,
            phys_start: s.start_address,
        });
    }
    if input_bytes > 0 {
        let s = device
            .input_sm()
            .ok_or(AssembleError::MissingSyncManager(PdoDirection::Input))?;
        sm.push(SmConfig {
            kind: SmKind::Inputs,
            direction: PdoDirection::Input,
            start_address: s.start_address,
            length: input_bytes,
            control_byte: s.control_byte,
        });
        // Inputs sit after outputs in the logical image.
        fmmu.push(FmmuConfig {
            direction: PdoDirection::Input,
            logical_start: output_bytes as u32,
            length: input_bytes,
            phys_start: s.start_address,
        });
    }

    Ok(ProcessImage {
        channels,
        sm,
        fmmu,
        output_bytes,
        input_bytes,
    })
}

/// Place one entry at the current bit cursor, emitting a channel for real
/// (non-padding) entries, then advance the cursor by its bit length.
fn place(
    channels: &mut Vec<EsiChannel>,
    bits: &mut u32,
    direction: PdoDirection,
    slot: usize,
    pdo_index: u16,
    e: &crate::model::Entry,
) {
    // Index 0 = padding/gap: it occupies space but is not a mappable point.
    if e.index != 0 {
        channels.push(EsiChannel {
            name: channel_name(slot, e),
            direction,
            byte_offset: (*bits / 8) as u16,
            bit_offset: (*bits % 8) as u8,
            bit_len: e.bit_len,
            object_index: e.index,
            sub_index: e.sub_index,
            pdo_index,
            data_type: e.data_type.clone(),
            slot,
        });
    }
    *bits += e.bit_len as u32;
}

/// Channel name: the ESI entry name, namespaced by slot so two modules
/// with same-named entries (e.g. two "DI" slices) stay unique. Falls back
/// to the object coordinates when the ESI omits a name **or when the name
/// survives sanitizing as nothing** — a wholly non-ASCII name (Chinese,
/// Japanese) sanitizes to the empty string, which used to leave the
/// channel called just `m0_`, and every such entry in a slot identical.
///
/// Uniqueness within a slot is NOT guaranteed here: vendors routinely give
/// every bit of a digital slice the same `<Name>` and distinguish them by
/// `SubIndex` alone. [`disambiguate`] resolves that afterwards, once all
/// the names are known.
fn channel_name(slot: usize, e: &crate::model::Entry) -> String {
    let sanitized = sanitize(&e.name);
    let base = if sanitized.is_empty() {
        format!("obj_{:04x}_{:02x}", e.index, e.sub_index)
    } else {
        sanitized
    };
    format!("m{slot}_{base}")
}

/// Make every channel name unique, leaving already-unique names untouched.
///
/// Vendor ESIs commonly name all eight entries of a digital slice `Input`
/// and separate them only by `SubIndex`, so the natural name collides.
/// Consumers key channels by name — ia2 rejects a duplicate outright at
/// connect — so an assembled image with repeats is one that cannot be
/// used, which defeats the point of generating it.
///
/// Colliding names get the entry's object coordinates appended, which are
/// unique by construction for a well-formed ESI. A malformed ESI that maps
/// the *same* object twice in one direction gets a trailing ordinal, so
/// this always terminates with distinct names.
///
/// Names that do not collide are deliberately left alone: a project
/// already bound to `m0_di` keeps working, and by definition every
/// currently-connectable config had unique names.
fn disambiguate(channels: &mut [EsiChannel]) {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for c in channels.iter() {
        *counts.entry(c.name.as_str()).or_default() += 1;
    }
    let collided: std::collections::HashSet<String> = counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(name, _)| name.to_string())
        .collect();
    if collided.is_empty() {
        return;
    }
    let mut used: std::collections::HashSet<String> = channels
        .iter()
        .map(|c| c.name.clone())
        .filter(|n| !collided.contains(n))
        .collect();
    for c in channels.iter_mut() {
        if !collided.contains(&c.name) {
            continue;
        }
        let mut candidate = format!("{}_{:04x}_{:02x}", c.name, c.object_index, c.sub_index);
        let mut n = 2;
        while used.contains(&candidate) {
            candidate = format!("{}_{:04x}_{:02x}_{n}", c.name, c.object_index, c.sub_index);
            n += 1;
        }
        used.insert(candidate.clone());
        c.name = candidate;
    }
}

/// Reduce an ESI entry name to a safe IEC/identifier-ish channel token:
/// lowercase, non-alphanumerics → `_`, collapsed.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_us = false;
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_us = false;
        } else if !last_us {
            out.push('_');
            last_us = true;
        }
    }
    out.trim_matches('_').to_string()
}

fn bits_to_bytes(bits: u32, dir: PdoDirection) -> Result<u16, AssembleError> {
    let bytes = bits.div_ceil(8);
    u16::try_from(bytes).map_err(|_| AssembleError::ImageTooLarge(dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::parse;

    /// Vendors name every bit of a digital slice `Input` and separate them
    /// by `SubIndex` alone. Consumers key channels by name — ia2 refuses a
    /// duplicate at connect — so repeats make the assembled image unusable.
    #[test]
    fn same_named_entries_in_one_module_get_distinct_names() {
        const DI: &str = r##"
<EtherCATInfo><Descriptions>
 <Devices><Device><Type ProductCode="#x74">CPL</Type>
  <Sm StartAddress="#x1400" ControlByte="#x20">Inputs</Sm>
 </Device></Devices>
 <Modules><Module><Type ModuleIdent="#x10">EL1008</Type>
  <TxPdo><Index>#x1A00</Index>
   <Entry><Index>#x6000</Index><SubIndex>1</SubIndex><BitLen>1</BitLen><Name>Input</Name><DataType>BOOL</DataType></Entry>
   <Entry><Index>#x6010</Index><SubIndex>1</SubIndex><BitLen>1</BitLen><Name>Input</Name><DataType>BOOL</DataType></Entry>
   <Entry><Index>#x6020</Index><SubIndex>1</SubIndex><BitLen>1</BitLen><Name>Input</Name><DataType>BOOL</DataType></Entry>
  </TxPdo></Module></Modules>
</Descriptions></EtherCATInfo>"##;
        let esi = parse(DI).expect("parses");
        let img = assemble(&esi, &[0x10]).expect("assembles");
        let names: Vec<&str> = img.channels.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["m0_input_6000_01", "m0_input_6010_01", "m0_input_6020_01"],
            "collided names take the entry's object coordinates"
        );
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "names must be unique");
        // The bits still land where they did — only the naming changed.
        let offsets: Vec<(u16, u8)> = img
            .channels
            .iter()
            .map(|c| (c.byte_offset, c.bit_offset))
            .collect();
        assert_eq!(offsets, [(0, 0), (0, 1), (0, 2)]);
    }

    /// A wholly non-ASCII `<Name>` sanitizes to nothing. It used to leave
    /// the channel called just `m0_`, so every such entry in a slot was
    /// indistinguishable — and Chinese/Japanese names are ordinary in
    /// vendor ESIs.
    #[test]
    fn non_ascii_names_fall_back_to_object_coordinates() {
        const CN: &str = r##"
<EtherCATInfo><Descriptions>
 <Devices><Device><Type ProductCode="#x74">CPL</Type>
  <Sm StartAddress="#x1400" ControlByte="#x20">Inputs</Sm>
 </Device></Devices>
 <Modules><Module><Type ModuleIdent="#x30">CN</Type>
  <TxPdo><Index>#x1A02</Index>
   <Entry><Index>#x6200</Index><SubIndex>1</SubIndex><BitLen>16</BitLen><Name>输入电压</Name><DataType>UINT</DataType></Entry>
   <Entry><Index>#x6210</Index><SubIndex>2</SubIndex><BitLen>16</BitLen><Name>输出电流</Name><DataType>UINT</DataType></Entry>
  </TxPdo></Module></Modules>
</Descriptions></EtherCATInfo>"##;
        let esi = parse(CN).expect("parses");
        let img = assemble(&esi, &[0x30]).expect("assembles");
        let names: Vec<&str> = img.channels.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["m0_obj_6200_01", "m0_obj_6210_02"]);
    }

    /// Unique names are left exactly as they were: a project already bound
    /// to `m0_di` must keep working across this change.
    #[test]
    fn unique_names_are_not_rewritten() {
        let esi = parse(ESI).expect("parses");
        let img = assemble(&esi, &[0x10, 0x20]).expect("assembles");
        for c in &img.channels {
            assert!(
                !c.name.contains("_6") && !c.name.contains("_7"),
                "unique name was needlessly rewritten: {}",
                c.name
            );
        }
    }

    // A synthetic 2-module coupler ESI: a 16-DI input module (ident 0x10)
    // and a 16-DO output module (ident 0x20), plus an 8-channel digital
    // input module (ident 0x30) to exercise bit packing.
    const ESI: &str = r##"
<EtherCATInfo>
 <Descriptions>
  <Devices>
   <Device>
    <Type ProductCode="#x00000074" RevisionNo="#x00010000">NX-COUPLER</Type>
    <Sm StartAddress="#x1000" ControlByte="#x26">MBoxOut</Sm>
    <Sm StartAddress="#x1080" ControlByte="#x22">MBoxIn</Sm>
    <Sm StartAddress="#x1100" ControlByte="#x64">Outputs</Sm>
    <Sm StartAddress="#x1400" ControlByte="#x20">Inputs</Sm>
   </Device>
  </Devices>
  <Modules>
   <Module>
    <Type ModuleIdent="#x00000010">NX-1600 16DI</Type>
    <TxPdo Sm="3">
     <Index>#x1A00</Index><Name>DI</Name>
     <Entry><Index>#x6000</Index><SubIndex>1</SubIndex><BitLen>16</BitLen><Name>Inputs</Name><DataType>UINT</DataType></Entry>
    </TxPdo>
   </Module>
   <Module>
    <Type ModuleIdent="#x00000020">NX-2400 16DO</Type>
    <RxPdo Sm="2">
     <Index>#x1600</Index><Name>DO</Name>
     <Entry><Index>#x7000</Index><SubIndex>1</SubIndex><BitLen>16</BitLen><Name>Outputs</Name><DataType>UINT</DataType></Entry>
    </RxPdo>
   </Module>
   <Module>
    <Type ModuleIdent="#x00000030">NX-1400 8DI</Type>
    <TxPdo Sm="3">
     <Index>#x1A10</Index><Name>DI8</Name>
     <Entry><Index>#x6010</Index><SubIndex>1</SubIndex><BitLen>1</BitLen><Name>Ch0</Name><DataType>BOOL</DataType></Entry>
     <Entry><Index>#x6010</Index><SubIndex>2</SubIndex><BitLen>1</BitLen><Name>Ch1</Name><DataType>BOOL</DataType></Entry>
    </TxPdo>
   </Module>
  </Modules>
 </Descriptions>
</EtherCATInfo>"##;

    fn esi() -> Esi {
        parse(ESI).expect("synthetic ESI parses")
    }

    #[test]
    fn parses_device_and_modules() {
        let e = esi();
        assert_eq!(e.devices.len(), 1);
        assert_eq!(e.devices[0].name, "NX-COUPLER");
        assert_eq!(e.devices[0].product_code, Some(0x74));
        assert_eq!(e.modules.len(), 3);
        assert_eq!(e.module(0x20).unwrap().name, "NX-2400 16DO");
        assert_eq!(e.devices[0].output_sm().unwrap().start_address, 0x1100);
        assert_eq!(e.devices[0].input_sm().unwrap().start_address, 0x1400);
    }

    #[test]
    fn assembles_input_only() {
        let img = assemble(&esi(), &[0x10]).unwrap();
        assert_eq!(img.input_bytes, 2);
        assert_eq!(img.output_bytes, 0);
        assert_eq!(img.channels.len(), 1);
        let c = &img.channels[0];
        assert_eq!(c.direction, PdoDirection::Input);
        assert_eq!(c.byte_offset, 0);
        assert_eq!(c.bit_len, 16);
        assert_eq!(c.object_index, 0x6000);
        assert_eq!(c.name, "m0_inputs");
        // One input SM + one input FMMU, no output ones.
        assert_eq!(img.sm.len(), 1);
        assert_eq!(img.sm[0].start_address, 0x1400);
        assert_eq!(img.sm[0].length, 2);
        assert_eq!(img.fmmu[0].logical_start, 0);
    }

    #[test]
    fn assembles_mixed_in_and_out_with_offsets() {
        // Slot order: 16DI, 16DO, 8DI → inputs = 16b + 2*1b, outputs = 16b.
        let img = assemble(&esi(), &[0x10, 0x20, 0x30]).unwrap();
        assert_eq!(img.output_bytes, 2); // the 16DO
        assert_eq!(img.input_bytes, 3); // 16b + 1b + 1b = 18 bits → 3 bytes
        assert_eq!(img.channels.len(), 4); // 1 in + 1 out + 2 in bits

        // Output channel at offset 0 in the output image.
        let out: Vec<_> = img
            .channels
            .iter()
            .filter(|c| c.direction == PdoDirection::Output)
            .collect();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].byte_offset, 0);
        assert_eq!(out[0].object_index, 0x7000);
        assert_eq!(out[0].slot, 1);

        // Input channels: the 16-bit at byte 0, then the two 1-bit entries
        // at byte 2 bit 0 and byte 2 bit 1 (packed after the 16 bits).
        let ins: Vec<_> = img
            .channels
            .iter()
            .filter(|c| c.direction == PdoDirection::Input)
            .collect();
        assert_eq!(ins.len(), 3);
        assert_eq!(
            (ins[0].byte_offset, ins[0].bit_offset, ins[0].bit_len),
            (0, 0, 16)
        );
        assert_eq!(
            (ins[1].byte_offset, ins[1].bit_offset, ins[1].bit_len),
            (2, 0, 1)
        );
        assert_eq!(
            (ins[2].byte_offset, ins[2].bit_offset, ins[2].bit_len),
            (2, 1, 1)
        );
        assert_eq!(ins[1].name, "m2_ch0");

        // SM/FMMU: outputs at logical 0, inputs after the outputs.
        let out_fmmu = img
            .fmmu
            .iter()
            .find(|f| f.direction == PdoDirection::Output)
            .unwrap();
        let in_fmmu = img
            .fmmu
            .iter()
            .find(|f| f.direction == PdoDirection::Input)
            .unwrap();
        assert_eq!(out_fmmu.logical_start, 0);
        assert_eq!(out_fmmu.length, 2);
        assert_eq!(in_fmmu.logical_start, 2); // after the 2 output bytes
        assert_eq!(in_fmmu.length, 3);
        assert_eq!(in_fmmu.phys_start, 0x1400);
    }

    #[test]
    fn padding_entry_advances_offset_without_a_channel() {
        // 8 bits real + 8 bits padding (Index 0) + 8 bits real → the second
        // real entry must land at byte 2, and padding emits no channel.
        let xml = r##"
<EtherCATInfo><Descriptions>
 <Devices><Device><Type>C</Type>
  <Sm StartAddress="#x1400" ControlByte="#x20">Inputs</Sm>
 </Device></Devices>
 <Modules><Module><Type ModuleIdent="#x1">M</Type>
  <TxPdo Sm="3"><Index>#x1A00</Index>
   <Entry><Index>#x6000</Index><SubIndex>1</SubIndex><BitLen>8</BitLen><Name>A</Name><DataType>USINT</DataType></Entry>
   <Entry><Index>0</Index><SubIndex>0</SubIndex><BitLen>8</BitLen><Name></Name><DataType></DataType></Entry>
   <Entry><Index>#x6000</Index><SubIndex>2</SubIndex><BitLen>8</BitLen><Name>B</Name><DataType>USINT</DataType></Entry>
  </TxPdo>
 </Module></Modules>
</Descriptions></EtherCATInfo>"##;
        let e = parse(xml).unwrap();
        let img = assemble(&e, &[0x1]).unwrap();
        assert_eq!(img.input_bytes, 3);
        assert_eq!(img.channels.len(), 2, "padding produced no channel");
        assert_eq!(img.channels[0].byte_offset, 0);
        assert_eq!(img.channels[1].byte_offset, 2); // skipped the padding byte
    }

    #[test]
    fn unknown_module_is_a_clear_error() {
        let err = assemble(&esi(), &[0x10, 0xDEAD]).unwrap_err();
        assert_eq!(
            err,
            AssembleError::UnknownModule {
                slot: 1,
                ident: 0xDEAD
            }
        );
    }

    #[test]
    fn empty_detected_list_is_an_empty_image() {
        let img = assemble(&esi(), &[]).unwrap();
        assert_eq!(img.output_bytes, 0);
        assert_eq!(img.input_bytes, 0);
        assert!(img.channels.is_empty());
        assert!(img.sm.is_empty());
        assert!(img.fmmu.is_empty());
    }
}
