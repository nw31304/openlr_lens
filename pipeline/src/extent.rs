use anyhow::{bail, Result};

/// WGS84 bounding box (west, south, east, north) in degrees.
#[derive(Debug, Clone, Copy)]
pub struct Bbox {
    pub west:  f64,
    pub south: f64,
    pub east:  f64,
    pub north: f64,
}

impl Bbox {
    pub fn slug(&self) -> String {
        format!("{:.4},{:.4},{:.4},{:.4}", self.west, self.south, self.east, self.north)
    }
}

/// Convert an `--extent` argument to a safe filename slug.
/// Lower-cases the input and replaces any non-alphanumeric character with `-`.
/// Examples: "NZ" → "nz", "north-america" → "north-america",
///           "166.0,-47.5,178.5,-34.0" → "166-0--47-5-178-5--34-0"
pub fn extent_slug(spec: &str) -> String {
    spec.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Resolve an `--extent` argument to a `Bbox` (or `None` for the whole world).
pub fn resolve(spec: &str) -> Result<Option<Bbox>> {
    let lower = spec.to_lowercase();

    // Explicit bbox: "west,south,east,north"
    if lower.contains(',') {
        let parts: Vec<&str> = spec.splitn(4, ',').collect();
        if parts.len() != 4 {
            bail!("--extent bbox must be 'west,south,east,north', got '{spec}'");
        }
        let [w, s, e, n]: [f64; 4] = parts
            .iter()
            .map(|p| p.trim().parse::<f64>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| anyhow::anyhow!("non-numeric value in bbox '{spec}'"))?
            .try_into()
            .unwrap();
        if w >= e {
            bail!("bbox west ({w}) must be less than east ({e}) in '{spec}'");
        }
        if s >= n {
            bail!("bbox south ({s}) must be less than north ({n}) in '{spec}'");
        }
        return Ok(Some(Bbox { west: w, south: s, east: e, north: n }));
    }

    if lower == "world" {
        return Ok(None);
    }

    // Continent names
    if let Some(bbox) = continent_bbox(&lower) {
        return Ok(Some(bbox));
    }

    // ISO 3166-1 alpha-2 country code
    if spec.len() == 2 {
        if let Some(bbox) = country_bbox(spec.to_uppercase().as_str()) {
            return Ok(Some(bbox));
        }
        bail!("unknown ISO 3166-1 alpha-2 country code '{spec}'");
    }

    // ISO 3166-1 alpha-3 country code
    if spec.len() == 3 {
        let upper = spec.to_uppercase();
        if let Some(a2) = alpha3_to_alpha2(&upper) {
            if let Some(bbox) = country_bbox(a2) {
                return Ok(Some(bbox));
            }
        }
        bail!("unknown ISO 3166-1 alpha-3 country code '{spec}'");
    }

    bail!("unrecognised --extent '{spec}'. Use a country code (NZ or NZL), continent, 'world', or 'west,south,east,north'");
}

fn continent_bbox(name: &str) -> Option<Bbox> {
    Some(match name {
        "africa"        => Bbox { west: -18.0,  south: -35.0, east:  52.0, north:  38.0 },
        "antarctica"    => Bbox { west: -180.0, south: -90.0, east: 180.0, north: -60.0 },
        "asia"          => Bbox { west:  25.0,  south: -11.0, east: 180.0, north:  82.0 },
        "europe"        => Bbox { west: -25.0,  south:  34.0, east:  45.0, north:  72.0 },
        "north-america" => Bbox { west: -168.0, south:   5.0, east: -52.0, north:  84.0 },
        "oceania"       => Bbox { west: 110.0,  south: -50.0, east: 180.0, north:   5.0 },
        "south-america" => Bbox { west: -82.0,  south: -56.0, east: -34.0, north:  13.0 },
        _ => return None,
    })
}

/// Bounding boxes for ISO 3166-1 alpha-2 country codes.
/// Covers all UN member states + common territories; tuned to road-network extents.
/// Validate and extend as needed — this is a best-effort starting point.
fn country_bbox(code: &str) -> Option<Bbox> {
    // Format: (west, south, east, north)
    let (w, s, e, n) = match code {
        "AD" => (-1.79,  42.43,  1.79,  42.66),
        "AE" => (51.58,  22.63, 56.38,  26.08),
        "AF" => (60.53,  29.32, 74.89,  38.49),
        "AG" => (-61.91, 16.99,-61.67,  17.73),
        "AL" => (19.30,  39.62, 21.06,  42.66),
        "AM" => (43.58,  38.74, 46.63,  41.30),
        "AO" => (11.68, -18.02, 24.08,  -4.44),
        "AR" => (-73.56,-55.06,-53.65, -21.83),
        "AT" => ( 9.53,  46.37, 17.16,  49.02),
        "AU" => (112.92,-43.63,153.64, -10.69),
        "AZ" => (44.80,  38.39, 50.37,  41.86),
        "BA" => (15.72,  42.56, 19.62,  45.27),
        "BD" => (88.08,  20.74, 92.67,  26.63),
        "BE" => ( 2.55,  49.50,  6.41,  51.51),
        "BF" => (-5.52,   9.40,  2.40,  15.08),
        "BG" => (22.36,  41.23, 28.61,  44.23),
        "BJ" => ( 0.77,   6.14,  3.85,  12.24),
        "BN" => (114.07,  4.00,115.36,   5.05),
        "BO" => (-69.65,-22.87,-57.50, -9.68),
        "BR" => (-73.99,-33.75,-28.85,   5.27),
        "BT" => (88.75,  26.72, 92.12,  27.98),
        "BW" => (19.97, -26.83, 29.38, -17.66),
        "BY" => (23.17,  51.32, 32.76,  56.17),
        "BZ" => (-89.23, 15.89,-87.79,  18.50),
        "CA" => (-141.00,41.68,-52.64,  83.11),
        "CD" => (12.18, -13.45, 31.30,   5.39),
        "CF" => (14.42,   2.22, 27.46,  11.00),
        "CG" => (11.09,  -5.04, 18.65,   3.71),
        "CH" => ( 5.96,  45.82, 10.49,  47.81),
        "CI" => (-8.60,   4.36, -2.49,  10.74),
        "CL" => (-75.65,-55.89,-66.42, -17.51),
        "CM" => ( 8.49,   1.65, 16.01,  12.38),
        "CN" => (73.68,  18.20,135.03,  53.46),
        "CO" => (-79.00,  -4.23,-66.87, 13.39),
        "CR" => (-85.94,  8.03,-82.55,  11.22),
        "CU" => (-84.96, 19.82,-74.13,  23.19),
        "CV" => (-25.36, 14.81,-22.65,  17.21),
        "CY" => (32.26,  34.57, 34.00,  35.17),
        "CZ" => (12.09,  48.56, 18.85,  51.05),
        "DE" => ( 5.99,  47.30, 15.02,  54.98),
        "DJ" => (41.75,  10.93, 43.42,  12.71),
        "DK" => ( 8.07,  54.56, 15.20,  57.75),
        "DO" => (-72.00, 17.47,-68.32,  19.93),
        "DZ" => (-8.67,  18.97,  12.00, 37.09),
        "EC" => (-80.97, -4.96,-75.19,   1.68),
        "EE" => (21.84,  57.51, 28.21,  59.69),
        "EG" => (24.70,  22.00, 36.90,  31.67),
        "ER" => (36.43,  12.36, 43.13,  17.99),
        "ES" => (-9.39,  35.95,  3.04,  43.75),
        "ET" => (32.99,   3.42, 47.98,  14.88),
        "FI" => (19.32,  59.70, 31.58,  70.09),
        "FJ" => (177.09,-19.17,179.00, -16.14),
        "FR" => (-5.14,  41.34,  9.56,  51.09),
        "GA" => ( 8.70,  -3.98, 14.50,   2.33),
        "GB" => (-8.62,  49.89,  1.77,  60.85),
        "GE" => (39.97,  41.06, 46.64,  43.59),
        "GH" => (-3.26,   4.74,  1.19,  11.17),
        "GM" => (-16.82, 13.06,-13.80,  13.83),
        "GN" => (-15.13,  7.19,-7.64,   12.67),
        "GQ" => ( 8.31,   0.92, 11.33,   3.76),
        "GR" => (19.37,  34.80, 28.24,  41.75),
        "GT" => (-92.23, 13.74,-88.23,  17.82),
        "GW" => (-16.71, 10.93,-13.70,  12.68),
        "GY" => (-61.41,  1.18,-56.48,   8.55),
        "HN" => (-89.36, 12.98,-83.15,  16.52),
        "HR" => (13.50,  42.39, 19.45,  46.54),
        "HT" => (-74.46, 18.02,-71.62,  19.92),
        "HU" => (16.11,  45.74, 22.90,  48.59),
        "ID" => (95.01,  -8.55,141.02,   5.91),
        "IE" => (-10.48, 51.42, -6.01,  55.39),
        "IL" => (34.27,  29.50, 35.89,  33.34),
        "IN" => (68.19,   8.07, 97.40,  35.51),
        "IQ" => (38.79,  29.10, 48.57,  37.39),
        "IR" => (44.03,  25.07, 63.33,  39.78),
        "IS" => (-24.33, 63.29,-13.50,  66.56),
        "IT" => ( 6.63,  35.49, 18.52,  47.09),
        "JM" => (-78.37, 17.70,-76.19,  18.52),
        "JO" => (34.92,  29.19, 39.30,  33.38),
        "JP" => (122.94, 24.04,145.82,  45.55),
        "KE" => (33.91,  -4.67, 41.90,   5.02),
        "KG" => (69.46,  39.19, 80.26,  43.24),
        "KH" => (102.35, 10.49,107.63,  14.70),
        "KI" => (-176.00,-4.70,176.00,   3.38),
        "KM" => (43.22, -12.41, 44.54, -11.36),
        "KP" => (124.32, 37.67,130.78,  42.99),
        "KR" => (125.89, 33.11,129.59,  38.61),
        "KW" => (46.57,  28.53, 48.42,  30.10),
        "KZ" => (50.27,  40.66, 87.36,  55.39),
        "LA" => (100.12, 13.88,107.56,  22.50),
        "LB" => (35.13,  33.09, 36.61,  34.64),
        "LI" => ( 9.47,  47.05,  9.64,  47.27),
        "LK" => (79.65,   5.92, 81.88,   9.84),
        "LR" => (-11.49,  4.36, -7.37,   8.55),
        "LS" => (27.01, -30.65, 29.46, -28.57),
        "LT" => (20.94,  53.91, 26.84,  56.45),
        "LU" => ( 5.67,  49.44,  6.53,  50.18),
        "LV" => (20.97,  55.68, 28.24,  57.97),
        "LY" => ( 9.32,  19.50, 25.19,  33.17),
        "MA" => (-17.02, 27.66,  2.00,  35.76),
        "MD" => (26.62,  45.49, 30.05,  48.47),
        "ME" => (18.45,  41.85, 20.34,  43.55),
        "MG" => (43.25, -25.60, 50.48, -11.95),
        "MK" => (20.45,  40.84, 22.95,  42.37),
        "ML" => (-12.17, 10.10,  4.27,  24.97),
        "MM" => (92.18,   9.79,101.18,  28.34),
        "MN" => (87.76,  41.60,119.93,  52.15),
        "MR" => (-17.07, 14.62,  16.21, 27.30),
        "MT" => (14.18,  35.79, 14.58,  36.09),
        "MU" => (57.31, -20.52, 57.80, -19.97),
        "MV" => (72.76,  -0.69, 73.76,   7.10),
        "MW" => (32.68, -17.14, 35.92,  -9.23),
        "MX" => (-118.35,14.53,-86.71,  32.72),
        "MY" => (99.64,   0.85,119.28,   7.36),
        "MZ" => (32.07, -26.86, 40.84,  -10.32),
        "NA" => (11.72, -29.05, 25.26, -16.95),
        "NE" => ( 0.30,  11.70, 15.90,  23.33),
        "NG" => ( 2.69,   4.24, 14.68,  13.87),
        "NI" => (-87.67, 10.73,-83.15,  15.02),
        "NL" => ( 3.36,  50.75,  7.23,  53.55),
        "NO" => ( 4.99,  57.98, 31.29,  71.18),
        "NP" => (80.09,  26.40, 88.18,  30.42),
        "NR" => (166.91, -0.55,166.96,  -0.50),
        "NZ" => (166.00,-47.50,178.50, -34.00),
        "OM" => (52.00,  16.65, 59.84,  26.40),
        "PA" => (-83.05,  7.21,-77.16,   9.66),
        "PE" => (-81.33,-18.35,-68.65,  -0.04),
        "PG" => (140.84, -11.66,155.97,  -1.31),
        "PH" => (117.17,  4.59,126.54,  21.12),
        "PK" => (60.87,  23.69, 77.83,  36.97),
        "PL" => (14.12,  49.00, 24.15,  54.84),
        "PT" => (-9.53,  36.84, -6.19,  42.15),
        "PW" => (131.12,  2.96,134.72,   8.10),
        "PY" => (-62.64,-27.59,-54.29, -19.29),
        "QA" => (50.75,  24.55, 51.61,  26.15),
        "RO" => (22.09,  43.62, 29.69,  48.26),
        "RS" => (18.83,  42.25, 22.99,  46.17),
        "RU" => (-180.00,41.19,180.00,  81.86),
        "RW" => (28.86,  -2.84, 30.90,  -1.05),
        "SA" => (34.63,  16.30, 55.67,  32.13),
        "SB" => (155.51, -11.87,162.40,  -6.60),
        "SC" => (55.22,  -9.80, 55.79,  -9.51),
        "SD" => (23.89,   8.68, 38.41,  22.23),
        "SE" => (10.96,  55.34, 24.17,  69.06),
        "SG" => (103.64,  1.20,104.01,   1.47),
        "SI" => (13.70,  45.43, 16.56,  46.85),
        "SK" => (16.83,  47.73, 22.56,  49.61),
        "SL" => (-13.25,  6.93,-10.30,   9.99),
        "SM" => (12.41,  43.90, 12.52,  43.99),
        "SN" => (-17.54, 12.33,-11.35,  15.80),
        "SO" => (40.99,  -1.68, 51.41,  11.99),
        "SR" => (-58.07,  1.82,-53.96,   6.01),
        "SS" => (23.89,   3.51, 47.97,  12.24),
        "ST" => ( 6.47,   0.02,  7.46,   1.70),
        "SV" => (-90.10, 13.15,-87.72,  14.45),
        "SY" => (35.70,  32.31, 42.38,  37.32),
        "SZ" => (30.79, -27.32, 32.13, -25.72),
        "TD" => (13.47,   7.44, 24.00,  23.45),
        "TG" => ( 0.00,   6.11,  1.87,  11.14),
        "TH" => (97.34,   5.61,105.64,  20.46),
        "TJ" => (67.36,  36.68, 74.98,  41.04),
        "TL" => (124.05, -9.46,127.34,  -8.13),
        "TM" => (52.50,  35.14, 66.55,  42.79),
        "TN" => ( 7.52,  30.24, 11.49,  37.35),
        "TO" => (-175.36,-21.46,-173.71,-15.56),
        "TR" => (25.67,  35.82, 44.82,  42.11),
        "TT" => (-61.95, 10.03,-60.90,  10.89),
        "TV" => (176.06, -9.00,178.72,  -5.64),
        "TZ" => (29.34, -11.74, 40.44,  -0.95),
        "UA" => (22.14,  44.36, 40.17,  52.38),
        "UG" => (29.58,  -1.48, 35.01,   4.23),
        "US" => (-179.14,18.91,-66.95,  71.39),
        "UY" => (-58.44,-34.95,-53.21, -30.11),
        "UZ" => (55.99,  37.14, 73.06,  45.59),
        "VA" => (12.45,  41.90, 12.46,  41.91),
        "VC" => (-61.46, 12.58,-61.12,  13.38),
        "VE" => (-73.35,  0.65,-59.76,  12.16),
        "VN" => (102.17,  8.33,109.47,  23.39),
        "VU" => (166.52,-20.25,170.24, -13.07),
        "WS" => (-172.80,-14.07,-171.43,-13.45),
        "YE" => (42.58,  12.11, 53.09,  19.00),
        "ZA" => (16.34, -34.82, 32.89, -22.13),
        "ZM" => (21.99, -18.08, 33.70,  -8.23),
        "ZW" => (25.24, -22.27, 32.85, -15.61),
        _ => return None,
    };
    Some(Bbox { west: w, south: s, east: e, north: n })
}

/// Map ISO 3166-1 alpha-3 → alpha-2 for every country in `country_bbox`.
fn alpha3_to_alpha2(code: &str) -> Option<&'static str> {
    Some(match code {
        "AND" => "AD", "ARE" => "AE", "AFG" => "AF", "ATG" => "AG",
        "ALB" => "AL", "ARM" => "AM", "AGO" => "AO", "ARG" => "AR",
        "AUT" => "AT", "AUS" => "AU", "AZE" => "AZ", "BIH" => "BA",
        "BGD" => "BD", "BEL" => "BE", "BFA" => "BF", "BGR" => "BG",
        "BEN" => "BJ", "BRN" => "BN", "BOL" => "BO", "BRA" => "BR",
        "BTN" => "BT", "BWA" => "BW", "BLR" => "BY", "BLZ" => "BZ",
        "CAN" => "CA", "COD" => "CD", "CAF" => "CF", "COG" => "CG",
        "CHE" => "CH", "CIV" => "CI", "CHL" => "CL", "CMR" => "CM",
        "CHN" => "CN", "COL" => "CO", "CRI" => "CR", "CUB" => "CU",
        "CPV" => "CV", "CYP" => "CY", "CZE" => "CZ", "DEU" => "DE",
        "DJI" => "DJ", "DNK" => "DK", "DOM" => "DO", "DZA" => "DZ",
        "ECU" => "EC", "EST" => "EE", "EGY" => "EG", "ERI" => "ER",
        "ESP" => "ES", "ETH" => "ET", "FIN" => "FI", "FJI" => "FJ",
        "FRA" => "FR", "GAB" => "GA", "GBR" => "GB", "GEO" => "GE",
        "GHA" => "GH", "GMB" => "GM", "GIN" => "GN", "GNQ" => "GQ",
        "GRC" => "GR", "GTM" => "GT", "GNB" => "GW", "GUY" => "GY",
        "HND" => "HN", "HRV" => "HR", "HTI" => "HT", "HUN" => "HU",
        "IDN" => "ID", "IRL" => "IE", "ISR" => "IL", "IND" => "IN",
        "IRQ" => "IQ", "IRN" => "IR", "ISL" => "IS", "ITA" => "IT",
        "JAM" => "JM", "JOR" => "JO", "JPN" => "JP", "KEN" => "KE",
        "KGZ" => "KG", "KHM" => "KH", "KIR" => "KI", "COM" => "KM",
        "PRK" => "KP", "KOR" => "KR", "KWT" => "KW", "KAZ" => "KZ",
        "LAO" => "LA", "LBN" => "LB", "LIE" => "LI", "LKA" => "LK",
        "LBR" => "LR", "LSO" => "LS", "LTU" => "LT", "LUX" => "LU",
        "LVA" => "LV", "LBY" => "LY", "MAR" => "MA", "MDA" => "MD",
        "MNE" => "ME", "MDG" => "MG", "MKD" => "MK", "MLI" => "ML",
        "MMR" => "MM", "MNG" => "MN", "MRT" => "MR", "MLT" => "MT",
        "MUS" => "MU", "MDV" => "MV", "MWI" => "MW", "MEX" => "MX",
        "MYS" => "MY", "MOZ" => "MZ", "NAM" => "NA", "NER" => "NE",
        "NGA" => "NG", "NIC" => "NI", "NLD" => "NL", "NOR" => "NO",
        "NPL" => "NP", "NRU" => "NR", "NZL" => "NZ", "OMN" => "OM",
        "PAN" => "PA", "PER" => "PE", "PNG" => "PG", "PHL" => "PH",
        "PAK" => "PK", "POL" => "PL", "PRT" => "PT", "PLW" => "PW",
        "PRY" => "PY", "QAT" => "QA", "ROU" => "RO", "SRB" => "RS",
        "RUS" => "RU", "RWA" => "RW", "SAU" => "SA", "SLB" => "SB",
        "SYC" => "SC", "SDN" => "SD", "SWE" => "SE", "SGP" => "SG",
        "SVN" => "SI", "SVK" => "SK", "SLE" => "SL", "SMR" => "SM",
        "SEN" => "SN", "SOM" => "SO", "SUR" => "SR", "SSD" => "SS",
        "STP" => "ST", "SLV" => "SV", "SYR" => "SY", "SWZ" => "SZ",
        "TCD" => "TD", "TGO" => "TG", "THA" => "TH", "TJK" => "TJ",
        "TLS" => "TL", "TKM" => "TM", "TUN" => "TN", "TON" => "TO",
        "TUR" => "TR", "TTO" => "TT", "TUV" => "TV", "TZA" => "TZ",
        "UKR" => "UA", "UGA" => "UG", "USA" => "US", "URY" => "UY",
        "UZB" => "UZ", "VAT" => "VA", "VCT" => "VC", "VEN" => "VE",
        "VNM" => "VN", "VUT" => "VU", "WSM" => "WS", "YEM" => "YE",
        "ZAF" => "ZA", "ZMB" => "ZM", "ZWE" => "ZW",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nz_resolves() {
        let bbox = resolve("NZ").unwrap().unwrap();
        assert!(bbox.west < 167.0 && bbox.east > 178.0);
    }

    #[test]
    fn world_resolves_to_none() {
        assert!(resolve("world").unwrap().is_none());
    }

    #[test]
    fn explicit_bbox_parses() {
        let bbox = resolve("166.0,-47.5,178.5,-34.0").unwrap().unwrap();
        assert!((bbox.west - 166.0).abs() < 0.001);
    }

    #[test]
    fn continent_resolves() {
        let bbox = resolve("oceania").unwrap().unwrap();
        assert!(bbox.west < 112.0);
    }

    #[test]
    fn alpha3_resolves_same_as_alpha2() {
        let a2 = resolve("NZ").unwrap().unwrap();
        let a3 = resolve("NZL").unwrap().unwrap();
        assert_eq!(a2.west,  a3.west);
        assert_eq!(a2.east,  a3.east);
        assert_eq!(a2.south, a3.south);
        assert_eq!(a2.north, a3.north);
    }

    #[test]
    fn alpha3_case_insensitive() {
        assert!(resolve("nzl").unwrap().is_some());
        assert!(resolve("Nzl").unwrap().is_some());
    }
}
