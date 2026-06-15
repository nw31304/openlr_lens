/// Identifies a single slippy-map tile at a fixed zoom level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TileKey {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl TileKey {
    /// Compute the tile containing `(lon, lat)` at zoom `z`.
    pub fn from_lonlat(lon: f64, lat: f64, z: u8) -> Self {
        let n = (1u32 << z) as f64;
        let max = (1u32 << z).saturating_sub(1);
        let x = ((lon + 180.0) / 360.0 * n).floor() as u32;
        let lat_r = lat.to_radians();
        let y = ((1.0 - lat_r.tan().asinh() / std::f64::consts::PI) / 2.0 * n).floor() as u32;
        TileKey { z, x: x.min(max), y: y.min(max) }
    }

    /// Bounding box `(west, south, east, north)` in degrees.
    pub fn bbox(self) -> (f64, f64, f64, f64) {
        let n = (1u32 << self.z) as f64;
        let west  = self.x as f64 / n * 360.0 - 180.0;
        let east  = (self.x as f64 + 1.0) / n * 360.0 - 180.0;
        let north_rad = std::f64::consts::PI * (1.0 - 2.0 * self.y as f64 / n);
        let south_rad = std::f64::consts::PI * (1.0 - 2.0 * (self.y as f64 + 1.0) / n);
        let north = north_rad.sinh().atan().to_degrees();
        let south = south_rad.sinh().atan().to_degrees();
        (west, south, east, north)
    }

    /// 3×3 neighbourhood (including self), clamped to valid tile range.
    pub fn neighborhood(self) -> Vec<TileKey> {
        let max = (1u32 << self.z).saturating_sub(1) as i32;
        let mut out = Vec::with_capacity(9);
        for dy in -1i32..=1 {
            for dx in -1i32..=1 {
                let nx = self.x as i32 + dx;
                let ny = self.y as i32 + dy;
                if nx >= 0 && ny >= 0 && nx <= max && ny <= max {
                    out.push(TileKey { z: self.z, x: nx as u32, y: ny as u32 });
                }
            }
        }
        out
    }
}

// ──────────────────────────────────────────────────────────────────────────────

/// Magic bytes for the tile payload header.
pub const TILE_MAGIC: [u8; 4] = *b"OLRL";
pub const TILE_VERSION: u8 = 1;

/// Tile header — all integers little-endian.
#[repr(C)]
pub struct TileHeader {
    pub magic:              [u8; 4],
    pub version:            u8,
    pub flags:              u8,
    pub _pad:               [u8; 2],
    pub segment_count:      u32,
    pub node_count:         u32,
    pub restriction_count:  u32,
    pub geom_vertex_count:  u32,
    pub xrestriction_count: u32,
    pub _reserved:          [u8; 12],
}
const _: () = assert!(std::mem::size_of::<TileHeader>() == 40);
