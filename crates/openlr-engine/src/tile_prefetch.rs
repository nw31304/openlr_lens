use openlr_codec::lrp::Lrp;
use openlr_graph::{haversine_m, TileKey};
use std::collections::HashSet;

use crate::params::DecodeParams;

/// Compute the set of tiles that should be loaded before decoding begins.
///
/// Strategy:
/// - For each LRP: fetch the 3×3 neighbourhood (covers the candidate search radius).
/// - For each leg (consecutive LRP pair): fetch all tiles intersecting a bounding box
///   drawn around the corridor, buffered by `search_radius_m` on each side.
///
/// This is pure slippy-tile geometry; no map data is required.
pub fn prefetch_tile_keys(lrps: &[Lrp], params: &DecodeParams, zoom: u8) -> Vec<TileKey> {
    let mut set: HashSet<TileKey> = HashSet::new();

    // Per-LRP: 3×3 neighbourhood.
    for lrp in lrps {
        let (lon, lat) = lrp.coord;
        for key in TileKey::from_lonlat(lon, lat, zoom).neighborhood() {
            set.insert(key);
        }
    }

    // Per-leg: corridor bounding box.
    for pair in lrps.windows(2) {
        let (lon0, lat0) = pair[0].coord;
        let (lon1, lat1) = pair[1].coord;

        // Perpendicular corridor buffer: candidate radius + 30 % of crow-fly,
        // capped at 3 km.  Using max_path_search_factor × DNP here loaded the
        // entire A* search circle as tiles (thousands for long legs), which
        // wastes memory and IO.  The actual route stays much closer to the
        // straight line between LRPs; if it doesn't, A* fails gracefully.
        let crow_fly_m = haversine_m(lon0, lat0, lon1, lat1);
        let buffer_m = params.candidate_search_radius_m + (crow_fly_m * 0.30).min(3_000.0);

        // Approximate degrees of buffer (1° ≈ 111 km).
        let buf_deg = buffer_m / 111_000.0;

        let west  = lon0.min(lon1) - buf_deg;
        let east  = lon0.max(lon1) + buf_deg;
        let south = lat0.min(lat1) - buf_deg;
        let north = lat0.max(lat1) + buf_deg;

        for key in tiles_in_bbox(west, south, east, north, zoom) {
            set.insert(key);
        }
    }

    let mut v: Vec<TileKey> = set.into_iter().collect();
    // Hilbert-ish sort: by z/x/y so callers can batch fetches efficiently.
    v.sort_by_key(|k| (k.z, k.x, k.y));
    v
}

/// Enumerate all tiles at `zoom` that intersect the given bounding box.
fn tiles_in_bbox(west: f64, south: f64, east: f64, north: f64, zoom: u8) -> Vec<TileKey> {
    let max = (1u32 << zoom).saturating_sub(1);

    let tl = TileKey::from_lonlat(west,  north, zoom);
    let br = TileKey::from_lonlat(east,  south, zoom);

    let x0 = tl.x.min(br.x);
    let x1 = tl.x.max(br.x).min(max);
    let y0 = tl.y.min(br.y);
    let y1 = tl.y.max(br.y).min(max);

    let mut out = Vec::new();
    for x in x0..=x1 {
        for y in y0..=y1 {
            out.push(TileKey { z: zoom, x, y });
        }
    }
    out
}

/// Straight-line distance between two LRP coordinates (useful for corridor sizing).
#[allow(dead_code)]
fn lrp_dist_m(a: &Lrp, b: &Lrp) -> f64 {
    haversine_m(a.coord.0, a.coord.1, b.coord.0, b.coord.1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_codec::interval::LinearInterval;
    use openlr_codec::lrp::Lrp;
    use openlr_codec::CircularInterval;

    fn dummy_lrp(lon: f64, lat: f64) -> Lrp {
        Lrp {
            coord: (lon, lat),
            bearing: CircularInterval::point(0.0),
            frc: 3,
            fow: 3,
            lfrcnp: Some(5),
            dnp: Some(LinearInterval { lb: 500.0, ub: 558.6 }),
            pos_offset: None,
            neg_offset: None,
        }
    }

    #[test]
    fn two_lrps_produce_tiles() {
        let lrps = vec![dummy_lrp(13.41, 52.52), dummy_lrp(13.42, 52.52)];
        let params = DecodeParams::default();
        let keys = prefetch_tile_keys(&lrps, &params, 12);
        assert!(!keys.is_empty());
        // All tiles should be at zoom 12
        assert!(keys.iter().all(|k| k.z == 12));
    }

    #[test]
    fn tiles_in_bbox_covers_single_tile() {
        let keys = tiles_in_bbox(13.41, 52.52, 13.41, 52.52, 12);
        assert_eq!(keys.len(), 1);
    }
}
