use base64::{Engine as _, engine::general_purpose::STANDARD as B64};

use crate::{CircularInterval, LinearInterval};
use crate::lrp::{LocationReference, Lrp};
use super::DecodeError;

// ── Constants ──────────────────────────────────────────────────────────────────

const BEARING_SECTOR_DEG: f64 = 360.0 / 32.0;  // 11.25°
const DNP_BUCKET_M: f64       = 15_000.0 / 256.0; // ≈58.59375 m

// ── Public entry points ────────────────────────────────────────────────────────

pub fn decode_v3(bytes: &[u8]) -> Result<LocationReference, DecodeError> {
    // Minimum: header(1) + first LRP(9) + last LRP(6) = 16 bytes.
    if bytes.len() < 16 {
        return Err(DecodeError::TooShort { min: 16, got: bytes.len() });
    }

    // Byte 0: status.  Bits 2-0 must be 3 (v3); bits 6-3 encode location type.
    let status = bytes[0];
    if status & 0x07 != 3 {
        return Err(DecodeError::InvalidHeader(status));
    }

    // Remainder must be: 9 + 7*(n-2) + 6 + [0,1,2] extra bytes.
    // n = bytes.len() / 7  (integer division);  offsets = (bytes.len()-16) % 7.
    let n_lrps = bytes.len() / 7;
    if n_lrps < 2 {
        return Err(DecodeError::TooShort { min: 16, got: bytes.len() });
    }
    let n_offsets = (bytes.len().wrapping_sub(16)) % 7;
    if n_offsets > 2 {
        return Err(DecodeError::TrailingBytes(n_offsets));
    }

    // ── First LRP (absolute coords) ──────────────────────────────────────────
    let lon0 = decode_abs_coord(bytes[1], bytes[2], bytes[3]);
    let lat0 = decode_abs_coord(bytes[4], bytes[5], bytes[6]);
    let (frc0, fow0)        = decode_attr1(bytes[7]);
    let (lfrcnp0, bearing0) = decode_attr2(bytes[8]);
    let dnp0                = decode_dnp(bytes[9]);

    let first_lrp = Lrp {
        coord:      (lon0, lat0),
        bearing:    bearing0,
        frc:        frc0,
        fow:        fow0,
        lfrcnp:     Some(lfrcnp0),
        dnp:        Some(dnp0),
        pos_offset: None,
        neg_offset: None,
    };

    let mut lrps: Vec<Lrp> = vec![first_lrp];

    // ── Intermediate LRPs (relative coords) ─────────────────────────────────
    let mut pos = 10_usize;
    for _ in 0..n_lrps - 2 {
        let (prev_lon, prev_lat) = lrps.last().map(|l| l.coord).unwrap();
        let lon = decode_rel_coord(bytes[pos],     bytes[pos + 1], prev_lon);
        let lat = decode_rel_coord(bytes[pos + 2], bytes[pos + 3], prev_lat);
        let (frc, fow)       = decode_attr1(bytes[pos + 4]);
        let (lfrcnp, bearing) = decode_attr2(bytes[pos + 5]);
        let dnp              = decode_dnp(bytes[pos + 6]);
        lrps.push(Lrp {
            coord:      (lon, lat),
            bearing,
            frc,
            fow,
            lfrcnp:     Some(lfrcnp),
            dnp:        Some(dnp),
            pos_offset: None,
            neg_offset: None,
        });
        pos += 7;
    }

    // ── Last LRP ─────────────────────────────────────────────────────────────
    let (prev_lon, prev_lat) = lrps.last().map(|l| l.coord).unwrap();
    let lon_last = decode_rel_coord(bytes[pos],     bytes[pos + 1], prev_lon);
    let lat_last = decode_rel_coord(bytes[pos + 2], bytes[pos + 3], prev_lat);
    let (frc_last, fow_last)              = decode_attr1(bytes[pos + 4]);
    let (has_pos_off, has_neg_off, brng)  = decode_attr4(bytes[pos + 5]);
    // pos+6 is the start of the optional offset bytes

    lrps.push(Lrp {
        coord:      (lon_last, lat_last),
        bearing:    brng,
        frc:        frc_last,
        fow:        fow_last,
        lfrcnp:     None,
        dnp:        None,
        pos_offset: None,
        neg_offset: None,
    });

    // ── Offsets (spec §7.5.2, Equation 8) ────────────────────────────────────
    // Positive offset is a fraction of the FIRST leg's DNP (LRP-0 → LRP-1).
    // Negative offset is a fraction of the LAST leg's DNP (LRP-(n-2) → LRP-(n-1)).
    // "LRP length" = DNP of the respective leg, not the total path length.
    let first_dnp = lrps[0].dnp.unwrap();
    let last_leg_dnp = lrps[lrps.len() - 2].dnp.unwrap(); // penultimate LRP's DNP

    let offset_start = pos + 6;
    let mut off_idx = offset_start;

    if has_pos_off {
        let raw = bytes[off_idx] as f64;
        lrps[0].pos_offset = Some(LinearInterval {
            lb: raw / 256.0 * first_dnp.lb,
            ub: (raw + 1.0) / 256.0 * first_dnp.ub,
        });
        off_idx += 1;
    }
    if has_neg_off {
        let raw = bytes[off_idx] as f64;
        let last = lrps.len() - 1;
        lrps[last].neg_offset = Some(LinearInterval {
            lb: raw / 256.0 * last_leg_dnp.lb,
            ub: (raw + 1.0) / 256.0 * last_leg_dnp.ub,
        });
    }

    Ok(LocationReference { lrps })
}

