//! WebAssembly bindings for the OpenLRLens decode engine.
//!
//! # JS usage pattern
//!
//! ```js
//! import init, { Decoder } from './openlr_wasm.js';
//! await init();
//!
//! const dec = new Decoder();
//!
//! // 1. Parse the reference and learn which tiles are needed.
//! const { tiles } = JSON.parse(dec.start("CwRbnh...", JSON.stringify(params), 12));
//! // tiles: [[z, x, y], ...]
//!
//! // 2. Fetch each tile from the PMTiles archive and inject it.
//! for (const [z, x, y] of tiles) {
//!     const bytes = await pmtilesSource.getZxy(z, x, y);
//!     if (bytes) dec.load_tile(z, x, y, new Uint8Array(bytes));
//! }
//!
//! // 3. Run the decode.
//! const result = JSON.parse(dec.decode());
//! if (result.ok) {
//!     console.log(result.wkt);          // "LINESTRING (...)"
//!     console.log(result.segments);     // [{ frc, fow, osm_way_id }, ...]
//! }
//! ```

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn warn(s: &str);
    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
}

use openlr_codec::{decode_v3_base64, decode_tpeg_hex, decode_tpeg_base64};
use openlr_codec::lrp::{LocationReference, LocationType, Orientation, SideOfRoad};
use openlr_engine::{decode as engine_decode, decode_forced as engine_decode_forced, DecodeError, DecodeParams, Preset, prefetch_tile_keys, path_to_wkt, path_band_wkt};
use openlr_engine::{ScoredCandidate, ProjectionResult, CandidateScore};
use openlr_graph::{SegmentId, NodeId};
use openlr_graph::{polyline_length_m, haversine_m, Direction};
use openlr_engine::trace::TraversalDir;
use openlr_provider::TileLoader;
use serde::Serialize;

// ── Module init ───────────────────────────────────────────────────────────────

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

// ── JS-visible result types ───────────────────────────────────────────────────

/// Returned by `Decoder.start()` as a JSON string.
#[derive(Serialize)]
struct StartResult {
    /// Tiles to fetch before calling `decode()`.  Each entry is `[z, x, y]`.
    tiles: Vec<[u32; 3]>,
}

/// Returned by `Decoder.decode()` when A* needs a tile that has not been loaded yet.
/// JS must load the tile via `load_tile()` and call `decode()` again.
#[derive(Serialize)]
struct NeedsTileResult {
    needs_tile: [u32; 3],
}

/// Per-segment metadata included in a successful `DecodeResult`.
#[derive(Serialize)]
struct SegmentInfo {
    frc: u8,
    fow: u8,
    /// Traversal direction: "Both", "Forward", or "Backward".
    direction: &'static str,
    /// Segment length in metres (precomputed; not re-derived from geometry).
    length_m: f64,
    /// OSM way ID, present when the tile was built from OSM data.
    #[serde(skip_serializing_if = "Option::is_none")]
    osm_way_id: Option<i64>,
    /// Source tile key, e.g. `"12/2135/1425"`.  Used by the UI to highlight the segment.
    tile: String,
    /// Segment's index within its source tile (matches the GeoJSON `local_index` property).
    local_index: u32,
    /// Internal graph segment ID assigned during tile loading.  Matches the `segment_id`
    /// values in the decode trace (candidate rankings, routing events).
    segment_id: u32,
    /// Geometry as `[[lon, lat], ...]` — used by the UI to draw a dedicated highlight layer.
    geometry: Vec<[f64; 2]>,
}

/// Per-LRP metadata included in every `DecodeResult` (success or failure).
#[derive(Serialize)]
struct LrpInfo {
    lon: f64,
    lat: f64,
    frc: u8,
    fow: u8,
    /// Absent on the last LRP.
    #[serde(skip_serializing_if = "Option::is_none")]
    lfrcnp: Option<u8>,
    bearing_lb: f64,
    bearing_ub: f64,
    /// Distance-to-next-point interval in metres. Absent on the last LRP.
    #[serde(skip_serializing_if = "Option::is_none")]
    dnp_lb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dnp_ub: Option<f64>,
    /// Snap point on the matched segment (lon, lat). Absent on decode failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    snap_lon: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snap_lat: Option<f64>,
    /// True when snap landed on a segment endpoint node; false for interior projection.
    #[serde(skip_serializing_if = "Option::is_none")]
    snap_is_endpoint: Option<bool>,
    /// Distance from encoded LRP coordinate to snap point, metres.
    #[serde(skip_serializing_if = "Option::is_none")]
    snap_distance_m: Option<f64>,
}

fn lrp_info_vec(
    lrps: &[openlr_codec::lrp::Lrp],
    snap_points: &[(f64, f64)],
    snap_is_endpoint: &[bool],
    snap_distances_m: &[f64],
) -> Vec<LrpInfo> {
    lrps.iter().enumerate().map(|(i, lrp)| LrpInfo {
        lon: lrp.coord.0,
        lat: lrp.coord.1,
        frc: lrp.frc,
        fow: lrp.fow,
        lfrcnp: lrp.lfrcnp,
        bearing_lb: lrp.bearing.lb_deg,
        bearing_ub: lrp.bearing.ub_deg,
        dnp_lb: lrp.dnp.map(|d| d.lb),
        dnp_ub: lrp.dnp.map(|d| d.ub),
        snap_lon: snap_points.get(i).map(|p| p.0),
        snap_lat: snap_points.get(i).map(|p| p.1),
        snap_is_endpoint: snap_is_endpoint.get(i).copied(),
        snap_distance_m: snap_distances_m.get(i).copied(),
    }).collect()
}

