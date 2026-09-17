//! Boundary between the dependency-free `esi` crate and ia2's own
//! `project` types.
//!
//! The `esi` crate knows nothing about ia2: it parses ESI XML and produces
//! neutral [`esi::ProcessImage`] / [`esi::EsiChannel`] values. This module
//! maps those onto [`project::EthercatChannel`] (what the IO-map binds to)
//! and onto the cyclic exchange's offsets — keeping all ia2-specific
//! knowledge out of the reusable ESI crate.
//!
//! It is the *only* place that translates an ESI data-type name into ia2's
//! `EthercatDataType`, so the mapping policy lives in one spot.

use esi::{EsiChannel, PdoDirection};
use project::{EthercatChannel, EthercatDataType, EthercatPdoDirection};

/// Assemble a modular coupler's `project` channel list from its ESI XML and
/// the module idents it reports (slot order, e.g. read from `0xF050`).
///
/// This is the bridge the discovery path calls: parse → assemble → map.
/// Fully exercised off-hardware (the inputs are an XML string + an ident
/// slice), so the whole ESI-to-channels pipeline is unit-testable without a
/// bus.
pub fn assemble_channels(esi_xml: &str, detected: &[u32]) -> Result<Vec<EthercatChannel>, String> {
    let esi = esi::parse(esi_xml).map_err(|e| e.to_string())?;
    let image = esi::assemble(&esi, detected).map_err(|e| e.to_string())?;
    Ok(image
        .channels
        .iter()
        .enumerate()
        .map(|(i, c)| map_channel(i, c))
        .collect())
}

/// Map one ESI channel onto a `project::EthercatChannel`. The `slot` carried
/// by the ESI channel becomes the `slave_index` so the UI can group by
/// module; offsets are within the direction's image (matching
/// `pdi_byte_offset` semantics).
fn map_channel(_i: usize, c: &EsiChannel) -> EthercatChannel {
    EthercatChannel {
        name: c.name.clone(),
        slave_index: c.slot as u16,
        direction: match c.direction {
            PdoDirection::Input => EthercatPdoDirection::TxPdo,
            PdoDirection::Output => EthercatPdoDirection::RxPdo,
        },
        pdo_index: c.pdo_index,
        sub_index: c.sub_index,
        bit_length: c.bit_len,
        data_type: map_data_type(&c.data_type, c.bit_len),
        pdi_byte_offset: c.byte_offset,
        pdi_bit_offset: c.bit_offset,
    }
}

/// Map an ESI data-type name onto ia2's `EthercatDataType`.
///
/// ESI uses the IEC/ETG type names (`BOOL`, `UINT`, `INT`, `UDINT`,
/// `REAL32`, …) plus bit-width aliases (`UINT16`, `BIT`). Anything
/// unrecognized falls back to the closest unsigned type for the entry's bit
/// width, so a novel vendor type name degrades to a sane raw width rather
/// than failing the whole assembly.
///
/// The name and the BitLen are taken from the ESI independently and are NOT
/// cross-checked here, so this can emit a lane wider than the entry (a
/// 24-bit entry under an unknown name becomes U32, and a vendor naming an
/// 8-bit entry `UINT` becomes U16 at 8 bits). That is deliberate and safe:
/// `bits.rs` reads and writes exactly `bit_length` bits and extends into the
/// lane. It used to be neither — the accessors decoded by the lane's width
/// and indexed past the entry, so such a channel panicked the scan thread on
/// its first read. A lane NARROWER than the entry is refused outright, at
/// connect by `validate_channel_shapes` and again in the accessors.
fn map_data_type(name: &str, bit_len: u8) -> EthercatDataType {
    use EthercatDataType::*;
    match name.trim().to_ascii_uppercase().as_str() {
        "BOOL" | "BIT" => Bool,
        "BYTE" | "USINT" | "UINT8" | "UNSIGNED8" => U8,
        "SINT" | "INT8" | "INTEGER8" => I8,
        "WORD" | "UINT" | "UINT16" | "UNSIGNED16" => U16,
        "INT" | "INT16" | "INTEGER16" => I16,
        "DWORD" | "UDINT" | "UINT32" | "UNSIGNED32" => U32,
        "DINT" | "INT32" | "INTEGER32" => I32,
        "REAL" | "REAL32" | "FLOAT" => Real,
        _ => width_fallback(bit_len),
    }
}

