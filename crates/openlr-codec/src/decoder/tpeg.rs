use crate::{CircularInterval, LinearInterval};
use crate::lrp::{LocationReference, Lrp};
use super::DecodeError;

// ── Constants ──────────────────────────────────────────────────────────────────

const TPEG_BEARING_FACTOR: f64 = 360.0 / 256.0; // 256 sectors ≈ 1.40625° each

// ── Public entry points ────────────────────────────────────────────────────────

pub fn decode_tpeg(bytes: &[u8]) -> Result<LocationReference, DecodeError> {
    // Outer container: [0x08][lengthComp][lengthAttr][version][location_type=0x00]
    //                   [inner_len][inner_attr][first_lrp_data...]
    // The Python reference validates: bytes[0] == 0x08 and len(bytes) == bytes[1] + 2.
    if bytes.len() < 7 {
        return Err(DecodeError::TooShort { min: 7, got: bytes.len() });
    }
    if bytes[0] != 0x08 {
        return Err(DecodeError::InvalidHeader(bytes[0]));
    }
    let expected_len = bytes[1] as usize + 2;
    if bytes.len() != expected_len {
        return Err(DecodeError::LengthMismatch { expected: expected_len, got: bytes.len() });
    }

    // bytes[4] = location_type; only LinearLocationReference (0x00) supported for v1.
    let location_type = bytes[4];
    if location_type != 0x00 {
        return Err(DecodeError::InvalidLocationType(location_type));
    }

    // Skip outer header (5 bytes: id + len + attrs + version + loc_type)
    // and inner LinearLocationReference header (3 bytes: id + len + attrs).
    // First LRP AbsoluteGeoCoordinate starts at byte 7.
    let mut pos = 7_usize;

    // ── First LRP (absolute coords) ──────────────────────────────────────────
    if pos + 7 > bytes.len() {
        return Err(DecodeError::TooShort { min: pos + 7, got: bytes.len() });
    }
    let lon0 = decode_abs24(bytes[pos], bytes[pos + 1], bytes[pos + 2]);
    let lat0 = decode_abs24(bytes[pos + 3], bytes[pos + 4], bytes[pos + 5]);
    pos += 7; // 3+3 coord bytes + 1 optional-altitude BitArray selector (always 0x00)

    let (bearing0, frc0, fow0, new_pos) = parse_lp(bytes, pos)?;
    pos = new_pos;
    let (lfrcnp0, dnp0, new_pos) = parse_pp(bytes, pos)?;
    pos = new_pos;

    // ── Tentative last LRP (relative to first LRP; coords re-derived below if
    //    intermediates are present) ─────────────────────────────────────────
    let last_lrp_offset = pos; // saved so we can re-parse coords relative to final intermediate

    if pos + 5 > bytes.len() {
        return Err(DecodeError::TooShort { min: pos + 5, got: bytes.len() });
    }
    let mut last_lon = decode_rel16(bytes[pos], bytes[pos + 1], lon0);
    let mut last_lat = decode_rel16(bytes[pos + 2], bytes[pos + 3], lat0);
    pos += 5; // 2+2 rel coord bytes + 1 altitude selector

    let (bearing_last, frc_last, fow_last, new_pos) = parse_lp(bytes, pos)?;
    pos = new_pos;

    // ── Selector byte (BitArray, single byte for all practical OLR messages) ──
    // Spec §A.1.4 Annex A: bit0 = has_intermediates, bit1 = has_pos_off, bit2 = has_neg_off.
    // BitArray bit numbering: bit 0 → mask 0x40, bit 1 → 0x20, bit 2 → 0x10 (TPEG2-UBCR §4.2).
    if pos >= bytes.len() {
        return Err(DecodeError::TooShort { min: pos + 1, got: bytes.len() });
    }
    let selector = bytes[pos];
    let has_intermediates = selector & 0x40 != 0;
    let has_pos_off       = selector & 0x20 != 0;
    let has_neg_off       = selector & 0x10 != 0;
    pos += 1;

    // ── Intermediates ─────────────────────────────────────────────────────────
    let mut intermediates: Vec<Lrp> = Vec::new();
    if has_intermediates {
        let (n, new_pos) = decode_mb(bytes, pos)?;
        pos = new_pos;
        let mut prev_lon = lon0;
        let mut prev_lat = lat0;
        for _ in 0..n {
            if pos + 5 > bytes.len() {
                return Err(DecodeError::TooShort { min: pos + 5, got: bytes.len() });
            }
            let lon = decode_rel16(bytes[pos], bytes[pos + 1], prev_lon);
            let lat = decode_rel16(bytes[pos + 2], bytes[pos + 3], prev_lat);
            pos += 5; // 2+2 rel coords + 1 altitude selector
            let (bearing, frc, fow, new_pos) = parse_lp(bytes, pos)?;
            pos = new_pos;
            let (lfrcnp, dnp, new_pos) = parse_pp(bytes, pos)?;
            pos = new_pos;
            intermediates.push(Lrp {
                coord:      (lon, lat),
                bearing,
                frc,
                fow,
                lfrcnp:     Some(lfrcnp),
                dnp:        Some(dnp),
                pos_offset: None,
                neg_offset: None,
            });
            prev_lon = lon;
            prev_lat = lat;
        }
        // Re-derive last LRP coords relative to the final intermediate's coords.
        last_lon = decode_rel16(bytes[last_lrp_offset],     bytes[last_lrp_offset + 1], prev_lon);
        last_lat = decode_rel16(bytes[last_lrp_offset + 2], bytes[last_lrp_offset + 3], prev_lat);
    }

    // ── Offsets (TPEG: exact meters, represented as LinearInterval::point) ────
    let pos_offset = if has_pos_off {
        let (m, new_pos) = decode_mb(bytes, pos)?;
        pos = new_pos;
        Some(LinearInterval::point(m as f64))
    } else {
        None
    };
    let neg_offset = if has_neg_off {
        let (m, _) = decode_mb(bytes, pos)?;
        Some(LinearInterval::point(m as f64))
    } else {
        None
    };

    // ── Assemble ──────────────────────────────────────────────────────────────
    let mut lrps = Vec::with_capacity(2 + intermediates.len());
    lrps.push(Lrp {
        coord:      (lon0, lat0),
        bearing:    bearing0,
        frc:        frc0,
        fow:        fow0,
        lfrcnp:     Some(lfrcnp0),
        dnp:        Some(dnp0),
        pos_offset,
        neg_offset: None,
    });
    lrps.extend(intermediates);
    lrps.push(Lrp {
        coord:      (last_lon, last_lat),
        bearing:    bearing_last,
        frc:        frc_last,
        fow:        fow_last,
        lfrcnp:     None,
        dnp:        None,
        pos_offset: None,
        neg_offset,
    });

    Ok(LocationReference { lrps })
}