/// Returned by `Decoder.decode()` as a JSON string.
#[derive(Serialize)]
struct DecodeResult {
    ok: bool,
    /// "TomTomV3" or "Tpeg". Empty string on parse error.
    format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    wkt: Option<String>,
    segments: Vec<SegmentInfo>,
    lrps: Vec<LrpInfo>,
    /// [LB, UB] of the positive offset interval. Both 0 when no pos offset.
    pos_offset_lb: f64,
    pos_offset_ub: f64,
    /// [LB, UB] of the negative offset interval. Both 0 when no neg offset.
    neg_offset_lb: f64,
    neg_offset_ub: f64,
    /// True when offset bounds were estimated from DNP sum (decode failed, path length unknown).
    /// False when exact (decode succeeded and actual path length was used).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    offsets_approximate: bool,
    /// Conservative WKT trimmed at LB (maximal coverage). Used by the copy button.
    #[serde(skip_serializing_if = "Option::is_none")]
    conservative_wkt: Option<String>,
    /// WKT of the v3 uncertainty cap at the path head (LB→UB). Absent when LB==UB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pos_uncertainty_wkt: Option<String>,
    /// WKT of the v3 uncertainty cap at the path tail (end−UB → end−LB). Absent when LB==UB.
    #[serde(skip_serializing_if = "Option::is_none")]
    neg_uncertainty_wkt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Full decode trace; null when `trace_level` is `Off` or on error.
    #[serde(skip_serializing_if = "Option::is_none")]
    trace: Option<serde_json::Value>,
    // ── PointAlongLine ─────────────────────────────────────────────────────────
    /// "Line" or "PointAlongLine".
    location_type: String,
    /// Decoded point coordinate for PointAlongLine. Absent for line locations.
    #[serde(skip_serializing_if = "Option::is_none")]
    point_lon: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    point_lat: Option<f64>,
    /// PAL orientation: "NoOrientation" | "FirstTowardSecond" | "SecondTowardFirst" | "BothDirections"
    #[serde(skip_serializing_if = "Option::is_none")]
    orientation: Option<String>,
    /// PAL side of road: "DirectlyOnOrNA" | "Right" | "Left" | "Both"
    #[serde(skip_serializing_if = "Option::is_none")]
    side_of_road: Option<String>,
}

impl DecodeResult {
    fn err(msg: impl Into<String>) -> Self {
        DecodeResult {
            ok: false,
            format: String::new(),
            wkt: None,
            segments: vec![],
            lrps: vec![],
            pos_offset_lb: 0.0,
            pos_offset_ub: 0.0,
            neg_offset_lb: 0.0,
            neg_offset_ub: 0.0,
            offsets_approximate: false,
            conservative_wkt: None,
            pos_uncertainty_wkt: None,
            neg_uncertainty_wkt: None,
            error: Some(msg.into()),
            trace: None,
            location_type: "Line".to_string(),
            point_lon: None,
            point_lat: None,
            orientation: None,
            side_of_road: None,
        }
    }
}

// ── Forced-decode snap descriptor ────────────────────────────────────────────

/// One pre-selected snap point, passed in `decode_forced()`.
#[derive(serde::Deserialize)]
struct SnapDescriptor {
    segment_id: u32,
    traversal: String,   // "Forward" or "Backward"
    arc_offset_m: f64,
    snap_lon: f64,
    snap_lat: f64,
}

// ── Decoder ───────────────────────────────────────────────────────────────────

/// Stateful decode session.  Create one per reference string, or call `reset()`
/// between decodes if you want to reuse the loaded tile cache.
#[wasm_bindgen]
pub struct Decoder {
    loader: TileLoader,
    location_ref: Option<LocationReference>,
    params: DecodeParams,
    zoom: u8,
    openlr_format: &'static str,
}

#[wasm_bindgen]
impl Decoder {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Decoder {
        Decoder {
            loader: TileLoader::new(),
            location_ref: None,
            params: DecodeParams::default(),
            zoom: 12,
            openlr_format: "",
        }
    }

