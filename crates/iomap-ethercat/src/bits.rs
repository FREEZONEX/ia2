//! Pure bit-packing helpers for PDI buffers.
//!
//! EtherCAT PDOs pack values at byte + bit offsets within a SubDevice's
//! input or output PDI buffer. For byte-aligned 8/16/32 bit values this
//! is a trivial slice; for digital I/O (1 bit, often 8 channels per byte)
//! we mask + shift. Centralised here so the cyclic-exchange code and the
//! `read_channel` / `write_channel` paths share one bit layout — and so
//! we can unit-test the layout without spinning up ethercrab at all.
//!
//! Endianness: EtherCAT is little-endian. All multi-byte reads/writes
//! use LE byte order. The byte order is not configurable.

use iocore::{ChannelValue, IoError};
use project::EthercatDataType;

/// How many bits the lane for `data_type` can carry.
fn lane_bits(data_type: EthercatDataType) -> u8 {
    match data_type {
        EthercatDataType::Bool => 1,
        EthercatDataType::U8 | EthercatDataType::I8 => 8,
        EthercatDataType::U16 | EthercatDataType::I16 => 16,
        EthercatDataType::U32 | EthercatDataType::I32 | EthercatDataType::Real => 32,
    }
}

/// Reject a `(data_type, bit_length)` pair these accessors cannot serve
/// honestly. Shared with `validate.rs`, so a connect fails with the same rule
/// the accessors enforce rather than the two drifting apart.
///
/// `bit_length` is the wire fact — the PDO entry's BitLen, from ESI or from
/// the PDO-assignment scan — and `data_type` is the interpretation. A lane
/// WIDER than the entry is fine and normal (a 24-bit vendor entry read as
/// U32); a lane NARROWER than the entry would silently drop the high bits of
/// a real value, so it is refused. `Real` is exact: IEEE-754 f32 is 32 bits
/// and nothing else.
pub(crate) fn check_width(data_type: EthercatDataType, bit_length: u8) -> Result<(), String> {
    if bit_length == 0 {
        return Err("bit_length must be > 0".into());
    }
    let lane = lane_bits(data_type);
    if bit_length > lane {
        return Err(format!(
            "data_type {data_type:?} holds {lane} bits but the entry is {bit_length} bits — \
             the high bits would be dropped silently"
        ));
    }
    if matches!(data_type, EthercatDataType::Real) && bit_length != 32 {
        return Err(format!(
            "data_type Real is IEEE-754 f32, which is exactly 32 bits (entry is {bit_length})"
        ));
    }
    Ok(())
}

/// Sign-extend an `n`-bit two's-complement value held in the low bits of
/// `raw`. `n == 32` is the identity.
fn sign_extend(raw: u32, n: u8) -> i32 {
    if n >= 32 {
        return raw as i32;
    }
    let shift = 32 - n as u32;
    ((raw << shift) as i32) >> shift
}

/// Little-endian gather of the `bytes` covering a byte-aligned entry, masked
/// to `bit_length` bits. EtherCAT is little-endian and not configurable.
fn gather_le(bytes: &[u8], bit_length: u8) -> u32 {
    let mut raw = 0u32;
    for (i, b) in bytes.iter().take(4).enumerate() {
        raw |= (*b as u32) << (8 * i);
    }
    if bit_length < 32 {
        raw &= (1u32 << bit_length) - 1;
    }
    raw
}