pub fn decode_v3_base64(s: &str) -> Result<LocationReference, DecodeError> {
    let bytes = B64.decode(s).map_err(|e| DecodeError::Base64(e.to_string()))?;
    decode_v3(&bytes)
}

// ── Byte-level helpers ─────────────────────────────────────────────────────────

/// Decode a big-endian signed 24-bit integer to WGS84 degrees.
/// Formula (OpenLR whitepaper §8): deg = (i − sgn(i)·0.5) × 360 / 2^24
pub fn decode_abs_coord(hi: u8, mi: u8, lo: u8) -> f64 {
    let u = (hi as u32) << 16 | (mi as u32) << 8 | lo as u32;
    let i = if u >= 0x80_0000 { u as i32 - 0x100_0000 } else { u as i32 } as f64;
    let half_sgn = if i > 0.0 { 0.5 } else if i < 0.0 { -0.5 } else { 0.0 };
    (i - half_sgn) * 360.0 / 16_777_216.0
}

/// Decode a big-endian signed 16-bit relative offset to degrees.
pub fn decode_rel_coord(hi: u8, lo: u8, prev: f64) -> f64 {
    let i = ((hi as u16) << 8 | lo as u16) as i16;
    prev + i as f64 / 100_000.0
}

/// Attr1 byte: bits[5:3] = FRC, bits[2:0] = FOW.
fn decode_attr1(b: u8) -> (u8, u8) {
    ((b >> 3) & 0x07, b & 0x07)
}

/// Attr2 byte (non-last LRP): bits[7:5] = LFRCNP, bits[4:0] = bearing sector.
fn decode_attr2(b: u8) -> (u8, CircularInterval) {
    let lfrcnp = (b >> 5) & 0x07;
    let sector = b & 0x1F;
    (lfrcnp, bearing_sector_to_interval(sector))
}

/// Attr4 byte (last LRP): bit6 = pos-offset flag, bit5 = neg-offset flag,
/// bits[4:0] = bearing sector.
fn decode_attr4(b: u8) -> (bool, bool, CircularInterval) {
    let has_pos = b & 0x40 != 0;
    let has_neg = b & 0x20 != 0;
    let sector  = b & 0x1F;
    (has_pos, has_neg, bearing_sector_to_interval(sector))
}

/// Convert a 5-bit sector (0–31) to a [LB, UB] CircularInterval.
pub fn bearing_sector_to_interval(sector: u8) -> CircularInterval {
    CircularInterval {
        lb_deg: sector as f64 * BEARING_SECTOR_DEG,
        ub_deg: (sector as f64 + 1.0) * BEARING_SECTOR_DEG,
    }
}