    /// Parse `openlr_string` (auto-detects OpenLR binary v3 base64 or TPEG-OLR hex),
    /// store the decode parameters, and compute the set of tiles that must be loaded.
    ///
    /// `params_json`: JSON-serialized `DecodeParams`, or `""` / `"null"` for defaults.
    /// `zoom`: tile zoom level (must match the PMTiles archive; typically 12).
    ///
    /// Returns a JSON string: `{ "tiles": [[z, x, y], ...] }`.
    /// Throws a JS error string on parse failure.
    pub fn start(&mut self, openlr_string: &str, params_json: &str, zoom: u8) -> Result<String, JsValue> {
        let params: DecodeParams = match params_json {
            "" | "null" | "Default" => DecodeParams::default(),
            "Permissive" => DecodeParams::preset(Preset::Permissive),
            "Strict"     => DecodeParams::preset(Preset::Strict),
            other => serde_json::from_str(other)
                .map_err(|e| JsValue::from_str(&format!("invalid params: {e}")))?,
        };

        let (loc_ref, fmt) = parse_openlr(openlr_string)
            .map_err(|e| JsValue::from_str(&e))?;

        let tile_keys = prefetch_tile_keys(&loc_ref.lrps, &params, zoom);
        let tiles: Vec<[u32; 3]> = tile_keys
            .iter()
            .map(|k| [k.z as u32, k.x, k.y])
            .collect();

        self.location_ref = Some(loc_ref);
        self.params = params;
        self.zoom = zoom;
        self.openlr_format = fmt;

        Ok(serde_json::to_string(&StartResult { tiles }).unwrap())
    }

    /// Inject one tile's raw OLRL bytes into the graph.  Call once per tile
    /// returned by `start()`.  Missing tiles are silently skipped — decode will
    /// simply have fewer candidates near those coordinates.
    ///
    /// Throws a JS error string if the tile payload is malformed.
    pub fn load_tile(&mut self, z: u8, x: u32, y: u32, data: &[u8]) -> Result<(), JsValue> {
        self.loader
            .load_tile_at(z, x, y, data)
            .map_err(|e| JsValue::from_str(&format!("tile parse error: {e}")))
    }

    /// Run the decode against the loaded graph.
    ///
    /// Returns a JSON string.  On success:
    /// ```json
    /// { "ok": true, "wkt": "LINESTRING (...)", "segments": [...],
    ///   "pos_offset_m": 0.0, "neg_offset_m": 0.0, "trace": {...} }
    /// ```
    /// On failure:
    /// ```json
    /// { "ok": false, "error": "LRP 0: no candidate segments found", "segments": [] }
    /// ```
    pub fn decode(&self) -> String {
        let loc_ref = match &self.location_ref {
            Some(r) => r,
            None => return serde_json::to_string(&DecodeResult::err("call start() first")).unwrap(),
        };

        let result = match engine_decode(loc_ref, &self.loader.graph, &self.params, self.zoom) {
            Err(failure) => {
                // A* needs a tile that hasn't been loaded yet — not a permanent failure.
                // Return a distinct signal so JS can load the tile and retry decode().
                if let DecodeError::NeedsTile(tk) = failure.error {
                    return serde_json::to_string(&NeedsTileResult {
                        needs_tile: [tk.z as u32, tk.x, tk.y],
                    }).unwrap();
                }
                // For OffsetOverflow the route was fully found; carry the path so the JS
                // diagnostic layer can still access per-segment lengths.
                let overflow_path: Option<Vec<SegmentId>> =
                    if let DecodeError::OffsetOverflow { ref path, .. } = failure.error {
                        Some(path.clone())
                    } else {
                        None
                    };
                let error_str = failure.error.to_string();
                let trace_value = failure.trace.and_then(|t| {
                    // Fast path: serialise the whole trace at once.
                    if let Ok(val) = serde_json::to_value(&t) {
                        return Some(val);
                    }
                    // Slow path: NaN/Inf in some event field.  Serialise events one by one,
                    // dropping the offending ones.  Params are always finite, so they succeed.
                    warn("openlrlens: trace has non-finite floats; retrying per-event");
                    let n_total = t.events.len();
                    let events: Vec<serde_json::Value> = t.events.iter()
                        .filter_map(|ev| serde_json::to_value(ev).ok())
                        .collect();
                    let skipped = n_total - events.len();
                    if skipped > 0 {
                        warn(&format!("openlrlens: dropped {skipped} trace events with non-finite floats"));
                    }
                    let params_val = serde_json::to_value(&t.params)
                        .unwrap_or(serde_json::Value::Null);
                    serde_json::to_value(serde_json::json!({
                        "events": events,
                        "params": params_val,
                    })).ok()
                });
                // For OffsetOverflow: build segments from the routed path so the JS
                // diagnostic layer can access per-segment lengths even though ok=false.
                let overflow_segments: Vec<SegmentInfo> = overflow_path
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|seg_id| {
                        self.loader.graph.segments.get(seg_id).map(|seg| {
                            let (tile, local_index) = self.loader.seg_tile.get(seg_id)
                                .map(|&(z, x, y, li)| (format!("{z}/{x}/{y}"), li))
                                .unwrap_or_else(|| ("unknown".to_string(), 0));
                            SegmentInfo {
                                frc: seg.frc,
                                fow: seg.fow,
                                direction: match seg.direction {
                                    Direction::Both     => "Both",
                                    Direction::Forward  => "Forward",
                                    Direction::Backward => "Backward",
                                },
                                length_m: (seg.length_m * 10.0).round() / 10.0,
                                osm_way_id: seg.osm_way_id(),
                                tile,
                                local_index,
                                segment_id: seg_id.0,
                                geometry: seg.geometry.iter()
                                    .map(|&(lon, lat)| [lon, lat])
                                    .collect(),
                            }
                        })
                    })
                    .collect();
                // Per spec §7.5.2: offset byte is relative to the first-leg DNP
                // (positive) or last-leg DNP (negative), not the total path length.
                // The second-to-last LRP holds the last leg's DNP.
                let n_lrps = loc_ref.lrps.len();
                let first_leg_dnp = loc_ref.lrps.first().and_then(|l| l.dnp);
                let last_leg_dnp  = loc_ref.lrps.get(n_lrps.saturating_sub(2)).and_then(|l| l.dnp);
                let (pos_offset_lb, pos_offset_ub, pos_approx) = approximate_offset(
                    loc_ref.lrps.first().and_then(|l| l.pos_offset_raw),
                    loc_ref.lrps.first().and_then(|l| l.pos_offset),
                    first_leg_dnp,
                );
                let (neg_offset_lb, neg_offset_ub, neg_approx) = approximate_offset(
                    loc_ref.lrps.last().and_then(|l| l.neg_offset_raw),
                    loc_ref.lrps.last().and_then(|l| l.neg_offset),
                    last_leg_dnp,
                );
                let full_result = DecodeResult {
                    lrps: lrp_info_vec(&loc_ref.lrps, &[], &[], &[]),
                    format: self.openlr_format.to_string(),
                    trace: trace_value,
                    segments: overflow_segments,
                    pos_offset_lb,
                    pos_offset_ub,
                    neg_offset_lb,
                    neg_offset_ub,
                    offsets_approximate: pos_approx || neg_approx,
                    ..DecodeResult::err(&error_str)
                };
                return match serde_json::to_string(&full_result) {
                    Ok(s) => s,
                    Err(e) => {
                        // LrpInfo contained a non-finite f64 — drop lrps/trace rather than panic.
                        warn(&format!("openlrlens: failure result serialisation failed ({e}); dropping lrps"));
                        serde_json::to_string(&DecodeResult::err(&error_str)).unwrap()
                    }
                };
            }
            Ok(r) => r,
        };