/// Read `bit_length` bits starting at `(byte_offset, bit_offset)` from
/// `pdi` and decode according to `data_type`. Returns an `IoError` if
/// the range falls outside the PDI buffer.
pub fn read_value(
    pdi: &[u8],
    byte_offset: usize,
    bit_offset: u8,
    bit_length: u8,
    data_type: EthercatDataType,
) -> Result<ChannelValue, IoError> {
    if bit_length == 0 {
        return Err(IoError::Transport("bit_length must be > 0".into()));
    }
    let total_bits = bit_length as usize;
    let start_bit = (byte_offset * 8) + bit_offset as usize;
    let end_bit = start_bit + total_bits;
    if end_bit > pdi.len() * 8 {
        return Err(IoError::Transport(format!(
            "PDI read out of bounds: need bits {start_bit}..{end_bit}, have {} bits",
            pdi.len() * 8
        )));
    }

    check_width(data_type, bit_length).map_err(IoError::Transport)?;

    // Bool fast-path: 1 bit, masked out of the byte.
    if matches!(data_type, EthercatDataType::Bool) || bit_length == 1 {
        let byte = pdi[byte_offset];
        let bit = (byte >> bit_offset) & 1;
        return Ok(ChannelValue::Bool(bit != 0));
    }

    // Byte-aligned fast paths for 8 / 16 / 32 bit values. EtherCAT
    // permits sub-byte alignment but our config UI doesn't surface it;
    // bit_offset != 0 with bit_length > 1 is an unsupported config that
    // we surface as an error rather than silently mis-pack.
    if bit_offset != 0 {
        return Err(IoError::Transport(format!(
            "non-byte-aligned multi-bit reads are not supported (bit_length={bit_length}, bit_offset={bit_offset})",
        )));
    }

    // Only the bytes the entry actually occupies are read, and only the bits
    // within them. Decoding by the data_type's natural width instead used to
    // index past this slice and PANIC — an ESI entry whose type name is wider
    // than its BitLen (`UDINT` at 16 bits) or a 24-bit vendor entry falling
    // back to U32 both produced a channel that killed the scan thread on its
    // first read.
    let bytes_needed = bit_length.div_ceil(8) as usize;
    let slice = &pdi[byte_offset..byte_offset + bytes_needed];
    let raw = gather_le(slice, bit_length);

    Ok(match data_type {
        EthercatDataType::Bool => unreachable!("handled above"),
        EthercatDataType::U8 | EthercatDataType::U16 => ChannelValue::U16(raw as u16),
        EthercatDataType::I8 => ChannelValue::U16(sign_extend(raw, bit_length) as i16 as u16),
        EthercatDataType::I16 => ChannelValue::U16(sign_extend(raw, bit_length) as i16 as u16),
        EthercatDataType::U32 => ChannelValue::I32(raw as i32),
        EthercatDataType::I32 => ChannelValue::I32(sign_extend(raw, bit_length)),
        EthercatDataType::Real => {
            // REAL is IEEE-754 f32 on the wire; carry it as a true float
            // so fractional analog values survive (the bridge encodes to
            // VM bits per the bound variable's type). `check_width` has
            // already pinned bit_length to 32.
            ChannelValue::Real(f32::from_bits(raw))
        }
    })
}