/// Unsigned fallback by bit width for unrecognized data-type names.
fn width_fallback(bit_len: u8) -> EthercatDataType {
    match bit_len {
        1 => EthercatDataType::Bool,
        2..=8 => EthercatDataType::U8,
        9..=16 => EthercatDataType::U16,
        _ => EthercatDataType::U32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirror of the esi crate's synthetic coupler: 16DI(0x10) + 16DO(0x20).
    const ESI: &str = r##"
<EtherCATInfo><Descriptions>
 <Devices><Device><Type ProductCode="#x74">NX</Type>
  <Sm StartAddress="#x1100" ControlByte="#x64">Outputs</Sm>
  <Sm StartAddress="#x1400" ControlByte="#x20">Inputs</Sm>
 </Device></Devices>
 <Modules>
  <Module><Type ModuleIdent="#x10">16DI</Type>
   <TxPdo Sm="3"><Index>#x1A00</Index>
    <Entry><Index>#x6000</Index><SubIndex>1</SubIndex><BitLen>16</BitLen><Name>DI</Name><DataType>UINT</DataType></Entry>
   </TxPdo></Module>
  <Module><Type ModuleIdent="#x20">16DO</Type>
   <RxPdo Sm="2"><Index>#x1600</Index>
    <Entry><Index>#x7000</Index><SubIndex>1</SubIndex><BitLen>16</BitLen><Name>DO</Name><DataType>UINT</DataType></Entry>
   </RxPdo></Module>
 </Modules>
</Descriptions></EtherCATInfo>"##;

    #[test]
    fn assembles_project_channels_from_esi() {
        let chans = assemble_channels(ESI, &[0x10, 0x20]).unwrap();
        assert_eq!(chans.len(), 2);

        let di = chans.iter().find(|c| c.name == "m0_di").unwrap();
        assert_eq!(di.direction, EthercatPdoDirection::TxPdo);
        assert_eq!(di.slave_index, 0);
        assert_eq!(di.data_type, EthercatDataType::U16);
        assert_eq!(di.pdi_byte_offset, 0);
        assert_eq!(di.pdo_index, 0x1A00);
        assert_eq!(di.sub_index, 1);

        let do_ = chans.iter().find(|c| c.name == "m1_do").unwrap();
        assert_eq!(do_.direction, EthercatPdoDirection::RxPdo);
        assert_eq!(do_.slave_index, 1);
        assert_eq!(do_.pdi_byte_offset, 0); // first in the output image
    }

    #[test]
    fn maps_common_data_types() {
        assert_eq!(map_data_type("BOOL", 1), EthercatDataType::Bool);
        assert_eq!(map_data_type("uint", 16), EthercatDataType::U16);
        assert_eq!(map_data_type("INT", 16), EthercatDataType::I16);
        assert_eq!(map_data_type("UDINT", 32), EthercatDataType::U32);
        assert_eq!(map_data_type("REAL32", 32), EthercatDataType::Real);
        assert_eq!(map_data_type("DINT", 32), EthercatDataType::I32);
    }

    #[test]
    fn unknown_type_falls_back_to_width() {
        assert_eq!(map_data_type("VENDOR_T", 1), EthercatDataType::Bool);
        assert_eq!(map_data_type("WEIRD", 8), EthercatDataType::U8);
        assert_eq!(map_data_type("WEIRD", 16), EthercatDataType::U16);
        assert_eq!(map_data_type("WEIRD", 64), EthercatDataType::U32);
    }

    #[test]
    fn bad_esi_is_a_clear_error() {
        assert!(assemble_channels("<not valid", &[0x10]).is_err());
        // unknown module ident surfaces the assemble error
        assert!(assemble_channels(ESI, &[0xDEAD]).is_err());
    }
}