        self.build_ok_json(loc_ref, result)
    }

    /// Forced decode: bypass candidate selection and run routing with exactly the
    /// provided snap points (one per LRP).
    ///
    /// `snaps_json`: JSON array of snap descriptors:
    /// `[{ "segment_id": u32, "traversal": "Forward"|"Backward",
    ///     "arc_offset_m": f64, "snap_lon": f64, "snap_lat": f64 }, ...]`
    ///
    /// Precondition: `start()` must have been called for the current reference.
    /// The tile graph from the previous decode is reused; additional tiles are
    /// loaded on demand if A* discovers them.
    ///
    /// Returns the same JSON schema as `decode()`.
    pub fn decode_forced(&self, snaps_json: &str) -> String {
        let loc_ref = match &self.location_ref {
            Some(r) => r,
            None => return serde_json::to_string(&DecodeResult::err("call start() first")).unwrap(),
        };

        let snaps: Vec<SnapDescriptor> = match serde_json::from_str(snaps_json) {
            Ok(v) => v,
            Err(e) => return serde_json::to_string(
                &DecodeResult::err(format!("invalid snaps: {e}"))).unwrap(),
        };

        if snaps.len() != loc_ref.lrps.len() {
            return serde_json::to_string(&DecodeResult::err(format!(
                "expected {} snaps (one per LRP), got {}", loc_ref.lrps.len(), snaps.len()
            ))).unwrap();
        }

        let forced: Vec<ScoredCandidate> = match snaps.iter().map(|desc| {
            let seg_id = SegmentId(desc.segment_id);
            let seg = self.loader.graph.segments.get(&seg_id)
                .ok_or_else(|| format!("segment {} not in loaded graph", desc.segment_id))?;
            let traversal = match desc.traversal.as_str() {
                "Backward" => TraversalDir::Backward,
                _          => TraversalDir::Forward,
            };
            let (entry_node, exit_node) = match traversal {
                TraversalDir::Backward => (seg.end_node, seg.start_node),
                TraversalDir::Forward  => (seg.start_node, seg.end_node),
            };
            Ok(ScoredCandidate {
                segment_id: seg_id,
                traversal,
                projection: ProjectionResult {
                    arc_offset_m: desc.arc_offset_m,
                    point:        (desc.snap_lon, desc.snap_lat),
                    distance_m:   0.0,
                    bearing_deg:  0.0,
                    is_at_entry:  false,
                    is_at_exit:   false,
                },
                score: CandidateScore {
                    distance_score:       0.0,
                    bearing_score:        0.0,
                    frc_score:            0.0,
                    fow_score:            0.0,
                    interior_score:       0.0,
                    wrong_endpoint_score: 0.0,
                    total:                0.0,
                },
                entry_node,
                exit_node,
            })
        }).collect::<Result<Vec<_>, String>>() {
            Ok(v)  => v,
            Err(e) => return serde_json::to_string(&DecodeResult::err(e)).unwrap(),
        };

        match engine_decode_forced(loc_ref, forced, &self.loader.graph, &self.params, self.zoom) {
            Err(failure) => {
                if let DecodeError::NeedsTile(tk) = failure.error {
                    return serde_json::to_string(&NeedsTileResult {
                        needs_tile: [tk.z as u32, tk.x, tk.y],
                    }).unwrap();
                }
                let error_str = failure.error.to_string();
                let trace_value = failure.trace.and_then(|t| serde_json::to_value(t).ok());
                serde_json::to_string(&DecodeResult {
                    lrps:  lrp_info_vec(&loc_ref.lrps, &[], &[], &[]),
                    format: self.openlr_format.to_string(),
                    trace: trace_value,
                    ..DecodeResult::err(&error_str)
                }).unwrap()
            }
            Ok(result) => self.build_ok_json(loc_ref, result),
        }
    }

    /// Clear the stored location reference.  The loaded tile graph is kept so
    /// nearby re-decodes can reuse the cached tiles — call `reset_tiles()` too
    /// if you want to start completely fresh.
    pub fn reset(&mut self) {
        self.location_ref = None;
    }

    /// Drop all loaded tiles and the stored location reference.
    pub fn reset_tiles(&mut self) {
        self.loader = TileLoader::new();
        self.location_ref = None;
    }

    /// Tile zoom level in use (set by `start()`).
    pub fn zoom(&self) -> u8 {
        self.zoom
    }

    /// Return the internal graph segment ID for the segment at `(z, x, y, local_index)`,
    /// or -1 if that tile/index combination is not currently loaded.
    /// Useful for correlating map-click segments with trace log `segment_id` values.
    ///
    /// Returns `f64` rather than `i64` so JS receives a plain Number (not BigInt).
    /// All segment IDs are u32-bounded, so no precision is lost.
    pub fn segment_id_at(&self, z: u8, x: u32, y: u32, local_index: u32) -> f64 {
        self.loader.seg_tile.iter()
            .find(|(_, &(sz, sx, sy, sl))| sz == z && sx == x && sy == y && sl == local_index)
            .map(|(id, _)| id.0 as f64)
            .unwrap_or(-1.0)
    }

    /// Return all loaded segment→tile mappings as a JSON string.
    ///
    /// Each entry is `[segment_id, z, x, y, local_index]`.  This is the O(n) alternative
    /// to calling `segment_id_at` in a JS loop (which is O(n²) due to repeated linear scans).
    ///
    /// Used by the JS layer to build its segment_id → tile reverse-lookup map.
    pub fn all_segment_tile_mappings(&self) -> String {
        let mappings: Vec<[u32; 5]> = self.loader.seg_tile.iter()
            .map(|(id, &(z, x, y, li))| [id.0, z as u32, x, y, li])
            .collect();
        serde_json::to_string(&mappings).unwrap()
    }

    /// Return how many segments were loaded from tile `(z, x, y)`, or 0 if not loaded.
    pub fn tile_segment_count(&self, z: u8, x: u32, y: u32) -> u32 {
        self.loader.seg_tile.values()
            .filter(|&&(sz, sx, sy, _)| sz == z && sx == x && sy == y)
            .count() as u32
    }

    /// Number of segments in the loaded graph.
    pub fn loaded_segment_count(&self) -> usize {
        self.loader.graph.segments.len()
    }

    /// Number of nodes in the loaded graph.
    pub fn loaded_node_count(&self) -> usize {
        self.loader.graph.nodes.len()
    }

    // ── LLM diagnostic tool methods ───────────────────────────────────────────

    /// Return full attributes + geometry for one segment by its graph segment ID.
    /// Returns `{"error": "..."}` if the segment is not in the loaded tile set.
    pub fn get_segment(&self, segment_id: u32) -> String {
        let seg_id = SegmentId(segment_id);
        match self.loader.graph.segments.get(&seg_id) {
            None => serde_json::json!({
                "error": format!("segment {} not found in loaded tiles", segment_id)
            }).to_string(),
            Some(seg) => {
                let (tile, local_index) = self.loader.seg_tile.get(&seg_id)
                    .map(|&(z, x, y, li)| (format!("{z}/{x}/{y}"), li))
                    .unwrap_or_else(|| ("unknown".to_string(), 0));
                serde_json::json!({
                    "segment_id": segment_id,
                    "source_key": segment_source_key(&seg.stable_id),
                    "frc": seg.frc,
                    "fow": seg.fow,
                    "direction": match seg.direction {
                        Direction::Both     => "Both",
                        Direction::Forward  => "Forward",
                        Direction::Backward => "Backward",
                    },
                    "length_m":     (seg.length_m * 10.0).round() / 10.0,
                    "start_node":   seg.start_node.0,
                    "end_node":     seg.end_node.0,
                    "tile":         tile,
                    "local_index":  local_index,
                    "vertex_count": seg.geometry.len(),
                    "geometry":     seg.geometry.iter().map(|&(lon, lat)| [lon, lat]).collect::<Vec<_>>(),
                }).to_string()
            }
        }
    }

    /// Find segments in the loaded graph whose geometry comes within `radius_m` of (lat, lon).
    /// Results are sorted by distance and capped at 20.  Caps radius at 500 m.
    pub fn get_segments_near(&self, lat: f64, lon: f64, radius_m: f64) -> String {
        let cap = radius_m.min(500.0);
        let mut hits: Vec<(f64, u32, u8, u8, &'static str, f64, Option<String>)> = self.loader.graph.segments.iter()
            .filter_map(|(seg_id, seg)| {
                let min_dist = seg.geometry.iter()
                    .map(|&(slon, slat)| haversine_m(slon, slat, lon, lat))
                    .fold(f64::INFINITY, f64::min);
                if min_dist <= cap {
                    let dir_str: &'static str = match seg.direction {
                        Direction::Both     => "Both",
                        Direction::Forward  => "Forward",
                        Direction::Backward => "Backward",
                    };
                    Some((min_dist, seg_id.0, seg.frc, seg.fow, dir_str, seg.length_m, segment_source_key(&seg.stable_id)))
                } else {
                    None
                }
            })
            .collect();
        hits.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let segments: Vec<serde_json::Value> = hits.iter().take(20).map(|(dist, id, frc, fow, dir, len, src_key)| {
            serde_json::json!({
                "segment_id":  id,
                "source_key":  src_key,
                "frc":         frc,
                "fow":         fow,
                "direction":   dir,
                "length_m":    (len * 10.0).round() / 10.0,
                "distance_m":  (dist * 10.0).round() / 10.0,
            })
        }).collect();
        serde_json::json!({
            "query": { "lat": lat, "lon": lon, "radius_m": cap },
            "count": segments.len(),
            "segments": segments,
        }).to_string()
    }

    /// Return all segments connected at each endpoint of `segment_id`.
    ///
    /// Reports two groups — `at_start_node` and `at_end_node` — each listing every other
    /// segment that shares that node.  For each neighbour, `can_arrive` indicates whether a
    /// traversal of that segment can *end* at the node; `can_depart` indicates whether it can
    /// *begin* there.  Turn-restriction flags cover both transition directions through the node.
    ///
    /// This is direction-neutral and correct for bidirectional segments: a `Both` segment has
    /// two valid traversal directions, so each endpoint is simultaneously an entry and an exit.
    pub fn get_segment_neighbors(&self, segment_id: u32) -> String {
        let seg_id = SegmentId(segment_id);
        let seg = match self.loader.graph.segments.get(&seg_id) {
            Some(s) => s,
            None => return serde_json::json!({
                "error": format!("Segment {segment_id} not found in loaded graph.")
            }).to_string(),
        };

        let start_node = seg.start_node;
        let end_node   = seg.end_node;

        // Build neighbour entries for a given node id.
        // `can_arrive`  = other's traversal can end at `node`
        // `can_depart`  = other's traversal can begin at `node`
        // Both are true for Direction::Both.
        let mut build_entries = |node: NodeId| -> Vec<serde_json::Value> {
            let mut entries = Vec::new();
            for (&other_id, other) in &self.loader.graph.segments {
                if other_id == seg_id { continue; }
                let touches_node = other.start_node == node || other.end_node == node;
                if !touches_node { continue; }

                let dir_str: &'static str = match other.direction {
                    Direction::Both     => "Both",
                    Direction::Forward  => "Forward",
                    Direction::Backward => "Backward",
                };
                // Forward/Both traversal: start_node→end_node.  Departs from start_node, arrives at end_node.
                // Backward/Both traversal: end_node→start_node. Departs from end_node, arrives at start_node.
                let can_arrive = (matches!(other.direction, Direction::Forward  | Direction::Both) && other.end_node   == node)
                              || (matches!(other.direction, Direction::Backward | Direction::Both) && other.start_node == node);
                let can_depart = (matches!(other.direction, Direction::Forward  | Direction::Both) && other.start_node == node)
                              || (matches!(other.direction, Direction::Backward | Direction::Both) && other.end_node   == node);

                // Turn restrictions in both directions through this node.
                let restricted_into_self  = can_arrive  && self.loader.graph.is_restricted(other_id, node, seg_id);
                let restricted_from_self  = can_depart  && self.loader.graph.is_restricted(seg_id,   node, other_id);

                entries.push(serde_json::json!({
                    "segment_id":            other_id.0,
                    "source_key":            segment_source_key(&other.stable_id),
                    "frc":                   other.frc,
                    "fow":                   other.fow,
                    "direction":             dir_str,
                    "length_m":              (other.length_m * 10.0).round() / 10.0,
                    "can_arrive":            can_arrive,
                    "can_depart":            can_depart,
                    "restricted_into_self":  restricted_into_self,
                    "restricted_from_self":  restricted_from_self,
                }));
            }
            entries
        };

        let at_start = build_entries(start_node);
        let at_end   = build_entries(end_node);

        serde_json::json!({
            "segment_id":  segment_id,
            "direction":   match seg.direction {
                Direction::Both     => "Both",
                Direction::Forward  => "Forward",
                Direction::Backward => "Backward",
            },
            "start_node": {
                "node_id":  start_node.0,
                "count":    at_start.len(),
                "segments": at_start,
            },
            "end_node": {
                "node_id":  end_node.0,
                "count":    at_end.len(),
                "segments": at_end,
            },
        }).to_string()
    }

    /// Re-run the decode with `params_override` merged over the current params.
    /// Tiles must already be loaded; returns an error if a new tile is required.
    /// Returns a compact comparison result — call get_decode_summary for full segment details.
    pub fn retry_decode(&mut self, params_override: &str) -> String {
        let loc_ref = match &self.location_ref {
            Some(r) => r,
            None => return serde_json::json!({"ok": false, "error": "no reference loaded; call start() first"}).to_string(),
        };
        let merged = match merge_params(&self.params, params_override) {
            Ok(p) => p,
            Err(e) => return serde_json::json!({"ok": false, "error": format!("invalid params override: {e}")}).to_string(),
        };
        // Temporarily apply merged params, decode, restore originals.
        let saved = std::mem::replace(&mut self.params, merged.clone());
        let raw = self.decode();
        self.params = saved;

        // Parse just the fields we need for a compact comparison response.
        let parsed: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return raw,
        };
        if parsed.get("needs_tile").is_some() {
            return serde_json::json!({
                "ok": false,
                "error": "retry requires a tile that is not loaded; re-run the full decode from the UI to load additional tiles",
                "params_applied": merged,
            }).to_string();
        }
        let ok = parsed["ok"].as_bool().unwrap_or(false);
        let seg_count = parsed["segments"].as_array().map(|a| a.len()).unwrap_or(0);
        let path_total: f64 = parsed["segments"].as_array()
            .map(|segs| segs.iter().filter_map(|s| s["length_m"].as_f64()).sum())
            .unwrap_or(0.0);
        serde_json::json!({
            "ok": ok,
            "error": parsed["error"],
            "segment_count": seg_count,
            "path_total_length_m": (path_total * 10.0).round() / 10.0,
            "lrp_count": parsed["lrps"].as_array().map(|a| a.len()),
            "params_applied": merged,
        }).to_string()
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Compute approximate offset bounds for a failed decode using the relevant leg's
/// DNP as the LRP_length proxy (spec §7.5.2). Returns (lb, ub, approximate).
/// When no offset is encoded returns (0.0, 0.0, false).
/// TPEG offsets are exact even on failure (`exact` is Some, `raw` is None).
fn approximate_offset(raw: Option<u8>, exact: Option<openlr_codec::LinearInterval>,
                      leg_dnp: Option<openlr_codec::LinearInterval>) -> (f64, f64, bool) {
    if let Some(n) = raw {
        let (dnp_lb, dnp_ub) = leg_dnp.map(|d| (d.lb, d.ub)).unwrap_or((0.0, 0.0));
        let lb = n as f64 / 256.0 * dnp_lb;
        let ub = (n as f64 + 1.0) / 256.0 * dnp_ub;
        (lb, ub, true)
    } else if let Some(i) = exact {
        (i.lb, i.ub, false)
    } else {
        (0.0, 0.0, false)
    }
}