pub fn decode_tpeg_hex(s: &str) -> Result<LocationReference, DecodeError> {
    let bytes = from_hex(s)?;
    decode_tpeg(&bytes)
}

// ── Byte-level helpers ─────────────────────────────────────────────────────────

/// AbsoluteGeoCoordinate (IntSi24, big-endian). Same formula as OpenLR v3 (both follow
/// whitepaper §6.5.2 / Eq 2).
fn decode_abs24(hi: u8, mi: u8, lo: u8) -> f64 {
    crate::decoder::v3::decode_abs_coord(hi, mi, lo)
}

/// RelativeGeoCoordinate (IntSiLi: big-endian signed 16-bit), decamicrodegree units.
fn decode_rel16(hi: u8, lo: u8, prev: f64) -> f64 {
    let i = ((hi as u16) << 8 | lo as u16) as i16;
    prev + i as f64 / 100_000.0
}

/// TPEG Bearing (IntUnTi, 256 sectors): sector v → CircularInterval.
/// Spec §8.4: full circle divided into 256 sectors, precision 360/256°.
fn bearing_to_interval(sector: u8) -> CircularInterval {
    let v = sector as f64;
    CircularInterval {
        lb_deg: (v * TPEG_BEARING_FACTOR).round(),
        ub_deg: ((v + 1.0) * TPEG_BEARING_FACTOR).round(),
    }
}