/// Encode `value` into `pdi` at `(byte_offset, bit_offset)` for
/// `bit_length` bits, coercing to `data_type`. Returns `IoError` on
/// out-of-bounds or unsupported alignment.
pub fn write_value(
    pdi: &mut [u8],
    byte_offset: usize,
    bit_offset: u8,
    bit_length: u8,
    data_type: EthercatDataType,
    value: ChannelValue,
) -> Result<(), IoError> {
    if bit_length == 0 {
        return Err(IoError::Transport("bit_length must be > 0".into()));
    }
    let start_bit = (byte_offset * 8) + bit_offset as usize;
    let end_bit = start_bit + bit_length as usize;
    if end_bit > pdi.len() * 8 {
        return Err(IoError::Transport(format!(
            "PDI write out of bounds: need bits {start_bit}..{end_bit}, have {} bits",
            pdi.len() * 8
        )));
    }

    check_width(data_type, bit_length).map_err(IoError::Transport)?;

    // Bool / single-bit fast path.
    if matches!(data_type, EthercatDataType::Bool) || bit_length == 1 {
        let bit = match value {
            ChannelValue::Bool(b) => b as u8,
            _ => (value.to_i32() != 0) as u8,
        };
        let mask = 1u8 << bit_offset;
        let cell = &mut pdi[byte_offset];
        *cell = (*cell & !mask) | (bit << bit_offset);
        return Ok(());
    }

    if bit_offset != 0 {
        return Err(IoError::Transport(format!(
            "non-byte-aligned multi-bit writes are not supported (bit_length={bit_length}, bit_offset={bit_offset})",
        )));
    }

    let raw = match data_type {
        // IEEE-754 on the wire; the numeric view keeps the fraction when the
        // value is already a Real and converts by value otherwise.
        EthercatDataType::Real => value.to_f32().to_bits(),
        _ => value.to_i32() as u32,
    };
    let bytes_needed = bit_length.div_ceil(8) as usize;

    // Write exactly the entry's bits, masked. Storing the data_type's full
    // width instead used to index past this window and PANIC on a narrow
    // entry, and on an entry that is not a whole number of bytes (a 12-bit
    // PDO entry) it clobbered the neighbouring bits of the last byte, which
    // can belong to another channel.
    for i in 0..bytes_needed {
        let byte_mask: u8 = {
            let covered = bit_length as usize - i * 8;
            if covered >= 8 {
                0xff
            } else {
                (1u8 << covered) - 1
            }
        };
        let fresh = (raw >> (8 * i)) as u8;
        let cell = &mut pdi[byte_offset + i];
        *cell = (*cell & !byte_mask) | (fresh & byte_mask);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_bool_single_byte() {
        let mut pdi = [0u8; 1];
        write_value(
            &mut pdi,
            0,
            0,
            1,
            EthercatDataType::Bool,
            ChannelValue::Bool(true),
        )
        .unwrap();
        assert_eq!(pdi, [0b0000_0001]);
        let v = read_value(&pdi, 0, 0, 1, EthercatDataType::Bool).unwrap();
        assert_eq!(v, ChannelValue::Bool(true));
    }

    #[test]
    fn bool_bits_pack_independently_within_byte() {
        let mut pdi = [0u8; 1];
        // Set bit 0, bit 3, bit 7
        write_value(
            &mut pdi,
            0,
            0,
            1,
            EthercatDataType::Bool,
            ChannelValue::Bool(true),
        )
        .unwrap();
        write_value(
            &mut pdi,
            0,
            3,
            1,
            EthercatDataType::Bool,
            ChannelValue::Bool(true),
        )
        .unwrap();
        write_value(
            &mut pdi,
            0,
            7,
            1,
            EthercatDataType::Bool,
            ChannelValue::Bool(true),
        )
        .unwrap();
        assert_eq!(pdi, [0b1000_1001]);
        // Clear bit 3 — bits 0 and 7 must remain
        write_value(
            &mut pdi,
            0,
            3,
            1,
            EthercatDataType::Bool,
            ChannelValue::Bool(false),
        )
        .unwrap();
        assert_eq!(pdi, [0b1000_0001]);
        // Read individual bits
        assert_eq!(
            read_value(&pdi, 0, 0, 1, EthercatDataType::Bool).unwrap(),
            ChannelValue::Bool(true)
        );
        assert_eq!(
            read_value(&pdi, 0, 3, 1, EthercatDataType::Bool).unwrap(),
            ChannelValue::Bool(false)
        );
        assert_eq!(
            read_value(&pdi, 0, 7, 1, EthercatDataType::Bool).unwrap(),
            ChannelValue::Bool(true)
        );
    }

    #[test]
    fn u8_roundtrip() {
        let mut pdi = [0u8; 4];
        write_value(
            &mut pdi,
            1,
            0,
            8,
            EthercatDataType::U8,
            ChannelValue::U16(0x42),
        )
        .unwrap();
        assert_eq!(pdi, [0, 0x42, 0, 0]);
        let v = read_value(&pdi, 1, 0, 8, EthercatDataType::U8).unwrap();
        assert_eq!(v, ChannelValue::U16(0x42));
    }

    #[test]
    fn u16_little_endian() {
        let mut pdi = [0u8; 4];
        write_value(
            &mut pdi,
            0,
            0,
            16,
            EthercatDataType::U16,
            ChannelValue::U16(0x1234),
        )
        .unwrap();
        assert_eq!(pdi, [0x34, 0x12, 0, 0]);
        let v = read_value(&pdi, 0, 0, 16, EthercatDataType::U16).unwrap();
        assert_eq!(v, ChannelValue::U16(0x1234));
    }

    #[test]
    fn i16_negative_roundtrip() {
        let mut pdi = [0u8; 2];
        write_value(
            &mut pdi,
            0,
            0,
            16,
            EthercatDataType::I16,
            ChannelValue::U16(-100i16 as u16),
        )
        .unwrap();
        // -100 = 0xFF9C in two's-complement i16 (LE: 0x9C, 0xFF)
        assert_eq!(pdi, [0x9C, 0xFF]);
        let v = read_value(&pdi, 0, 0, 16, EthercatDataType::I16).unwrap();
        match v {
            ChannelValue::U16(raw) => assert_eq!(raw as i16, -100),
            _ => panic!("expected U16, got {v:?}"),
        }
    }

    #[test]
    fn i32_little_endian() {
        let mut pdi = [0u8; 6];
        write_value(
            &mut pdi,
            2,
            0,
            32,
            EthercatDataType::I32,
            ChannelValue::I32(-1),
        )
        .unwrap();
        // -1 in 32-bit LE is 0xFF * 4
        assert_eq!(pdi, [0, 0, 0xFF, 0xFF, 0xFF, 0xFF]);
        let v = read_value(&pdi, 2, 0, 32, EthercatDataType::I32).unwrap();
        assert_eq!(v, ChannelValue::I32(-1));
    }

    #[test]
    fn real_round_trips_with_fraction() {
        let mut pdi = [0u8; 4];
        // A true float keeps its fraction on the wire and back.
        write_value(
            &mut pdi,
            0,
            0,
            32,
            EthercatDataType::Real,
            ChannelValue::Real(12.7),
        )
        .unwrap();
        assert_eq!(pdi, 12.7f32.to_le_bytes());
        let v = read_value(&pdi, 0, 0, 32, EthercatDataType::Real).unwrap();
        assert_eq!(v, ChannelValue::Real(12.7));

        // An integer-lane value written to a REAL channel converts by value.
        write_value(
            &mut pdi,
            0,
            0,
            32,
            EthercatDataType::Real,
            ChannelValue::I32(42),
        )
        .unwrap();
        assert_eq!(pdi, 42.0f32.to_le_bytes());
    }

    #[test]
    fn out_of_bounds_read_errors() {
        let pdi = [0u8; 2];
        let err = read_value(&pdi, 2, 0, 8, EthercatDataType::U8).unwrap_err();
        assert!(matches!(err, IoError::Transport(_)));
    }

    #[test]
    fn out_of_bounds_write_errors() {
        let mut pdi = [0u8; 2];
        let err = write_value(
            &mut pdi,
            0,
            0,
            32,
            EthercatDataType::U32,
            ChannelValue::I32(0),
        )
        .unwrap_err();
        assert!(matches!(err, IoError::Transport(_)));
    }

    #[test]
    fn non_aligned_multi_bit_is_rejected() {
        let mut pdi = [0u8; 4];
        // 16-bit value at bit_offset=2 is unsupported
        let err = write_value(
            &mut pdi,
            0,
            2,
            16,
            EthercatDataType::U16,
            ChannelValue::U16(0),
        )
        .unwrap_err();
        assert!(matches!(err, IoError::Transport(_)));
        let err = read_value(&pdi, 0, 2, 16, EthercatDataType::U16).unwrap_err();
        assert!(matches!(err, IoError::Transport(_)));
    }

    #[test]
    fn writes_dont_clobber_adjacent_bool_bits() {
        // Simulates a typical EL1008-style digital input byte where 8 channels
        // share one byte. Writing channel 4 must leave channels 0..3, 5..7
        // untouched.
        let mut pdi = [0b1111_1111u8; 1];
        write_value(
            &mut pdi,
            0,
            4,
            1,
            EthercatDataType::Bool,
            ChannelValue::Bool(false),
        )
        .unwrap();
        assert_eq!(pdi, [0b1110_1111]);
    }

    // ---- entries whose bit_length is not the data_type's natural width ----
    //
    // Both of these used to PANIC on the first read: the decode indexed the
    // data_type's full width into a slice sized from bit_length. The trigger
    // is not exotic — `map_data_type` takes the type from the ESI *name* and
    // the length from its BitLen, with no cross-check, and its width fallback
    // sends anything over 16 bits to U32.

    /// A 24-bit vendor entry (a real shape: some encoders and analog
    /// modules) falls back to U32. Read the 24 bits that exist.
    #[test]
    fn a_24_bit_entry_read_as_u32_yields_its_own_bits() {
        let pdi = [0x78u8, 0x56, 0x34, 0xff];
        let v = read_value(&pdi, 0, 0, 24, EthercatDataType::U32).expect("24 bits fit a u32 lane");
        assert_eq!(v, ChannelValue::I32(0x345678), "the 4th byte is not ours");
    }

    /// A sloppy ESI naming an 8-bit entry `UINT` maps to U16 at bit_len 8.
    #[test]
    fn an_8_bit_entry_read_as_u16_does_not_reach_the_next_byte() {
        let pdi = [0xab_u8, 0xcd];
        let v = read_value(&pdi, 0, 0, 8, EthercatDataType::U16).expect("8 bits fit a u16 lane");
        assert_eq!(v, ChannelValue::U16(0xab));
    }

    /// Signedness follows the ENTRY's width, not the lane's: bit 23 is the
    /// sign bit of a 24-bit I32 entry.
    #[test]
    fn a_narrow_signed_entry_sign_extends_from_its_own_top_bit() {
        let pdi = [0xff_u8, 0xff, 0xff, 0x00];
        let v = read_value(&pdi, 0, 0, 24, EthercatDataType::I32).expect("24 bits fit an i32 lane");
        assert_eq!(v, ChannelValue::I32(-1));
        let pdi = [0x00_u8, 0x00, 0x80, 0x00];
        let v = read_value(&pdi, 0, 0, 24, EthercatDataType::I32).expect("in range");
        assert_eq!(
            v,
            ChannelValue::I32(-8_388_608),
            "0x800000 is the 24-bit minimum"
        );
    }

    /// The other direction is refused rather than guessed: a lane narrower
    /// than the entry would drop the high bits of a real measurement.
    #[test]
    fn a_lane_narrower_than_the_entry_is_refused() {
        let pdi = [0u8; 8];
        let err = read_value(&pdi, 0, 0, 32, EthercatDataType::U8)
            .expect_err("8-bit lane cannot hold a 32-bit entry");
        assert!(format!("{err}").contains("dropped silently"), "{err}");
        let mut pdi = [0u8; 8];
        assert!(write_value(
            &mut pdi,
            0,
            0,
            32,
            EthercatDataType::U8,
            ChannelValue::I32(1)
        )
        .is_err());
    }

    /// REAL is exact — a 16-bit IEEE-754 f32 does not exist.
    #[test]
    fn a_real_entry_that_is_not_32_bits_is_refused() {
        let pdi = [0u8; 8];
        let err = read_value(&pdi, 0, 0, 16, EthercatDataType::Real).expect_err("no 16-bit f32");
        assert!(format!("{err}").contains("exactly 32 bits"), "{err}");
    }

    /// A write to an entry that is not a whole number of bytes must leave the
    /// rest of the last byte alone — those bits can belong to another channel.
    #[test]
    fn a_partial_byte_write_preserves_the_neighbouring_bits() {
        let mut pdi = [0x00u8, 0xf0]; // high nibble of byte 1 belongs elsewhere
        write_value(
            &mut pdi,
            0,
            0,
            12,
            EthercatDataType::U16,
            ChannelValue::I32(0xabc),
        )
        .expect("12 bits fit a u16 lane");
        assert_eq!(
            pdi,
            [0xbc, 0xfa],
            "0x?a written into the low nibble, 0xf0 kept"
        );
        let v = read_value(&pdi, 0, 0, 12, EthercatDataType::U16).expect("read back");
        assert_eq!(v, ChannelValue::U16(0xabc));
    }

    /// Round-trip at the natural widths must be untouched by all of the above.
    #[test]
    fn full_width_entries_round_trip_exactly_as_before() {
        // Signed 8/16-bit entries ride the U16 lane as raw bits (the bridge
        // reinterprets per the bound variable's type), so each case names the
        // value it writes and the value it must read back.
        for (bits, ty, put, want) in [
            (
                8u8,
                EthercatDataType::U8,
                ChannelValue::I32(0xab),
                ChannelValue::U16(0xab),
            ),
            (
                16,
                EthercatDataType::U16,
                ChannelValue::I32(0xabcd),
                ChannelValue::U16(0xabcd),
            ),
            (
                16,
                EthercatDataType::I16,
                ChannelValue::U16(-2i16 as u16),
                ChannelValue::U16(0xfffe),
            ),
            (
                32,
                EthercatDataType::U32,
                ChannelValue::I32(0x1234_5678),
                ChannelValue::I32(0x1234_5678),
            ),
            (
                32,
                EthercatDataType::I32,
                ChannelValue::I32(-123_456),
                ChannelValue::I32(-123_456),
            ),
        ] {
            let mut pdi = [0u8; 8];
            write_value(&mut pdi, 1, 0, bits, ty, put).expect("write");
            let got = read_value(&pdi, 1, 0, bits, ty).expect("read");
            assert_eq!(got, want, "{ty:?} @ {bits} bits");
        }
    }
}