impl Decoder {
    /// Build the JSON success response from a `DecodedLocation`.
    /// Shared by `decode()` and `decode_forced()`.
    fn build_ok_json(&self, loc_ref: &LocationReference, result: openlr_engine::DecodedLocation) -> String {
        let lrps = lrp_info_vec(
            &loc_ref.lrps,
            &result.lrp_snap_points,
            &result.lrp_snap_is_endpoint,
            &result.lrp_snap_distances_m,
        );

        let pos_int = result.pos_offset;
        let neg_int = result.neg_offset;
        let (pos_offset_lb, pos_offset_ub) = pos_int.map(|i| (i.lb, i.ub)).unwrap_or((0.0, 0.0));
        let (neg_offset_lb, neg_offset_ub) = neg_int.map(|i| (i.lb, i.ub)).unwrap_or((0.0, 0.0));

        let wkt = path_to_wkt(
            &result.path,
            pos_offset_lb,
            neg_offset_lb,
            result.first_lrp_arc_m,
            result.last_lrp_arc_m,
            result.first_seg_traversal,
            result.last_seg_traversal,
            &self.loader.graph,
        );

        let n_path = result.path.len();
        let segments: Vec<SegmentInfo> = result.path.iter().enumerate().filter_map(|(i, seg_id)| {
            self.loader.graph.segments.get(seg_id).map(|seg| {
                let (tile, local_index) = self.loader.seg_tile.get(seg_id)
                    .map(|&(z, x, y, li)| (format!("{z}/{x}/{y}"), li))
                    .unwrap_or_else(|| ("unknown".to_string(), 0));
                let traversal = if i == 0 {
                    result.first_seg_traversal
                } else if i == n_path - 1 {
                    result.last_seg_traversal
                } else {
                    TraversalDir::Forward
                };
                let geometry: Vec<[f64; 2]> = match traversal {
                    TraversalDir::Forward  => seg.geometry.iter().map(|&(lon, lat)| [lon, lat]).collect(),
                    TraversalDir::Backward => seg.geometry.iter().rev().map(|&(lon, lat)| [lon, lat]).collect(),
                };
                SegmentInfo {
                    frc: seg.frc,
                    fow: seg.fow,
                    direction: match seg.direction {
                        Direction::Both     => "Both",
                        Direction::Forward  => "Forward",
                        Direction::Backward => "Backward",
                    },
                    length_m: (seg.length_m * 10.0).round() / 10.0,
                    osm_way_id: seg.osm_way_id(),
                    tile,
                    local_index,
                    segment_id: seg_id.0,
                    geometry,
                }
            })
        }).collect();

        let actual_lens: Vec<f64> = result.path.iter()
            .filter_map(|id| self.loader.graph.segments.get(id))
            .map(|s| polyline_length_m(&s.geometry))
            .collect();
        let last_seg_len = actual_lens.last().copied().unwrap_or(0.0);

        let pos_uncertainty_wkt = pos_int
            .filter(|i| i.ub > i.lb)
            .and_then(|i| path_band_wkt(
                &result.path,
                result.first_lrp_arc_m + i.lb,
                result.first_lrp_arc_m + i.ub,
                result.first_seg_traversal,
                &self.loader.graph,
            ));

        let last_lrp_pos_from_start: f64 = actual_lens[..actual_lens.len().saturating_sub(1)]
            .iter().sum::<f64>() + result.last_lrp_arc_m.min(last_seg_len);
        let neg_uncertainty_wkt = neg_int
            .filter(|i| i.ub > i.lb)
            .and_then(|i| path_band_wkt(
                &result.path,
                (last_lrp_pos_from_start - i.ub).max(0.0),
                last_lrp_pos_from_start - i.lb,
                result.first_seg_traversal,
                &self.loader.graph,
            ));

        let trace_value = result.trace.and_then(|t| serde_json::to_value(t).ok());

        let location_type = if loc_ref.location_type == LocationType::PointAlongLine {
            "PointAlongLine".to_string()
        } else {
            "Line".to_string()
        };
        let (point_lon, point_lat) = result.point_coord
            .map(|(lon, lat)| (Some(lon), Some(lat)))
            .unwrap_or((None, None));
        let orientation = result.orientation.map(|o| match o {
            Orientation::NoOrientation       => "NoOrientation",
            Orientation::FirstTowardSecond   => "FirstTowardSecond",
            Orientation::SecondTowardFirst   => "SecondTowardFirst",
            Orientation::BothDirections      => "BothDirections",
        }.to_string());
        let side_of_road = result.side_of_road.map(|s| match s {
            SideOfRoad::DirectlyOnOrNA => "DirectlyOnOrNA",
            SideOfRoad::Right          => "Right",
            SideOfRoad::Left           => "Left",
            SideOfRoad::Both           => "Both",
        }.to_string());

        serde_json::to_string(&DecodeResult {
            ok: true,
            format: self.openlr_format.to_string(),
            wkt,
            segments,
            lrps,
            pos_offset_lb,
            pos_offset_ub,
            neg_offset_lb,
            neg_offset_ub,
            offsets_approximate: false,
            conservative_wkt: None,
            pos_uncertainty_wkt,
            neg_uncertainty_wkt,
            error: None,
            trace: trace_value,
            location_type,
            point_lon,
            point_lat,
            orientation,
            side_of_road,
        }).unwrap()
    }
}