/// IntUnLoMB: big-endian MSB-continuation varint (TPEG2-UBCR §4.2).
/// MSB of each byte is a continuation flag; 7 payload bits per byte, MSB-first.
fn decode_mb(bytes: &[u8], pos: usize) -> Result<(u64, usize), DecodeError> {
    let mut p = pos;
    let mut result: u64 = 0;
    loop {
        if p >= bytes.len() {
            return Err(DecodeError::TooShort { min: p + 1, got: bytes.len() });
        }
        let b = bytes[p];
        result = (result << 7) | (b & 0x7F) as u64;
        p += 1;
        if b & 0x80 == 0 {
            break;
        }
    }
    Ok((result, p))
}

/// Parse a LineProperties component (id = 0x09).
/// Layout: [0x09][lp_len][lp_attr][frc][fow][bearing][selector][opt srBL...][opt srBR...]
/// Advances by lp_len + 2 bytes total.
fn parse_lp(bytes: &[u8], pos: usize) -> Result<(CircularInterval, u8, u8, usize), DecodeError> {
    if pos >= bytes.len() || bytes[pos] != 0x09 {
        return Err(DecodeError::InvalidComponent {
            expected: 0x09,
            got: bytes.get(pos).copied().unwrap_or(0),
        });
    }
    if pos + 1 >= bytes.len() {
        return Err(DecodeError::TooShort { min: pos + 2, got: bytes.len() });
    }
    let lp_len = bytes[pos + 1] as usize;
    let end = pos + lp_len + 2;
    if end > bytes.len() {
        return Err(DecodeError::TooShort { min: end, got: bytes.len() });
    }
    if pos + 5 >= bytes.len() {
        return Err(DecodeError::TooShort { min: pos + 6, got: bytes.len() });
    }
    let frc     = bytes[pos + 3];
    let fow     = bytes[pos + 4];
    let bearing = bearing_to_interval(bytes[pos + 5]);
    Ok((bearing, frc, fow, end))
}

/// Parse a PathProperties component (id = 0x0A).
/// Layout: [0x0A][pp_len][pp_attr][lfrcnp][dnp_varint...][selector]
/// Advances by pp_len + 2 bytes total.
fn parse_pp(bytes: &[u8], pos: usize) -> Result<(u8, LinearInterval, usize), DecodeError> {
    if pos >= bytes.len() || bytes[pos] != 0x0A {
        return Err(DecodeError::InvalidComponent {
            expected: 0x0A,
            got: bytes.get(pos).copied().unwrap_or(0),
        });
    }
    if pos + 1 >= bytes.len() {
        return Err(DecodeError::TooShort { min: pos + 2, got: bytes.len() });
    }
    let pp_len = bytes[pos + 1] as usize;
    let end = pos + pp_len + 2;
    if end > bytes.len() {
        return Err(DecodeError::TooShort { min: end, got: bytes.len() });
    }
    if pos + 3 >= bytes.len() {
        return Err(DecodeError::TooShort { min: pos + 4, got: bytes.len() });
    }
    let lfrcnp = bytes[pos + 3];
    let (dnp_raw, _) = decode_mb(bytes, pos + 4)?;
    Ok((lfrcnp, LinearInterval::point(dnp_raw as f64), end))
}

