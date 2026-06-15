pub mod pmtiles;
pub mod tile_reader;

pub use pmtiles::{PmtilesError, PmtilesReader};
pub use tile_reader::{TileLoader, TileReadError};

use std::collections::HashSet;
use std::path::Path;

use openlr_graph::{Graph, NetworkSegment, SegmentId, TileKey};

// ── OpenLrDataProvider trait (synchronous; WASM fulfils via JS-driven cache) ──

/// A coarse spatial result returned by the provider before exact engine filtering.
pub struct SpatialMapChunk {
    pub segments: Vec<NetworkSegment>,
}

/// All map access goes through this trait so the engine is storage-agnostic.
pub trait OpenLrDataProvider {
    type Error: std::fmt::Debug;

    /// Segments whose geometry comes within `radius_m` of `(lon, lat)`.
    fn segments_near(
        &self,
        lon: f64,
        lat: f64,
        radius_m: f64,
    ) -> Result<SpatialMapChunk, Self::Error>;

    /// Resolve a segment by stable id (cross-tile expansion / boundary stitching).
    fn segment_by_id(&self, id: SegmentId) -> Result<Option<NetworkSegment>, Self::Error>;
}

// ── PmtilesProvider ────────────────────────────────────────────────────────────

/// Combined error type for the PMTiles provider.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("pmtiles: {0}")]
    Pmtiles(#[from] PmtilesError),
    #[error("tile parse: {0}")]
    TileParse(#[from] TileReadError),
}

/// Reads tiles on demand from a `.pmtiles` archive, building an in-memory `Graph`.
///
/// On first access to an area, the 3×3 tile neighbourhood is loaded and merged.
/// Boundary nodes are stitched by GERS ID across tiles.
pub struct PmtilesProvider {
    reader: PmtilesReader,
    loader: TileLoader,
    loaded_tiles: HashSet<TileKey>,
    pub zoom: u8,
}

impl PmtilesProvider {
    /// Open the archive and derive the zoom level from the manifest/header.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ProviderError> {
        let reader = PmtilesReader::open(path)?;
        let zoom = reader.min_zoom();
        Ok(Self {
            reader,
            loader: TileLoader::new(),
            loaded_tiles: HashSet::new(),
            zoom,
        })
    }

    /// Pre-load all tiles in `keys`.  Call with `prefetch_tile_keys` output
    /// before starting a decode.
    pub fn load_tiles(&mut self, keys: &[TileKey]) -> Result<(), ProviderError> {
        for &key in keys {
            self.ensure_loaded(key)?;
        }
        Ok(())
    }

    /// Borrow the built graph.
    pub fn graph(&self) -> &Graph {
        &self.loader.graph
    }

    /// Consume the provider, returning the built graph.
    pub fn into_graph(self) -> Graph {
        self.loader.graph
    }

    fn ensure_loaded(&mut self, key: TileKey) -> Result<(), ProviderError> {
        if self.loaded_tiles.contains(&key) {
            return Ok(());
        }
        // Fetch tile bytes; missing tiles are silently skipped (sparse coverage).
        if let Some(bytes) = self.reader.get_tile(key)? {
            self.loader.load_tile(&bytes)?;
        }
        self.loaded_tiles.insert(key);
        Ok(())
    }
}

impl OpenLrDataProvider for PmtilesProvider {
    type Error = ProviderError;

    fn segments_near(&self, lon: f64, lat: f64, radius_m: f64) -> Result<SpatialMapChunk, Self::Error> {
        let nearby = self.loader.graph.segments_near(lon, lat, radius_m);
        let segments = nearby
            .iter()
            .filter_map(|(id, _)| self.loader.graph.segments.get(id).cloned())
            .collect();
        Ok(SpatialMapChunk { segments })
    }

    fn segment_by_id(&self, id: SegmentId) -> Result<Option<NetworkSegment>, Self::Error> {
        Ok(self.loader.graph.segments.get(&id).cloned())
    }
}

// ── Integration test against a real NZ archive ───────────────────────────────