// ── Segment source-key helper ─────────────────────────────────────────────────

/// Decode the human-readable source key from a segment's `stable_id`.
///
/// Layout (matching tileDecoder.js and the tile build pipeline):
///   bytes  0–7:  source integer (i64 LE) — OSM way id or similar
///   bytes  8–11: split index (u32 LE)    — 0 for unsplit segments
///   bytes 12–15: 0x00 for integer ids; non-zero for full GERS UUIDs
///
/// Returns `"{source_int}-{split_idx}"` for integer ids, `None` for GERS UUIDs and
/// all-zero synthetic ids.
fn segment_source_key(stable_id: &[u8; 16]) -> Option<String> {
    if stable_id[12..16] != [0u8; 4] { return None; }
    let source_int = i64::from_le_bytes(stable_id[0..8].try_into().unwrap());
    if source_int == 0 { return None; }
    let split_idx = u32::from_le_bytes(stable_id[8..12].try_into().unwrap());
    Some(format!("{source_int}-{split_idx}"))
}

// ── Param merge helper ────────────────────────────────────────────────────────

fn merge_params(base: &DecodeParams, override_json: &str) -> Result<DecodeParams, serde_json::Error> {
    let mut base_val = serde_json::to_value(base)?;
    let overlay: serde_json::Value = serde_json::from_str(override_json)?;
    if let (Some(base_obj), Some(overlay_obj)) = (base_val.as_object_mut(), overlay.as_object()) {
        for (k, v) in overlay_obj {
            base_obj.insert(k.clone(), v.clone());
        }
    }
    serde_json::from_value(base_val)
}

// ── Format auto-detection ─────────────────────────────────────────────────────

/// Try OpenLR binary v3 (base64) then TPEG-OLR (hex).  Returns `(LocationReference, format)`.
fn parse_openlr(s: &str) -> Result<(LocationReference, &'static str), String> {
    let has_base64_chars = s.chars().any(|c| c == '+' || c == '/' || c == '=');

    if has_base64_chars || looks_like_base64(s) {
        if let Ok(r) = decode_v3_base64(s)   { return Ok((r, "TomTomV3")); }
        if let Ok(r) = decode_tpeg_base64(s) { return Ok((r, "Tpeg")); }
    }

    if let Ok(r) = decode_tpeg_hex(s) { return Ok((r, "Tpeg")); }

    decode_v3_base64(s)
        .map(|r| (r, "TomTomV3"))
        .map_err(|e| format!("OpenLR parse error (tried v3 base64, TPEG base64, TPEG hex): {e}"))
}

fn looks_like_base64(s: &str) -> bool {
    // Heuristic: all chars are base64url-safe, and length is 4-byte aligned (with or without padding).
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && s.len() % 4 == 0
}