/// Decode a lowercase or uppercase hex string to bytes.
fn from_hex(s: &str) -> Result<Vec<u8>, DecodeError> {
    if s.len() % 2 != 0 {
        return Err(DecodeError::Hex("odd-length hex string".to_string()));
    }
    (0..s.len() / 2)
        .map(|i| {
            u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|e| DecodeError::Hex(e.to_string()))
        })
        .collect()
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Primitive helpers ────────────────────────────────────────────────────

    #[test]
    fn decode_mb_single_byte() {
        let (v, off) = decode_mb(&[0x7F], 0).unwrap();
        assert_eq!(v, 127);
        assert_eq!(off, 1);
    }

    #[test]
    fn decode_mb_five_bytes() {
        let ba = [0x84_u8, 0x89, 0xBA, 0x89, 0x11];
        let (v, off) = decode_mb(&ba, 0).unwrap();
        assert_eq!(v, 1_093_567_633);
        assert_eq!(off, 5);
    }

    #[test]
    fn abs24_first_lrp_lon() {
        // Byte sequence from test_linear_location test vector
        let (v, _) = (decode_abs24(0x04, 0x12, 0x17), ());
        assert!((v - 5.724_359_750_747_681).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn bearing_sector_33() {
        let i = bearing_to_interval(33);
        assert!((i.lb_deg - 46.0).abs() < 1e-9);
        assert!((i.ub_deg - 48.0).abs() < 1e-9);
    }

    #[test]
    fn bearing_sector_11() {
        let i = bearing_to_interval(11);
        assert!((i.lb_deg - 15.0).abs() < 1e-9);
        assert!((i.ub_deg - 17.0).abs() < 1e-9);
    }

    // ── Integration: 2-LRP with both offsets ────────────────────────────────
    // Python test_linear_location vector.
    #[test]
    fn tpeg_two_lrp_both_offsets() {
        let loc = decode_tpeg_hex(
            "0829011000252404121724D5C800090504060321000A050406825F00FFF300030009050406030B00300F77",
        )
        .unwrap();

        assert_eq!(loc.lrps.len(), 2);

        let l0 = &loc.lrps[0];
        assert!((l0.coord.0 - 5.724_359_750_747_681).abs() < 1e-9);
        assert!((l0.coord.1 - 51.799_324_750_900_27).abs() < 1e-9);
        assert_eq!(l0.frc, 6);
        assert_eq!(l0.fow, 3);
        assert!((l0.bearing.lb_deg - 46.0).abs() < 1e-9);
        assert!((l0.bearing.ub_deg - 48.0).abs() < 1e-9);
        assert_eq!(l0.lfrcnp, Some(6));
        assert!(l0.dnp.is_some());
        assert!((l0.dnp.unwrap().lb - 351.0).abs() < 1e-9);
        assert_eq!(l0.pos_offset.map(|i| i.lb as u64), Some(15));
        assert!(l0.neg_offset.is_none());

        let l1 = &loc.lrps[1];
        assert!((l1.coord.0 - 5.724_229_750_747_68).abs() < 1e-9);
        assert!((l1.coord.1 - 51.799_354_750_900_27).abs() < 1e-9);
        assert_eq!(l1.frc, 6);
        assert_eq!(l1.fow, 3);
        assert!((l1.bearing.lb_deg - 15.0).abs() < 1e-9);
        assert!((l1.bearing.ub_deg - 17.0).abs() < 1e-9);
        assert!(l1.lfrcnp.is_none());
        assert!(l1.dnp.is_none());
        assert!(l1.pos_offset.is_none());
        assert_eq!(l1.neg_offset.map(|i| i.lb as u64), Some(119));
    }

    // ── Integration: 4-LRP with 2 intermediates and both offsets ────────────
    // Python test_intermediates vector.
    #[test]
    fn tpeg_four_lrp_two_intermediates() {
        let loc = decode_tpeg_hex(
            "08510110004D4C083CE62242730009050401023E000A0504018567000148F9A1000905040102FC007002038EFF1900090504010257000A05040198480006E7F7D400090504010258000A0504018F3100834655",
        )
        .unwrap();

        assert_eq!(loc.lrps.len(), 4);

        let l0 = &loc.lrps[0];
        assert!((l0.coord.0 - 11.584_514_379_501_343).abs() < 1e-9);
        assert!((l0.coord.1 - 48.177_505_731_582_64).abs() < 1e-9);
        assert_eq!(l0.frc, 1);
        assert_eq!(l0.fow, 2);
        assert!((l0.bearing.lb_deg - 87.0).abs() < 1e-9);
        assert!((l0.bearing.ub_deg - 89.0).abs() < 1e-9);
        assert_eq!(l0.lfrcnp, Some(1));
        assert_eq!(l0.dnp.map(|i| i.lb as u64), Some(743));

        let l1 = &loc.lrps[1];
        assert!((l1.coord.0 - 11.593_614_379_501_343).abs() < 1e-9);
        assert!((l1.coord.1 - 48.175_195_731_582_64).abs() < 1e-9);
        assert_eq!(l1.frc, 1);
        assert_eq!(l1.fow, 2);
        assert!((l1.bearing.lb_deg - 122.0).abs() < 1e-9);
        assert!((l1.bearing.ub_deg - 124.0).abs() < 1e-9);
        assert_eq!(l1.lfrcnp, Some(1));
        assert_eq!(l1.dnp.map(|i| i.lb as u64), Some(3144));

        let l2 = &loc.lrps[2];
        assert!((l2.coord.0 - 11.611_284_379_501_344).abs() < 1e-9);
        assert!((l2.coord.1 - 48.154_275_731_582_64).abs() < 1e-9);
        assert_eq!(l2.frc, 1);
        assert_eq!(l2.fow, 2);
        assert!((l2.bearing.lb_deg - 124.0).abs() < 1e-9);
        assert!((l2.bearing.ub_deg - 125.0).abs() < 1e-9);
        assert_eq!(l2.lfrcnp, Some(1));
        assert_eq!(l2.dnp.map(|i| i.lb as u64), Some(1969));

        let l3 = &loc.lrps[3];
        assert!((l3.coord.0 - 11.614_564_379_501_344).abs() < 1e-9);
        assert!((l3.coord.1 - 48.137_965_731_582_646).abs() < 1e-9);
        assert_eq!(l3.frc, 1);
        assert_eq!(l3.fow, 2);
        assert!((l3.bearing.lb_deg - 354.0).abs() < 1e-9);
        assert!((l3.bearing.ub_deg - 356.0).abs() < 1e-9);
        assert!(l3.lfrcnp.is_none());
        assert!(l3.dnp.is_none());

        assert_eq!(loc.lrps[0].pos_offset.map(|i| i.lb as u64), Some(454));
        assert_eq!(loc.lrps[3].neg_offset.map(|i| i.lb as u64), Some(85));
    }

    // ── Error paths ──────────────────────────────────────────────────────────

    #[test]
    fn too_short_rejected() {
        assert!(matches!(
            decode_tpeg(&[0x08, 0x05, 0x00, 0x10, 0x00, 0x00, 0x00]),
            Err(DecodeError::TooShort { .. })
        ));
    }

    #[test]
    fn wrong_container_id_rejected() {
        let mut b = vec![0x07_u8; 8];
        b[1] = 6;
        assert!(matches!(decode_tpeg(&b), Err(DecodeError::InvalidHeader(_))));
    }

    #[test]
    fn length_mismatch_rejected() {
        // Real vector with corrupted length byte
        let mut ba = hex_bytes(
            "0829011000252404121724D5C800090504060321000A050406825F00FFF300030009050406030B00300F77",
        );
        ba[1] = 0x2A; // says 44 bytes but we have 43
        assert!(matches!(decode_tpeg(&ba), Err(DecodeError::LengthMismatch { .. })));
    }

    #[test]
    fn unknown_location_type_rejected() {
        let mut ba = hex_bytes(
            "0829011000252404121724D5C800090504060321000A050406825F00FFF300030009050406030B00300F77",
        );
        ba[4] = 0x0A; // not LinearLocationReference
        assert!(matches!(decode_tpeg(&ba), Err(DecodeError::InvalidLocationType(_))));
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        from_hex(s).unwrap()
    }
}