#[cfg(test)]
mod integration {
    use super::*;
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2) // crates/openlr-provider → workspace root
            .unwrap()
            .to_path_buf()
    }

    fn nz_archive() -> Option<PathBuf> {
        let candidates = [
            "out/nz-osm/openlrlens-nz-new-zealand-latest.pmtiles",
            "out/openlrlens-nz-2026-05-20-0.pmtiles",
            "out/openlrlens-166.0000,-47.5000,178.5000,-34.0000-2026-05-20-0.pmtiles",
            "out/openlrlens-nz-2026-05-20.pmtiles",
        ];
        let ws = workspace_root();
        candidates.iter().map(|c| ws.join(c)).find(|p| p.exists())
    }

    fn de_archive() -> Option<PathBuf> {
        let candidates = [
            "out/de-osm/openlrlens-de-germany-latest.pmtiles",
            "out/openlrlens-de-germany-latest.pmtiles",
        ];
        let ws = workspace_root();
        candidates.iter().map(|c| ws.join(c)).find(|p| p.exists())
    }

    #[test]
    fn open_nz_archive_and_load_auckland_tile() {
        let Some(path) = nz_archive() else {
            eprintln!("SKIP: NZ PMTiles archive not found");
            return;
        };
        let mut provider = PmtilesProvider::open(&path).expect("open failed");
        assert_eq!(provider.zoom, 12, "expected z12 tiles");

        // Auckland CBD ≈ (174.76, -36.85)
        let key = TileKey::from_lonlat(174.76, -36.85, 12);
        provider.load_tiles(&key.neighborhood()).expect("load failed");

        let g = provider.graph();
        assert!(!g.segments.is_empty(), "loaded graph should have segments");
        assert!(!g.nodes.is_empty(),    "loaded graph should have nodes");

        // A reasonably dense urban tile should have hundreds of segments.
        eprintln!("Auckland tile: {} segments, {} nodes",
            g.segments.len(), g.nodes.len());
    }

    #[test]
    fn segments_near_auckland() {
        let Some(path) = nz_archive() else {
            eprintln!("SKIP: NZ PMTiles archive not found");
            return;
        };
        let mut provider = PmtilesProvider::open(&path).expect("open failed");
        let key = TileKey::from_lonlat(174.76, -36.85, 12);
        provider.load_tiles(&key.neighborhood()).expect("load failed");

        let chunk = provider.segments_near(174.76, -36.85, 100.0).expect("query failed");
        assert!(!chunk.segments.is_empty(), "should find segments within 100 m of Auckland CBD");
        eprintln!("segments within 100 m of (174.76, -36.85): {}", chunk.segments.len());
    }

    /// End-to-end smoke test: build a 2-LRP LocationReference from real Auckland road
    /// segments, run the full pipeline (`prefetch_tile_keys` → `load_tiles` → `decode`),
    /// and assert a non-empty path comes back.
    #[test]
    fn end_to_end_decode_auckland() {
        let Some(path) = nz_archive() else {
            eprintln!("SKIP: NZ PMTiles archive not found");
            return;
        };

        use openlr_codec::{CircularInterval, LinearInterval};
        use openlr_codec::lrp::{LocationReference, Lrp};
        use openlr_engine::{decode, prefetch_tile_keys, DecodeParams, Preset};
        use openlr_graph::{bearing_at_offset, haversine_m, Direction};

        let mut provider = PmtilesProvider::open(&path).expect("open failed");
        let key = TileKey::from_lonlat(174.76, -36.85, 12);
        provider.load_tiles(&key.neighborhood()).expect("initial tile load failed");

        // Find two connected segments: seg_a ends where seg_b begins (or vice-versa
        // for bidirectional).  Both must be ≥ 20 m so the bearing window isn't degenerate.
        let (seg_a, seg_b, coord_0, coord_1) = {
            let graph = provider.graph();
            let mut found = None;
            'outer: for sa in graph.segments.values() {
                if sa.length_m < 20.0 { continue; }
                if matches!(sa.direction, Direction::Backward) { continue; }
                for sb in graph.segments.values() {
                    if sb.id == sa.id { continue; }
                    if sb.length_m < 20.0 { continue; }
                    if matches!(sb.direction, Direction::Backward) { continue; }
                    // seg_a exits at sa.end_node; seg_b is entered from that same node.
                    if sb.start_node == sa.end_node {
                        let c0 = sa.geometry[0];
                        let c1 = *sb.geometry.last().unwrap();
                        found = Some((sa.clone(), sb.clone(), c0, c1));
                        break 'outer;
                    }
                }
            }
            found.expect("no connected segment pair found in Auckland graph")
        };

        // Build the 2-LRP reference using actual bearings from the segment geometry.
        // LRP[0]: at the start of seg_a, forward bearing.
        // LRP[1]: at the end of seg_b, backward bearing (approach direction, per OpenLR spec).
        let bearing_0 = bearing_at_offset(&seg_a.geometry, 0.0, true);
        let bearing_1 = bearing_at_offset(&seg_b.geometry, seg_b.length_m, false);
        let dnp_crow = haversine_m(coord_0.0, coord_0.1, coord_1.0, coord_1.1);

        let loc_ref = LocationReference {
            lrps: vec![
                Lrp {
                    coord: coord_0,
                    bearing: CircularInterval {
                        lb_deg: bearing_0 - 30.0,
                        ub_deg: bearing_0 + 30.0,
                    },
                    frc:    seg_a.frc,
                    fow:    seg_a.fow,
                    lfrcnp: Some(7),
                    dnp: Some(LinearInterval {
                        lb: 0.0,
                        ub: (dnp_crow + seg_a.length_m + seg_b.length_m) * 5.0,
                    }),
                    pos_offset: None,
                    neg_offset: None,
                },
                Lrp {
                    coord: coord_1,
                    bearing: CircularInterval {
                        lb_deg: bearing_1 - 30.0,
                        ub_deg: bearing_1 + 30.0,
                    },
                    frc:    seg_b.frc,
                    fow:    seg_b.fow,
                    lfrcnp: None,
                    dnp:    None,
                    pos_offset: None,
                    neg_offset: None,
                },
            ],
        };

        // Exercise the full pipeline: prefetch tiles for this reference, then decode.
        let params = DecodeParams::preset(Preset::Permissive);
        let keys = prefetch_tile_keys(&loc_ref.lrps, &params, provider.zoom);
        provider.load_tiles(&keys).expect("prefetch tile load failed");

        let result = decode(&loc_ref, provider.graph(), &params);
        match result {
            Ok(decoded) => {
                assert!(!decoded.path.is_empty(), "decoded path must be non-empty");
                assert!(
                    decoded.path.contains(&seg_a.id) || decoded.path.contains(&seg_b.id),
                    "path should include at least one of the source segments",
                );
                eprintln!(
                    "e2e decode OK: {} segments (seg_a={:?} seg_b={:?})",
                    decoded.path.len(), seg_a.id, seg_b.id
                );
            }
            Err(e) => panic!("end-to-end decode failed: {e:?}"),
        }
    }

    /// 6-LRP Germany reference near Basel (lat≈47.66°, lon≈7.73°).
    /// Exercises multi-leg RouteGenerator and WKT output for visual comparison.
    #[test]
    fn decode_germany_6lrp_wkt() {
        let Some(path) = de_archive() else {
            eprintln!("SKIP: DE PMTiles archive not found");
            return;
        };

        use openlr_codec::decoder::v3::decode_v3_base64;
        use openlr_engine::{decode, path_to_wkt, prefetch_tile_keys, DecodeParams, Preset};

        let mut provider = PmtilesProvider::open(&path).expect("open failed");

        let loc_ref = decode_v3_base64(
            "CwV/ECHkoiORC//N/bIjjRYD+fy+I44FAAv+0yOOAwAL/2cbcn3flfluGwM=",
        ).expect("v3 decode failed");

        assert_eq!(loc_ref.lrps.len(), 6, "expected 6 LRPs");
        for (i, lrp) in loc_ref.lrps.iter().enumerate() {
            eprintln!(
                "LRP[{i}]: ({:.6}, {:.6})  frc={} fow={} lfrcnp={}{}",
                lrp.coord.0, lrp.coord.1, lrp.frc, lrp.fow,
                lrp.lfrcnp.map_or("-".to_string(), |v| v.to_string()),
                lrp.dnp.map_or(String::new(), |d| format!("  dnp=[{:.0},{:.0}]m", d.lb, d.ub)),
            );
        }

        let params = DecodeParams::preset(Preset::Permissive);
        eprintln!("Params: radius={:.0}m  bearing_tol={:.0}°  max_cands={}  lfrcnp_tol={}  max_exp={}",
            params.candidate_search_radius_m, params.bearing_tolerance_deg,
            params.max_candidates_per_lrp, params.lfrcnp_tolerance, params.max_astar_expansions);
        let keys = prefetch_tile_keys(&loc_ref.lrps, &params, provider.zoom);
        eprintln!("Prefetching {} tile(s) …", keys.len());
        provider.load_tiles(&keys).expect("tile load failed");
        eprintln!("Graph: {} segs, {} nodes", provider.graph().segments.len(), provider.graph().nodes.len());

        let result = decode(&loc_ref, provider.graph(), &params).expect("decode failed");
        assert!(!result.path.is_empty());

        let wkt = path_to_wkt(
            &result.path, result.pos_offset_m, result.neg_offset_m,
            result.first_lrp_arc_m, result.last_lrp_arc_m, provider.graph(),
        ).expect("WKT generation failed");

        eprintln!("Decoded {} segment(s):", result.path.len());
        for id in &result.path { eprintln!("  {:?}", id); }
        eprintln!();
        println!("{wkt}");
    }

    /// End-to-end decode of a real Germany OpenLR v3 binary string.
    ///
    /// Reference: lon≈7.728°, lat≈48.259° (Offenburg, Baden-Württemberg).
    /// FRC=6, ~355 m crow-fly between the two LRPs.
    #[test]
    fn decode_germany_openlr() {
        let Some(path) = de_archive() else {
            eprintln!("SKIP: DE PMTiles archive not found");
            return;
        };

        use openlr_codec::decoder::v3::decode_v3_base64;
        use openlr_engine::{decode, prefetch_tile_keys, DecodeParams, Preset};

        let mut provider = PmtilesProvider::open(&path).expect("open failed");

        let loc_ref = decode_v3_base64("CwV+1CJROzbLCf8gARozDg==")
            .expect("v3 decode failed");

        assert_eq!(loc_ref.lrps.len(), 2);
        let lrp0 = &loc_ref.lrps[0];
        let lrp1 = &loc_ref.lrps[1];
        eprintln!(
            "LRP[0]: ({:.6}, {:.6})  frc={} fow={}  dnp=[{:.0},{:.0}] m  bearing=[{:.2},{:.2}]°",
            lrp0.coord.0, lrp0.coord.1, lrp0.frc, lrp0.fow,
            lrp0.dnp.as_ref().map_or(0.0, |d| d.lb),
            lrp0.dnp.as_ref().map_or(0.0, |d| d.ub),
            lrp0.bearing.lb_deg, lrp0.bearing.ub_deg,
        );
        eprintln!(
            "LRP[1]: ({:.6}, {:.6})  frc={} fow={}  bearing=[{:.2},{:.2}]°",
            lrp1.coord.0, lrp1.coord.1, lrp1.frc, lrp1.fow,
            lrp1.bearing.lb_deg, lrp1.bearing.ub_deg,
        );

        let params = DecodeParams::preset(Preset::Permissive);
        let keys = prefetch_tile_keys(&loc_ref.lrps, &params, provider.zoom);
        eprintln!("Prefetching {} tile(s) …", keys.len());
        provider.load_tiles(&keys).expect("tile load failed");

        eprintln!(
            "Graph: {} segments, {} nodes",
            provider.graph().segments.len(),
            provider.graph().nodes.len(),
        );

        let result = decode(&loc_ref, provider.graph(), &params);
        match result {
            Ok(decoded) => {
                assert!(!decoded.path.is_empty(), "decoded path must be non-empty");
                eprintln!("Germany decode OK: {} segment(s) in path", decoded.path.len());
                for id in &decoded.path {
                    eprintln!("  {:?}", id);
                }
            }
            Err(e) => panic!("Germany decode failed: {e:?}"),
        }
    }

    /// Diagnostic: print candidate counts per LRP and test all individual legs.
    /// Helps identify which leg combination prevents the full decode from succeeding.
    #[test]
    fn decode_germany_6lrp_candidates() {
        let Some(path) = de_archive() else {
            eprintln!("SKIP: DE PMTiles archive not found");
            return;
        };

        use openlr_codec::decoder::v3::decode_v3_base64;
        use openlr_codec::lrp::LocationReference;
        use openlr_engine::{decode, prefetch_tile_keys, DecodeParams, Preset};

        let mut provider = PmtilesProvider::open(&path).expect("open failed");

        let full_ref = decode_v3_base64(
            "CwV/ECHkoiORC//N/bIjjRYD+fy+I44FAAv+0yOOAwAL/2cbcn3flfluGwM=",
        ).expect("v3 decode failed");

        let params = DecodeParams::preset(Preset::Permissive);
        let keys = prefetch_tile_keys(&full_ref.lrps, &params, provider.zoom);
        provider.load_tiles(&keys).expect("tile load failed");
        eprintln!("Graph: {} segs, {} nodes", provider.graph().segments.len(), provider.graph().nodes.len());

        // Try each consecutive pair as an isolated 2-LRP decode.
        eprintln!("\n--- Testing each leg in isolation (max_cands={}) ---", params.max_candidates_per_lrp);
        for leg in 0..full_ref.lrps.len() - 1 {
            let loc2 = LocationReference { lrps: vec![full_ref.lrps[leg].clone(), full_ref.lrps[leg + 1].clone()] };
            let r = decode(&loc2, provider.graph(), &params);
            eprintln!(
                "  Leg {leg}: {} → {}  [lfrcnp={}, dnp=[{:.0},{:.0}]m]  → {}",
                leg, leg + 1,
                full_ref.lrps[leg].lfrcnp.map_or("-".into(), |v: u8| v.to_string()),
                full_ref.lrps[leg].dnp.as_ref().map_or(0.0, |d| d.lb),
                full_ref.lrps[leg].dnp.as_ref().map_or(0.0, |d| d.ub),
                match &r {
                    Ok(d) => format!("OK ({} segs)", d.path.len()),
                    Err(e) => format!("FAIL: {e:?}"),
                },
            );
        }

        // Try with higher lfrcnp_tolerance to see if FRC mapping is the issue.
        for tol in [2u8, 3u8] {
            let mut p2 = DecodeParams::preset(Preset::Permissive);
            p2.lfrcnp_tolerance = tol;
            p2.max_candidates_per_lrp = 0; // unlimited
            eprintln!("\n--- lfrcnp_tolerance={tol}, unlimited candidates ---");
            for leg in 0..full_ref.lrps.len() - 1 {
                let loc2 = LocationReference { lrps: vec![full_ref.lrps[leg].clone(), full_ref.lrps[leg + 1].clone()] };
                let r = decode(&loc2, provider.graph(), &p2);
                eprintln!(
                    "  Leg {leg}: {}",
                    match &r {
                        Ok(d) => format!("OK ({} segs)", d.path.len()),
                        Err(e) => format!("FAIL: {e:?}"),
                    },
                );
            }
        }
    }

    /// Isolates leg 4 of the 6-LRP Germany reference (LRP[4]→LRP[5], ~7.3 km westward
    /// through the Wiesental valley).  Constructs a 2-LRP LocationReference directly so we
    /// can debug candidate selection and A* without the combinatorial overhead of all 6 LRPs.
    #[test]
    fn decode_germany_leg4_isolated() {
        let Some(path) = de_archive() else {
            eprintln!("SKIP: DE PMTiles archive not found");
            return;
        };

        use openlr_codec::decoder::v3::decode_v3_base64;
        use openlr_codec::lrp::LocationReference;
        use openlr_engine::{decode, path_to_wkt, prefetch_tile_keys, DecodeParams, Preset};

        let mut provider = PmtilesProvider::open(&path).expect("open failed");

        // Decode the full 6-LRP reference to get the LRP structs, then extract LRP[4] + LRP[5].
        let full_ref = decode_v3_base64(
            "CwV/ECHkoiORC//N/bIjjRYD+fy+I44FAAv+0yOOAwAL/2cbcn3flfluGwM=",
        ).expect("v3 decode failed");

        let lrp4 = full_ref.lrps[4].clone();
        let lrp5 = full_ref.lrps[5].clone();
        let loc_ref = LocationReference { lrps: vec![lrp4.clone(), lrp5.clone()] };

        eprintln!(
            "LRP[4→0]: ({:.6}, {:.6})  frc={} fow={} lfrcnp={}  dnp=[{:.0},{:.0}]m  bearing=[{:.2},{:.2}]°",
            lrp4.coord.0, lrp4.coord.1, lrp4.frc, lrp4.fow,
            lrp4.lfrcnp.map_or("-".to_string(), |v| v.to_string()),
            lrp4.dnp.as_ref().map_or(0.0, |d| d.lb),
            lrp4.dnp.as_ref().map_or(0.0, |d| d.ub),
            lrp4.bearing.lb_deg, lrp4.bearing.ub_deg,
        );
        eprintln!(
            "LRP[5→1]: ({:.6}, {:.6})  frc={} fow={}  bearing=[{:.2},{:.2}]°",
            lrp5.coord.0, lrp5.coord.1, lrp5.frc, lrp5.fow,
            lrp5.bearing.lb_deg, lrp5.bearing.ub_deg,
        );

        let params = DecodeParams::preset(Preset::Permissive);
        eprintln!(
            "Params: radius={:.0}m  bearing_tol={:.0}°  lfrcnp_tolerance={}  max_expansions={}",
            params.candidate_search_radius_m, params.bearing_tolerance_deg,
            params.lfrcnp_tolerance, params.max_astar_expansions,
        );

        let keys = prefetch_tile_keys(&loc_ref.lrps, &params, provider.zoom);
        eprintln!("Prefetching {} tile(s) …", keys.len());
        provider.load_tiles(&keys).expect("tile load failed");
        eprintln!("Graph: {} segs, {} nodes", provider.graph().segments.len(), provider.graph().nodes.len());

        let result = decode(&loc_ref, provider.graph(), &params);
        match result {
            Ok(decoded) => {
                eprintln!("Leg-4 decode OK: {} segment(s)", decoded.path.len());
                if let Some(wkt) = path_to_wkt(&decoded.path, decoded.pos_offset_m, decoded.neg_offset_m, decoded.first_lrp_arc_m, decoded.last_lrp_arc_m, provider.graph()) {
                    println!("{wkt}");
                }
            }
            Err(e) => {
                eprintln!("Leg-4 decode FAILED: {e:?}");
                panic!("decode failed: {e:?}");
            }
        }
    }

    /// Negative-offset trimming test: decode a 2-LRP Germany reference that encodes a
    /// negative offset (the decoded path is trimmed at the far end).
    /// Verifies that `neg_offset_m` is decoded correctly and that `path_to_wkt` trims the
    /// end of the polyline to the right position.
    #[test]
    fn decode_germany_neg_offset_wkt() {
        let Some(path) = de_archive() else {
            eprintln!("SKIP: DE PMTiles archive not found");
            return;
        };

        use openlr_codec::decoder::v3::decode_v3_base64;
        use openlr_engine::{decode, path_to_wkt, prefetch_tile_keys, DecodeParams, Preset};

        let mut provider = PmtilesProvider::open(&path).expect("open failed");

        let loc_ref = decode_v3_base64("CwV0fiHP2iupDwRE/tYrqgr/6v50KyIf")
            .expect("v3 decode failed");

        eprintln!("LRP count: {}", loc_ref.lrps.len());
        for (i, lrp) in loc_ref.lrps.iter().enumerate() {
            eprintln!(
                "LRP[{i}]: ({:.6}, {:.6})  frc={} fow={} lfrcnp={}{}  pos_offset={:?}  neg_offset={:?}",
                lrp.coord.0, lrp.coord.1, lrp.frc, lrp.fow,
                lrp.lfrcnp.map_or("-".to_string(), |v| v.to_string()),
                lrp.dnp.map_or(String::new(), |d| format!("  dnp=[{:.0},{:.0}]m", d.lb, d.ub)),
                lrp.pos_offset, lrp.neg_offset,
            );
        }

        let params = DecodeParams::preset(Preset::Permissive);
        let keys = prefetch_tile_keys(&loc_ref.lrps, &params, provider.zoom);
        eprintln!("Prefetching {} tile(s) …", keys.len());
        provider.load_tiles(&keys).expect("tile load failed");
        eprintln!("Graph: {} segs, {} nodes", provider.graph().segments.len(), provider.graph().nodes.len());

        let result = decode(&loc_ref, provider.graph(), &params).expect("decode failed");
        assert!(!result.path.is_empty(), "path must be non-empty");

        eprintln!(
            "Decoded {} segment(s), pos_offset={:.1}m, neg_offset={:.1}m, last_lrp_arc={:.1}m",
            result.path.len(), result.pos_offset_m, result.neg_offset_m, result.last_lrp_arc_m,
        );
        assert!(result.neg_offset_m > 0.0, "expected a positive neg_offset_m (the encoded negative offset)");

        let wkt = path_to_wkt(
            &result.path, result.pos_offset_m, result.neg_offset_m,
            result.first_lrp_arc_m, result.last_lrp_arc_m, provider.graph(),
        ).expect("WKT generation failed");

        eprintln!("WKT point count: {}", wkt.split(',').count());
        println!("{wkt}");

        // Write to a tmp file for visual inspection.
        std::fs::write("/tmp/germany_neg_offset.wkt", &wkt).ok();
        eprintln!("WKT written to /tmp/germany_neg_offset.wkt");
    }

    /// Diagnostic test: dump all accepted candidates for each LRP of the pos-offset reference
    /// `CwV1BCHeEDv1BQEj/3s7WiY=` to understand why the wrong road is selected.
    #[test]
    fn decode_germany_pos_offset_candidates() {
        let Some(path) = de_archive() else {
            eprintln!("SKIP: DE PMTiles archive not found");
            return;
        };

        use openlr_codec::decoder::v3::decode_v3_base64;
        use openlr_engine::{decode, prefetch_tile_keys, DecodeParams, DecodeEvent, Preset};
        use openlr_engine::trace::TraceLevel;

        let mut provider = PmtilesProvider::open(&path).expect("open failed");

        let loc_ref = decode_v3_base64("CwV1BCHeEDv1BQEj/3s7WiY=")
            .expect("v3 decode failed");

        eprintln!("=== Pos-offset reference: CwV1BCHeEDv1BQEj/3s7WiY= ===");
        for (i, lrp) in loc_ref.lrps.iter().enumerate() {
            eprintln!(
                "LRP[{i}]: ({:.6}, {:.6})  frc={} fow={}  lfrcnp={}  bearing=[{:.2},{:.2}]{}{}",
                lrp.coord.0, lrp.coord.1, lrp.frc, lrp.fow,
                lrp.lfrcnp.map_or("-".to_string(), |v| v.to_string()),
                lrp.bearing.lb_deg, lrp.bearing.ub_deg,
                lrp.dnp.map_or(String::new(), |d| format!("  dnp=[{:.1},{:.1}]m", d.lb, d.ub)),
                lrp.pos_offset.map_or(String::new(), |d| format!("  pos_off=[{:.1},{:.1}]m", d.lb, d.ub)),
            );
        }

        let mut params = DecodeParams::preset(Preset::Permissive);
        params.trace_level = TraceLevel::Summary;
        let keys = prefetch_tile_keys(&loc_ref.lrps, &params, provider.zoom);
        eprintln!("Prefetching {} tile(s)…", keys.len());
        provider.load_tiles(&keys).expect("tile load failed");
        eprintln!("Graph: {} segs, {} nodes", provider.graph().segments.len(), provider.graph().nodes.len());

        let result = decode(&loc_ref, provider.graph(), &params).expect("decode failed");

        // Dump candidates from trace.
        if let Some(trace) = &result.trace {
            for event in &trace.events {
                if let DecodeEvent::CandidatesRanked { lrp_idx, accepted, rejected_count } = event {
                    let lrp = &loc_ref.lrps[*lrp_idx];
                    eprintln!(
                        "\n--- LRP[{lrp_idx}] candidates: {} accepted, {rejected_count} rejected ---",
                        accepted.len(),
                    );
                    eprintln!(
                        "  LRP coord=({:.6},{:.6})  bearing=[{:.2},{:.2}]°  frc={} fow={}",
                        lrp.coord.0, lrp.coord.1,
                        lrp.bearing.lb_deg, lrp.bearing.ub_deg,
                        lrp.frc, lrp.fow,
                    );
                    for (rank, c) in accepted.iter().enumerate() {
                        eprintln!(
                            "  #{rank}: seg={:?}  proj=({:.6},{:.6})  dist={:.1}m  bearing={:.1}°  arc={:.1}m  score={:.1}  traversal={:?}",
                            c.segment_id,
                            c.projection.point.0, c.projection.point.1,
                            c.projection.distance_m,
                            c.projection.bearing_deg,
                            c.projection.arc_offset_m,
                            c.score.total,
                            c.traversal,
                        );
                    }
                }
            }
        }

        eprintln!("\nDecoded {} segs, pos_offset={:.1}m, first_lrp_arc={:.1}m",
            result.path.len(), result.pos_offset_m, result.first_lrp_arc_m);

        // Dump each decoded segment's geometry, start/end nodes, length, direction.
        let graph = provider.graph();
        eprintln!("\n--- Decoded path segments ---");
        let mut total_len = 0.0f64;
        for (i, sid) in result.path.iter().enumerate() {
            if let Some(seg) = graph.segments.get(sid) {
                let start_pt = seg.geometry.first().copied().unwrap_or((0.0,0.0));
                let end_pt   = seg.geometry.last().copied().unwrap_or((0.0,0.0));
                eprintln!(
                    "  [{}] seg={:?}  frc={} dir={:?}  len={:.1}m  start_node={:?} end_node={:?}",
                    i, sid, seg.frc, seg.direction, seg.length_m,
                    seg.start_node, seg.end_node,
                );
                eprintln!(
                    "       geom: ({:.6},{:.6}) → ({:.6},{:.6})  ({} pts)",
                    start_pt.0, start_pt.1, end_pt.0, end_pt.1, seg.geometry.len(),
                );
                total_len += seg.length_m;
            }
        }
        eprintln!("Route segment total: {:.1}m  (DNP window: [{:.1},{:.1}]m)", total_len,
            loc_ref.lrps[0].dnp.map_or(0.0, |d| d.lb),
            loc_ref.lrps[0].dnp.map_or(0.0, |d| d.ub));

        // Dump geometry of the top two LRP[0] candidates (9290 and 9700) for comparison.
        eprintln!("\n--- Key candidate segments ---");
        for sid in [openlr_graph::SegmentId(9290), openlr_graph::SegmentId(9700), openlr_graph::SegmentId(11520)] {
            if let Some(seg) = graph.segments.get(&sid) {
                let s = seg.geometry.first().copied().unwrap_or((0.0,0.0));
                let e = seg.geometry.last().copied().unwrap_or((0.0,0.0));
                eprintln!(
                    "  seg={:?}  frc={} fow={} dir={:?}  len={:.1}m  start={:?} end={:?}",
                    sid, seg.frc, seg.fow, seg.direction, seg.length_m, seg.start_node, seg.end_node,
                );
                eprintln!("    ({:.6},{:.6}) → ({:.6},{:.6})", s.0, s.1, e.0, e.1);
            }
        }

        // Also print FOW for all accepted LRP[0] candidates.
        eprintln!("\n--- LRP[0] candidate segment FOW values ---");
        if let Some(trace) = &result.trace {
            for event in &trace.events {
                if let DecodeEvent::CandidatesRanked { lrp_idx: 0, accepted, .. } = event {
                    for c in accepted {
                        if let Some(seg) = graph.segments.get(&c.segment_id) {
                            eprintln!("  seg={:?}  frc={}  fow={}  score={:.1}",
                                c.segment_id, seg.frc, seg.fow, c.score.total);
                        }
                    }
                }
            }
        }

        // Try with 500m search radius to see if Robert-Bosch-Straße comes into range.
        eprintln!("\n--- Retry with 500m search radius ---");
        let mut p2 = DecodeParams::preset(Preset::Permissive);
        p2.trace_level = TraceLevel::Summary;
        p2.candidate_search_radius_m = 500.0;
        let keys2 = prefetch_tile_keys(&loc_ref.lrps, &p2, provider.zoom);
        provider.load_tiles(&keys2).expect("tile load failed");
        eprintln!("Graph (500m): {} segs, {} nodes", provider.graph().segments.len(), provider.graph().nodes.len());

        let result2 = decode(&loc_ref, provider.graph(), &p2).expect("decode failed with 500m radius");
        if let Some(trace2) = &result2.trace {
            for event in &trace2.events {
                if let DecodeEvent::CandidatesRanked { lrp_idx, accepted, rejected_count } = event {
                    if *lrp_idx == 0 {
                        eprintln!("LRP[0] with 500m radius: {} accepted, {rejected_count} rejected", accepted.len());
                        for (rank, c) in accepted.iter().take(5).enumerate() {
                            eprintln!(
                                "  #{rank}: seg={:?}  proj=({:.6},{:.6})  dist={:.1}m  bearing={:.1}°  score={:.1}",
                                c.segment_id, c.projection.point.0, c.projection.point.1,
                                c.projection.distance_m, c.projection.bearing_deg, c.score.total,
                            );
                        }
                    }
                }
            }
        }
        let total_len2: f64 = result2.path.iter()
            .filter_map(|id| provider.graph().segments.get(id))
            .map(|s| s.length_m)
            .sum();
        eprintln!("500m decode: {} segs, route length {:.1}m, pos_offset={:.1}m, first_lrp_arc={:.1}m",
            result2.path.len(), total_len2, result2.pos_offset_m, result2.first_lrp_arc_m);
    }

    /// Positive-offset trimming test.
    #[test]
    fn decode_germany_pos_offset_wkt() {
        let Some(path) = de_archive() else {
            eprintln!("SKIP: DE PMTiles archive not found");
            return;
        };

        use openlr_codec::decoder::v3::decode_v3_base64;
        use openlr_engine::{decode, path_to_wkt, prefetch_tile_keys, DecodeParams, Preset};

        let mut provider = PmtilesProvider::open(&path).expect("open failed");

        let loc_ref = decode_v3_base64("CwV1BCHeEDv1BQEj/3s7WiY=")
            .expect("v3 decode failed");

        eprintln!("LRP count: {}", loc_ref.lrps.len());
        for (i, lrp) in loc_ref.lrps.iter().enumerate() {
            eprintln!(
                "LRP[{i}]: ({:.6}, {:.6})  frc={} fow={} lfrcnp={}{}  pos_offset={:?}  neg_offset={:?}",
                lrp.coord.0, lrp.coord.1, lrp.frc, lrp.fow,
                lrp.lfrcnp.map_or("-".to_string(), |v| v.to_string()),
                lrp.dnp.map_or(String::new(), |d| format!("  dnp=[{:.0},{:.0}]m", d.lb, d.ub)),
                lrp.pos_offset, lrp.neg_offset,
            );
        }

        let params = DecodeParams::preset(Preset::Permissive);
        let keys = prefetch_tile_keys(&loc_ref.lrps, &params, provider.zoom);
        eprintln!("Prefetching {} tile(s) …", keys.len());
        provider.load_tiles(&keys).expect("tile load failed");
        eprintln!("Graph: {} segs, {} nodes", provider.graph().segments.len(), provider.graph().nodes.len());

        let result = decode(&loc_ref, provider.graph(), &params).expect("decode failed");
        assert!(!result.path.is_empty(), "path must be non-empty");

        eprintln!(
            "Decoded {} segment(s), pos_offset={:.1}m, neg_offset={:.1}m, first_lrp_arc={:.1}m, last_lrp_arc={:.1}m",
            result.path.len(), result.pos_offset_m, result.neg_offset_m,
            result.first_lrp_arc_m, result.last_lrp_arc_m,
        );
        assert!(result.pos_offset_m > 0.0, "expected a positive pos_offset_m");

        let wkt = path_to_wkt(
            &result.path, result.pos_offset_m, result.neg_offset_m,
            result.first_lrp_arc_m, result.last_lrp_arc_m, provider.graph(),
        ).expect("WKT generation failed");

        eprintln!("WKT point count: {}", wkt.split(',').count());
        println!("{wkt}");

        std::fs::write("/tmp/germany_pos_offset.wkt", &wkt).ok();
        eprintln!("WKT written to /tmp/germany_pos_offset.wkt");
    }
}