/// Convert the 1-byte DNP raw value to a LinearInterval (meters).
pub fn decode_dnp(raw: u8) -> LinearInterval {
    LinearInterval {
        lb: raw as f64 * DNP_BUCKET_M,
        ub: (raw as f64 + 1.0) * DNP_BUCKET_M,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // rustlr test_decode1: "C/+zGCZJgyuvBAAh/x8rHw=="
    // 2 LRPs, no offsets; 16 bytes.
    #[test]
    fn v3_two_lrp_no_offsets() {
        let loc = decode_v3_base64("C/+zGCZJgyuvBAAh/x8rHw==").unwrap();
        assert_eq!(loc.lrps.len(), 2);
        assert!(loc.lrps[0].pos_offset.is_none());
        assert!(loc.lrps[1].neg_offset.is_none());
        assert!(loc.lrps[0].dnp.is_some());
        assert!(loc.lrps[1].dnp.is_none());
        assert!(loc.lrps[0].lfrcnp.is_some());
        assert!(loc.lrps[1].lfrcnp.is_none());
    }

    // rustlr test_decode4: "C/7VOCaEbSu/BP+5AMUrbJEQ"
    // 2 LRPs, both offsets; 18 bytes.
    #[test]
    fn v3_two_lrp_both_offsets() {
        let loc = decode_v3_base64("C/7VOCaEbSu/BP+5AMUrbJEQ").unwrap();
        assert_eq!(loc.lrps.len(), 2);
        assert!(loc.lrps[0].pos_offset.is_some());
        assert!(loc.lrps[1].neg_offset.is_some());
    }

    // "C/4bnSaa4yu5Af91ACAruQT+r/+9Kwc=" — 3 LRPs, no offsets; 23 bytes.
    #[test]
    fn v3_three_lrp_no_offsets() {
        let loc = decode_v3_base64("C/4bnSaa4yu5Af91ACAruQT+r/+9Kwc=").unwrap();
        assert_eq!(loc.lrps.len(), 3);
        assert!(loc.lrps[1].dnp.is_some());   // intermediate has DNP
        assert!(loc.lrps[2].dnp.is_none());   // last does not
    }

    #[test]
    fn abs_coord_round_trip() {
        // lon ≈ 13.41 (Berlin)
        let lon = 13.41_f64;
        let u = ((lon * 16_777_216.0 / 360.0 + 0.5).round() as i32).clamp(-8_388_608, 8_388_607);
        let encoded = u.to_be_bytes();
        let decoded = decode_abs_coord(encoded[1], encoded[2], encoded[3]);
        // Theoretical max error = ½ LSB = 360/2^24/2 ≈ 1.07e-5°.
        assert!((decoded - lon).abs() < 2e-5, "delta={}", decoded - lon);
    }

    #[test]
    fn bearing_sector_0() {
        let i = bearing_sector_to_interval(0);
        assert_eq!(i.lb_deg, 0.0);
        assert!((i.ub_deg - 11.25).abs() < 1e-10);
    }

    #[test]
    fn bearing_sector_15() {
        let i = bearing_sector_to_interval(15);
        assert!((i.lb_deg - 168.75).abs() < 1e-10);
        assert!((i.ub_deg - 180.0).abs() < 1e-10);
    }

    #[test]
    fn dnp_bucket_bounds() {
        let d = decode_dnp(0);
        assert_eq!(d.lb, 0.0);
        assert!((d.ub - 15_000.0 / 256.0).abs() < 1e-9);
        let d255 = decode_dnp(255);
        assert!((d255.ub - 15_000.0).abs() < 1e-6);
    }

    // Pinned coordinate values computed from raw bytes, cross-checked against
    // the whitepaper §8 formula: deg = (i − sgn(i)·0.5) × 360 / 2^24.
    #[test]
    fn v3_two_lrp_coord_values() {
        let loc = decode_v3_base64("C/+zGCZJgyuvBAAh/x8rHw==").unwrap();
        let lrp0 = &loc.lrps[0];
        let lrp1 = &loc.lrps[1];

        assert!((lrp0.coord.0 - -0.422_448).abs() < 1e-5, "lon0={}", lrp0.coord.0);
        assert!((lrp0.coord.1 -  53.841_301).abs() < 1e-5, "lat0={}", lrp0.coord.1);
        assert_eq!(lrp0.frc, 5);
        assert_eq!(lrp0.fow, 3);
        assert_eq!(lrp0.lfrcnp, Some(5));
        let b0 = lrp0.bearing.clone();
        assert!((b0.lb_deg - 168.75).abs() < 1e-9, "bearing lb={}", b0.lb_deg);
        assert!((b0.ub_deg - 180.0 ).abs() < 1e-9, "bearing ub={}", b0.ub_deg);
        let d0 = lrp0.dnp.as_ref().unwrap();
        assert!((d0.lb - 234.375  ).abs() < 1e-6, "dnp lb={}", d0.lb);
        assert!((d0.ub - 292.968_75).abs() < 1e-6, "dnp ub={}", d0.ub);

        assert!((lrp1.coord.0 - -0.422_118).abs() < 1e-5, "lon1={}", lrp1.coord.0);
        assert!((lrp1.coord.1 -  53.839_051).abs() < 1e-5, "lat1={}", lrp1.coord.1);
        assert_eq!(lrp1.frc, 5);
        assert_eq!(lrp1.fow, 3);
        let b1 = lrp1.bearing.clone();
        assert!((b1.lb_deg - 348.75).abs() < 1e-9, "bearing lb={}", b1.lb_deg);
        assert!((b1.ub_deg - 360.0 ).abs() < 1e-9, "bearing ub={}", b1.ub_deg);
        assert!(lrp1.dnp.is_none());
        assert!(lrp1.pos_offset.is_none());
        assert!(lrp1.neg_offset.is_none());
    }

    #[test]
    fn too_short_rejected() {
        assert!(matches!(decode_v3(&[0x0B; 15]), Err(DecodeError::TooShort { .. })));
    }

    #[test]
    fn bad_version_rejected() {
        // status byte with version != 3
        let mut b = vec![0x0B_u8; 16];
        b[0] = 0x0C; // version = 4
        assert!(matches!(decode_v3(&b), Err(DecodeError::InvalidHeader(_))));
    }
}
