pub mod astar;
pub mod candidate;
pub mod diagnostics;
pub mod params;
pub mod route_generator;
pub mod tile_prefetch;
pub mod trace;
pub mod validation;
pub mod wkt;

pub use params::{DecodeParams, Preset};
pub use route_generator::RouteGenerator;
pub use tile_prefetch::prefetch_tile_keys;
pub use trace::{DecodeEvent, DecodeOutcome, DecodeTrace, TraceLevel};
pub use trace::{ScoredCandidate, ProjectionResult, CandidateScore};
pub use wkt::{path_to_wkt, path_band_wkt};

use std::collections::HashMap;

use openlr_codec::interval::LinearInterval;
use openlr_codec::lrp::{LocationReference, LocationType, Orientation, SideOfRoad, Lrp};
use openlr_graph::{Graph, NodeId, SegmentId, TileKey};

use astar::find_route;
use candidate::select_candidates;
use route_generator::RouteGenerator as Gen;
use trace::{DecodeTrace as Trace, RoutingFailure, TraversalDir};
use validation::validate_dnp;

// (exit_node, entry_node, effective_lfrcnp) → None (A* found no path) or Some((segs, interior_length_m))
type RouteCache = HashMap<(NodeId, NodeId, u8), Option<(Vec<SegmentId>, f64)>>;

// ── Public result types ───────────────────────────────────────────────────────

/// The decoded location (line or point).
#[derive(Debug, Clone)]
pub struct DecodedLocation {
    /// Ordered segment IDs making up the matched path (includes partial first/last segments).
    pub path: Vec<SegmentId>,
    /// Positive offset interval — meters from the first LRP's position forward to the
    /// start of the location. None when no positive offset is encoded.
    pub pos_offset: Option<LinearInterval>,
    /// Negative offset interval — meters backward from the last LRP's position to the
    /// end of the location. None when no negative offset is encoded.
    pub neg_offset: Option<LinearInterval>,
    /// Arc offset of the first LRP on the first path segment, in the traversal direction (m).
    pub first_lrp_arc_m: f64,
    /// Arc offset of the last LRP on the last path segment, in the traversal direction (m).
    pub last_lrp_arc_m: f64,
    /// Traversal direction of path[0].
    pub first_seg_traversal: TraversalDir,
    /// Traversal direction of path[last].
    pub last_seg_traversal: TraversalDir,
    /// Projected (lon, lat) of the winning candidate's snap point for each LRP.
    pub lrp_snap_points: Vec<(f64, f64)>,
    /// Whether each LRP's winning snap was to a segment endpoint node (true) or interior (false).
    pub lrp_snap_is_endpoint: Vec<bool>,
    /// Distance from each encoded LRP coordinate to its snap point, meters.
    pub lrp_snap_distances_m: Vec<f64>,
    /// Trace of every decision made during this decode.
    /// `None` when `params.trace_level == TraceLevel::Off`.
    pub trace: Option<DecodeTrace>,
    // ── PointAlongLine-specific fields ────────────────────────────────────────
    /// The decoded point coordinate (lon, lat). Set for PAL; None for line locations.
    pub point_coord: Option<(f64, f64)>,
    /// PAL orientation attribute.
    pub orientation: Option<Orientation>,
    /// PAL side-of-road attribute.
    pub side_of_road: Option<SideOfRoad>,
}

/// Decode failure.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DecodeError {
    #[error("LRP {0}: no candidate segments found")]
    NoCandidates(usize),
    #[error("leg {0}: routing failed — {1}")]
    RoutingFailed(usize, String),
    #[error("leg {0}: all candidate combinations exhausted")]
    AllCombinationsFailed(usize),
    /// A* reached a boundary node whose home tile is not loaded.
    /// Not a permanent failure — the caller must load the tile and retry decode.
    #[error("tile {}/{}/{} required by A*", .0.z, .0.x, .0.y)]
    NeedsTile(TileKey),
    /// Combined positive + negative offsets exceed the decoded path length.
    /// The trimmed location would have zero or negative length — the reference is malformed.
    /// The routed path is carried here so the caller can still expose segment data for diagnostics.
    #[error("offsets overflow path: combined lower-bound {combined_lb_m:.1} m ≥ path {path_m:.1} m")]
    OffsetOverflow { combined_lb_m: f64, path_m: f64, path: Vec<SegmentId> },
}

/// Bundles a `DecodeError` with the partial `DecodeTrace` accumulated before the failure.
/// The trace is `None` when `params.trace_level == TraceLevel::Off`.
#[derive(Debug, Clone)]
pub struct DecodeFailure {
    pub error: DecodeError,
    pub trace: Option<DecodeTrace>,
}

impl std::fmt::Display for DecodeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Decode a `LocationReference` against `graph` using `params`.
///
/// The graph must already contain all tiles needed (use `prefetch_tile_keys` to
/// determine which tiles to load before calling this).
pub fn decode(
    loc_ref: &LocationReference,
    graph: &Graph,
    params: &DecodeParams,
    zoom: u8,
) -> Result<DecodedLocation, DecodeFailure> {
    let lrps = loc_ref.lrps.as_slice();
    let is_pal = loc_ref.location_type == LocationType::PointAlongLine;
    let mut trace = if params.trace_level != TraceLevel::Off {
        Some(Trace::new(params.clone()))
    } else {
        None
    };

    // ── 1. Candidate selection ──────────────────────────────────────────────
    let mut all_candidates: Vec<Vec<ScoredCandidate>> = Vec::with_capacity(lrps.len());
    for (i, lrp) in lrps.iter().enumerate() {
        let is_last = i == lrps.len() - 1;
        let cands = select_candidates(
            i,
            lrp,
            is_last,
            graph,
            params,
            trace.get_or_insert_with(|| Trace::new(params.clone())),
        );
        if cands.is_empty() {
            let outcome = DecodeOutcome::NoCandidates { lrp_idx: i };
            if let Some(t) = &mut trace {
                t.push_summary(DecodeEvent::DecodeComplete(outcome));
            }
            return Err(DecodeFailure { error: DecodeError::NoCandidates(i), trace });
        }
        all_candidates.push(cands);
    }

    // ── 2. Route each leg ───────────────────────────────────────────────────
    // RouteGenerator yields candidate-index combinations in ascending total
    // score order (cheapest first).  We try each in turn until one routes
    // all legs successfully.  The inner block scopes the `t` borrow so that
    // `trace` is free for the offset section below.
    // `winning_indices` is returned alongside the path so we can look up the
    // first/last candidate arc offsets for offset trimming.
    //
    // `needs_tile` is set when A* encounters a boundary node whose home tile is
    // not loaded.  It lives outside the block so we can inspect it after `t` drops.
    let mut needs_tile: Option<TileKey> = None;
    let mut deepest_failed_leg: usize = 0;
    let route_result: Option<(Vec<SegmentId>, f64, Vec<f64>, Vec<usize>)> = {
        let t = trace.get_or_insert_with(|| Trace::new(params.clone()));
        let mut route_cache: RouteCache = HashMap::new();

        // Pre-check: for single-leg (2-LRP) references, try topologically simple
        // combinations BEFORE the main RouteGenerator loop.  The RouteGenerator
        // orders by per-LRP score, so a longer but higher-scoring combination can
        // be accepted before the correct shorter one is ever tried.
        //
        // Two classes of topologically simple pairs (both guarded by a distance
        // threshold so only genuinely nearby candidates qualify):
        //
        //   1. Same-segment: both LRPs project onto the same segment in the same
        //      traversal direction with from-arc ≤ to-arc.  The path is a single
        //      segment; no A* needed.
        //
        //   2. Directly adjacent: from.exit_node == to.entry_node.  A* is trivial
        //      (start == goal → empty interior).  The path is exactly [from, to].
        //      This case is missed by the RouteGenerator when a longer, higher-
        //      scoring combination (that routes *through* the adjacent segment as an
        //      interior) is tried first and succeeds.
        //
        // Guard: both projections must be within 25 % of the search radius.  DNP
        // validation is the ultimate safety net for false positives.
        let nearby_threshold = params.candidate_search_radius_m * 0.25;
        let precheck: Option<(Vec<SegmentId>, f64, Vec<f64>, Vec<usize>)> = 'precheck: {
            if lrps.len() == 2 {
                for (i, from_cand) in all_candidates[0].iter().enumerate() {
                    for (j, to_cand) in all_candidates[1].iter().enumerate() {
                        let both_close = from_cand.projection.distance_m <= nearby_threshold
                            && to_cand.projection.distance_m <= nearby_threshold;
                        if !both_close { continue; }

                        let is_same_seg = from_cand.segment_id == to_cand.segment_id
                            && from_cand.traversal == to_cand.traversal
                            && from_cand.projection.arc_offset_m <= to_cand.projection.arc_offset_m;

                        // Directly adjacent: from's exit node is to's entry node.
                        // A* will return a trivial empty interior for this pair.
                        let is_adjacent = from_cand.exit_node == to_cand.entry_node;

                        if is_same_seg || is_adjacent {
                            let indices = vec![i, j];
                            let mut fl = 0usize;
                            if let Some((p, len, legs)) = try_route_combination(
                                &indices, &all_candidates, lrps, graph, params, t,
                                &mut route_cache, &mut needs_tile, zoom, &mut fl,
                            ) {
                                break 'precheck Some((p, len, legs, indices));
                            }
                            if needs_tile.is_some() { break 'precheck None; }
                        }
                    }
                }
            }
            None
        };

        if needs_tile.is_some() {
            None
        } else if let Some(r) = precheck {
            Some(r)
        } else {
            let cap = params.max_routing_attempts;
            let mut route_attempts = 0usize;
            let found = 'search: {
                for indices in Gen::new(&all_candidates) {
                    if cap > 0 && route_attempts >= cap {
                        t.push_summary(DecodeEvent::RouteAttemptsExhausted {
                            limit: cap,
                            attempted: route_attempts,
                        });
                        break 'search None;
                    }
                    route_attempts += 1;
                    let mut failed_leg = 0usize;
                    if let Some((p, len, legs)) = try_route_combination(
                        &indices, &all_candidates, lrps, graph, params, t,
                        &mut route_cache, &mut needs_tile, zoom, &mut failed_leg,
                    ) {
                        break 'search Some((p, len, legs, indices));
                    }
                    if needs_tile.is_some() { break 'search None; }
                    deepest_failed_leg = deepest_failed_leg.max(failed_leg);
                }
                None
            };
            if found.is_none() && needs_tile.is_none() {
                let outcome = DecodeOutcome::NoRoute { leg: deepest_failed_leg };
                t.push_summary(DecodeEvent::DecodeComplete(outcome));
            }
            found
        }
    };

    // NeedsTile takes priority: discard the partial trace and ask JS to load the tile.
    if let Some(tk) = needs_tile {
        return Err(DecodeFailure { error: DecodeError::NeedsTile(tk), trace: None });
    }

    let (path, total_path_m, leg_lengths, winning_indices): (Vec<SegmentId>, f64, Vec<f64>, Vec<usize>) = match route_result {
        Some(r) => r,
        None => return Err(DecodeFailure { error: DecodeError::AllCombinationsFailed(deepest_failed_leg + 1), trace }),
    };

    // ── 3. Offsets ──────────────────────────────────────────────────────────
    // Per spec §7.5.2: the offset byte encodes a fraction of the path between
    // the FIRST two LRPs (positive offset) or the LAST two LRPs (negative offset),
    // not the total path.  Use the actual decoded leg lengths from routing.
    // For TPEG: pos_offset/neg_offset are already exact LinearInterval::point values.
    let first_leg_m = leg_lengths.first().copied().unwrap_or(total_path_m);
    let last_leg_m  = leg_lengths.last().copied().unwrap_or(total_path_m);
    let pos_offset: Option<LinearInterval> = lrps.first().and_then(|l| {
        l.pos_offset_raw.map(|n| LinearInterval {
            lb: n as f64 / 256.0 * first_leg_m,
            ub: (n as f64 + 1.0) / 256.0 * first_leg_m,
        }).or(l.pos_offset)
    });
    let neg_offset: Option<LinearInterval> = lrps.last().and_then(|l| {
        l.neg_offset_raw.map(|n| LinearInterval {
            lb: n as f64 / 256.0 * last_leg_m,
            ub: (n as f64 + 1.0) / 256.0 * last_leg_m,
        }).or(l.neg_offset)
    });

    // Emit trace events (scoped so the borrow on `trace` ends before the overflow check).
    {
        let t = trace.get_or_insert_with(|| Trace::new(params.clone()));
        if let Some(interval) = pos_offset {
            t.push_summary(trace::DecodeEvent::OffsetApplied { is_positive: true, interval });
        }
        if let Some(interval) = neg_offset {
            t.push_summary(trace::DecodeEvent::OffsetApplied { is_positive: false, interval });
        }
    }

    // Validate: combined lower-bound offsets must not reach or exceed the path length.
    // If they do, the trimmed location has zero or negative length — the reference is malformed.
    let combined_offset_lb =
        pos_offset.map_or(0.0, |i| i.lb) + neg_offset.map_or(0.0, |i| i.lb);
    if (pos_offset.is_some() || neg_offset.is_some()) && combined_offset_lb >= total_path_m {
        return Err(DecodeFailure {
            error: DecodeError::OffsetOverflow {
                combined_lb_m: combined_offset_lb,
                path_m: total_path_m,
                path: path.clone(),
            },
            trace,
        });
    }

    // ── 4. LRP arc offsets, snap points, traversal directions ──────────────
    let first_lrp_arc_m = all_candidates[0][winning_indices[0]].projection.arc_offset_m;
    let last_lrp_arc_m  = all_candidates[lrps.len() - 1][*winning_indices.last().unwrap()]
        .projection.arc_offset_m;
    let first_seg_traversal = all_candidates[0][winning_indices[0]].traversal;
    let last_seg_traversal  = all_candidates[lrps.len() - 1][*winning_indices.last().unwrap()].traversal;

    let lrp_snap_points: Vec<(f64, f64)> = (0..lrps.len())
        .map(|i| all_candidates[i][winning_indices[i]].projection.point)
        .collect();
    let lrp_snap_is_endpoint: Vec<bool> = (0..lrps.len())
        .map(|i| {
            let p = &all_candidates[i][winning_indices[i]].projection;
            p.is_at_entry || p.is_at_exit
        })
        .collect();
    let lrp_snap_distances_m: Vec<f64> = (0..lrps.len())
        .map(|i| all_candidates[i][winning_indices[i]].projection.distance_m)
        .collect();

    // ── 5. PAL: compute point coordinate ───────────────────────────────────
    // Use the conservative (LB) trim — smallest offset keeps the point closest
    // to the LRP, maximising coverage when the interval is wide (v3 buckets).
    let (point_coord, orientation, side_of_road) = if is_pal {
        let dist = first_lrp_arc_m + pos_offset.map_or(0.0, |i| i.lb);
        let pt = wkt::point_at_path_distance(&path, dist, first_seg_traversal, graph);
        (pt, loc_ref.orientation, loc_ref.side_of_road)
    } else {
        (None, None, None)
    };

    // ── 6. Emit completion ──────────────────────────────────────────────────
    let outcome = DecodeOutcome::Success {
        path: path.clone(),
        pos_offset,
        neg_offset,
    };
    if let Some(t) = &mut trace {
        t.push_summary(DecodeEvent::DecodeComplete(outcome));
    }

    Ok(DecodedLocation {
        path, pos_offset, neg_offset,
        first_lrp_arc_m, last_lrp_arc_m,
        first_seg_traversal, last_seg_traversal,
        lrp_snap_points, lrp_snap_is_endpoint, lrp_snap_distances_m,
        trace,
        point_coord, orientation, side_of_road,
    })
}

// ── Per-combination routing attempt ──────────────────────────────────────────

/// Try to route all legs for the candidate combination given by `indices`.
///
/// Returns `Some(path)` if every leg routes successfully and passes DNP
/// validation, or `None` if any leg fails (the caller tries the next
/// combination).
///
/// Path construction invariant:
///   `path = [from₀.segment_id, …interior₀…, to₀.segment_id,
///                               …interior₁…, to₁.segment_id, …]`
///
/// The `from` for leg N is always `all_candidates[N][indices[N]]`, which equals
/// the `to` chosen for leg N−1 — guaranteeing path continuity.
fn try_route_combination(
    indices: &[usize],
    all_candidates: &[Vec<ScoredCandidate>],
    lrps: &[Lrp],
    graph: &Graph,
    params: &DecodeParams,
    trace: &mut Trace,
    cache: &mut RouteCache,
    needs_tile: &mut Option<TileKey>,
    zoom: u8,
    out_failed_leg: &mut usize,
) -> Option<(Vec<SegmentId>, f64, Vec<f64>)> {
    let mut path: Vec<SegmentId> = Vec::new();
    let mut total_path_m: f64 = 0.0;
    let mut leg_lengths: Vec<f64> = Vec::with_capacity(lrps.len().saturating_sub(1));

    for leg in 0..lrps.len() - 1 {
        let from = &all_candidates[leg][indices[leg]];
        let to   = &all_candidates[leg + 1][indices[leg + 1]];
        let lrp  = &lrps[leg];

        let lfrcnp_raw = lrp.lfrcnp.unwrap_or(7);
        let lfrcnp     = lfrcnp_raw.saturating_add(params.lfrcnp_tolerance).min(7);
        let dnp        = lrp.dnp.unwrap_or(openlr_codec::LinearInterval { lb: 0.0, ub: 15_000.0 });

        // Same-segment fast path: both LRPs are on the same segment traversed in the
        // same direction, with the from-LRP before the to-LRP.  No A* needed — the
        // distance is simply the difference of arc offsets.  Routing from exit_node back
        // to entry_node would force a U-turn on the same segment, which A* correctly
        // blocks; we must bypass A* entirely for this case.
        if from.segment_id == to.segment_id
            && from.traversal == to.traversal
            && from.projection.arc_offset_m <= to.projection.arc_offset_m
        {
            let direct_m = to.projection.arc_offset_m - from.projection.arc_offset_m;
            if validate_dnp(leg, direct_m, dnp, params, trace).is_err() {
                *out_failed_leg = leg;
                return None;
            }
            trace.push_summary(DecodeEvent::RouteFound {
                leg,
                path: vec![from.segment_id],
                length_m: direct_m,
                from_snap: from.projection.point,
                to_snap:   to.projection.point,
            });
            leg_lengths.push(direct_m);
            total_path_m += direct_m;
            if path.is_empty() {
                path.push(from.segment_id);
            }
            // to.segment_id == from.segment_id — already in path; do not push again.
            continue;
        }

        let key = (from.exit_node, to.entry_node, lfrcnp);

        // Route cache: avoids re-running A* for the same (exit, entry, lfrcnp) triple.
        // We cache only A* success/failure (None = no path exists); DNP validation is
        // NOT cached because it depends on the partial edge lengths of the specific
        // candidate pair being tested (different arc offsets → different full lengths).
        let cached = cache.get(&key).cloned();
        let (segments, interior_m) = match cached {
            Some(Some((segs, len))) => (segs, len),
            Some(None) => { *out_failed_leg = leg; return None; }
            None => {
                // Not yet known — run A*.
                trace.push_summary(DecodeEvent::RouteSearchStarted {
                    leg,
                    from: from.clone(),
                    to: to.clone(),
                });
                match find_route(
                    leg, from, to, graph, lfrcnp, dnp,
                    params.max_path_search_factor,
                    params.max_astar_expansions,
                    trace,
                    zoom,
                ) {
                    Ok(result) => {
                        let segs = result.segments;
                        let len  = result.length_m;
                        cache.insert(key, Some((segs.clone(), len)));
                        (segs, len)
                    }
                    Err(RoutingFailure::NeedsTile { z, x, y }) => {
                        // Not a true routing failure — a tile is missing.  Do NOT cache
                        // as NoPath; the route may exist once the tile is loaded.
                        *needs_tile = Some(TileKey { z, x, y });
                        return None;
                    }
                    Err(reason) => {
                        trace.push_summary(DecodeEvent::RouteFailed { leg, reason });
                        cache.insert(key, None);  // true A* failure — no path
                        *out_failed_leg = leg;
                        return None;
                    }
                }
            }
        };

        // Full LRP-to-LRP distance = partial first edge + interior A* route + partial last edge.
        // The partial edges come from each candidate's arc offset on their respective segment;
        // the direction of travel determines which end of the segment the partial faces.
        let from_seg_len = graph.segments.get(&from.segment_id).map_or(0.0, |s| s.length_m);
        let to_seg_len   = graph.segments.get(&to.segment_id).map_or(0.0, |s| s.length_m);
        // arc_offset_m is from traversal entry; from_partial is entry→exit (remainder),
        // to_partial is entry→projection (the offset itself).  Both hold for both directions.
        let from_partial = (from_seg_len - from.projection.arc_offset_m).max(0.0);
        let to_partial   = to.projection.arc_offset_m.min(to_seg_len);
        let full_length_m = from_partial + interior_m + to_partial;

        if validate_dnp(leg, full_length_m, dnp, params, trace).is_err() {
            *out_failed_leg = leg;
            return None;
        }

        {
            let mut leg_path = segments.clone();
            leg_path.insert(0, from.segment_id);
            if to.segment_id != from.segment_id {
                leg_path.push(to.segment_id);
            }
            trace.push_summary(DecodeEvent::RouteFound {
                leg,
                path: leg_path,
                length_m: full_length_m,
                from_snap: from.projection.point,
                to_snap:   to.projection.point,
            });
        }
        leg_lengths.push(full_length_m);
        total_path_m += full_length_m;

        if path.is_empty() {
            path.push(from.segment_id);
        }
        path.extend_from_slice(&segments);
        // Don't push to.segment_id when it equals from.segment_id and the interior
        // is empty — that is the single-segment case, and from is already in path.
        if !segments.is_empty() || to.segment_id != from.segment_id {
            path.push(to.segment_id);
        }
    }

    // Consecutive duplicate segments mean A* traversed the to-candidate's own
    // segment backward just to arrive at its entry node — a degenerate U-turn
    // that the A* U-turn prevention should ideally block but sometimes misses
    // when the incoming segment on the to-candidate happens to differ.  Any path
    // with a consecutive dup is topologically invalid for OpenLR.
    if path.windows(2).any(|w| w[0] == w[1]) {
        return None;
    }

    Some((path, total_path_m, leg_lengths))
}

// ── Forced decode (skip candidate selection) ──────────────────────────────────

/// Decode with pre-selected candidates — candidate selection is skipped entirely.
///
/// `forced_snaps` must contain exactly one `ScoredCandidate` per LRP.  Routing,
/// DNP validation, offset trimming, and PAL computation run unchanged.
pub fn decode_forced(
    loc_ref: &LocationReference,
    forced_snaps: Vec<ScoredCandidate>,
    graph: &Graph,
    params: &DecodeParams,
    zoom: u8,
) -> Result<DecodedLocation, DecodeFailure> {
    let lrps = loc_ref.lrps.as_slice();
    let n_lrps = lrps.len();
    assert_eq!(forced_snaps.len(), n_lrps, "one snap required per LRP");
    let is_pal = loc_ref.location_type == LocationType::PointAlongLine;

    let mut trace = if params.trace_level != TraceLevel::Off {
        Some(Trace::new(params.clone()))
    } else {
        None
    };

    let all_candidates: Vec<Vec<ScoredCandidate>> =
        forced_snaps.into_iter().map(|s| vec![s]).collect();
    let indices: Vec<usize> = vec![0; n_lrps];

    let mut deepest_failed_leg: usize = 0;
    let mut needs_tile: Option<TileKey> = None;
    let route_result: Option<(Vec<SegmentId>, f64, Vec<f64>)> = {
        let t = trace.get_or_insert_with(|| Trace::new(params.clone()));
        let mut route_cache: RouteCache = HashMap::new();
        let result = try_route_combination(
            &indices, &all_candidates, lrps, graph, params, t,
            &mut route_cache, &mut needs_tile, zoom, &mut deepest_failed_leg,
        );
        if result.is_none() && needs_tile.is_none() {
            t.push_summary(DecodeEvent::DecodeComplete(
                DecodeOutcome::NoRoute { leg: deepest_failed_leg },
            ));
        }
        result
    };

    if let Some(tk) = needs_tile {
        return Err(DecodeFailure { error: DecodeError::NeedsTile(tk), trace: None });
    }

    let (path, total_path_m, leg_lengths) = match route_result {
        Some(r) => r,
        None => return Err(DecodeFailure {
            error: DecodeError::AllCombinationsFailed(deepest_failed_leg + 1),
            trace,
        }),
    };

    // ── Offsets ───────────────────────────────────────────────────────────────
    // Per spec §7.5.2: offset fraction is relative to the first leg (pos) or
    // last leg (neg), not the total path length.
    let first_leg_m = leg_lengths.first().copied().unwrap_or(total_path_m);
    let last_leg_m  = leg_lengths.last().copied().unwrap_or(total_path_m);
    let pos_offset: Option<LinearInterval> = lrps.first().and_then(|l| {
        l.pos_offset_raw.map(|n| LinearInterval {
            lb: n as f64 / 256.0 * first_leg_m,
            ub: (n as f64 + 1.0) / 256.0 * first_leg_m,
        }).or(l.pos_offset)
    });
    let neg_offset: Option<LinearInterval> = lrps.last().and_then(|l| {
        l.neg_offset_raw.map(|n| LinearInterval {
            lb: n as f64 / 256.0 * last_leg_m,
            ub: (n as f64 + 1.0) / 256.0 * last_leg_m,
        }).or(l.neg_offset)
    });
    {
        let t = trace.get_or_insert_with(|| Trace::new(params.clone()));
        if let Some(interval) = pos_offset {
            t.push_summary(trace::DecodeEvent::OffsetApplied { is_positive: true, interval });
        }
        if let Some(interval) = neg_offset {
            t.push_summary(trace::DecodeEvent::OffsetApplied { is_positive: false, interval });
        }
    }
    let combined_offset_lb =
        pos_offset.map_or(0.0, |i| i.lb) + neg_offset.map_or(0.0, |i| i.lb);
    if (pos_offset.is_some() || neg_offset.is_some()) && combined_offset_lb >= total_path_m {
        return Err(DecodeFailure {
            error: DecodeError::OffsetOverflow {
                combined_lb_m: combined_offset_lb,
                path_m: total_path_m,
                path: path.clone(),
            },
            trace,
        });
    }

    // ── Snap points, traversal directions ────────────────────────────────────
    let first_lrp_arc_m     = all_candidates[0][0].projection.arc_offset_m;
    let last_lrp_arc_m      = all_candidates[n_lrps - 1][0].projection.arc_offset_m;
    let first_seg_traversal = all_candidates[0][0].traversal;
    let last_seg_traversal  = all_candidates[n_lrps - 1][0].traversal;

    let lrp_snap_points: Vec<(f64, f64)> =
        all_candidates.iter().map(|c| c[0].projection.point).collect();
    let lrp_snap_is_endpoint: Vec<bool> =
        all_candidates.iter().map(|c| {
            let p = &c[0].projection;
            p.is_at_entry || p.is_at_exit
        }).collect();
    let lrp_snap_distances_m: Vec<f64> =
        all_candidates.iter().map(|c| c[0].projection.distance_m).collect();

    // ── PAL ───────────────────────────────────────────────────────────────────
    let (point_coord, orientation, side_of_road) = if is_pal {
        let dist = first_lrp_arc_m + pos_offset.map_or(0.0, |i| i.lb);
        let pt = wkt::point_at_path_distance(&path, dist, first_seg_traversal, graph);
        (pt, loc_ref.orientation, loc_ref.side_of_road)
    } else {
        (None, None, None)
    };

    let outcome = DecodeOutcome::Success { path: path.clone(), pos_offset, neg_offset };
    if let Some(t) = &mut trace {
        t.push_summary(DecodeEvent::DecodeComplete(outcome));
    }

    Ok(DecodedLocation {
        path, pos_offset, neg_offset,
        first_lrp_arc_m, last_lrp_arc_m,
        first_seg_traversal, last_seg_traversal,
        lrp_snap_points, lrp_snap_is_endpoint, lrp_snap_distances_m,
        trace,
        point_coord, orientation, side_of_road,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_codec::{CircularInterval, LinearInterval};
    use openlr_codec::lrp::{LocationReference, LocationType, Orientation, SideOfRoad, Lrp};
    use openlr_graph::{Direction, Graph, NetworkNode, NetworkSegment, NodeId, SegmentId};

    fn node(id: u32, lon: f64, lat: f64) -> NetworkNode {
        NetworkNode { id: NodeId(id), lon, lat, stable_id: [0; 16], is_boundary: false }
    }
    fn seg(id: u32, s: u32, e: u32, len: f64, geom: Vec<(f64,f64)>) -> NetworkSegment {
        NetworkSegment {
            id: SegmentId(id), start_node: NodeId(s), end_node: NodeId(e),
            geometry: geom, length_m: len, frc: 3, fow: 3, direction: Direction::Both,
            stable_id: [0u8; 16],
        }
    }

    /// Three-segment zig-zag: A (east) → B (north) → C (east).
    ///
    /// LRPs are placed in the *interior* of each segment so their bearings
    /// uniquely identify which segment they belong to.  Using a wide-open DNP
    /// [0, 1000 m] avoids issues with the trivial (adjacent) A* length of 0.
    ///
    /// Verifies:
    /// - path is [A, B, C] with junction segment B appearing exactly once
    /// - multi-leg continuity: from for leg 1 is locked to the `to` chosen for leg 0
    #[test]
    fn three_lrp_path_continuous() {
        let mut g = Graph::new();
        //  node0 (0.000, 0.000) ──(seg1 east)──▶ node1 (0.001, 0.000)
        //                                                 │
        //                                           (seg2 north)
        //                                                 │
        //                                                 ▼
        //  node3 (0.002, 0.001) ◀──(seg3 east)── node2 (0.001, 0.001)
        g.add_node(node(0, 0.000, 0.000));
        g.add_node(node(1, 0.001, 0.000));
        g.add_node(node(2, 0.001, 0.001));
        g.add_node(node(3, 0.002, 0.001));
        g.add_segment(seg(1, 0, 1, 111.0, vec![(0.000, 0.000), (0.001, 0.000)]));
        g.add_segment(seg(2, 1, 2, 111.0, vec![(0.001, 0.000), (0.001, 0.001)]));
        g.add_segment(seg(3, 2, 3, 111.0, vec![(0.001, 0.001), (0.002, 0.001)]));

        // LRP0 at midpoint of seg1 (0.0005, 0.000) — bearing east ~90°.
        // LRP1 at midpoint of seg2 (0.001, 0.0005) — bearing north ~0°.
        // LRP2 at midpoint of seg3 (0.0015, 0.001) — last LRP, backward bearing west ~270°.
        //
        // Each LRP position unambiguously matches exactly one segment because
        // the other segments are too far or face the wrong direction.
        let loc_ref = LocationReference::line(vec![
                Lrp {
                    coord: (0.0005, 0.000),
                    bearing: CircularInterval { lb_deg: 75.0,  ub_deg: 105.0  }, // east
                    frc: 3, fow: 3,
                    lfrcnp: Some(7),
                    dnp: Some(LinearInterval { lb: 0.0, ub: 1000.0 }),
                    pos_offset: None, neg_offset: None,
                    pos_offset_raw: None, neg_offset_raw: None,
                },
                Lrp {
                    coord: (0.001, 0.0005),
                    bearing: CircularInterval { lb_deg: 345.0, ub_deg: 15.0   }, // north (wraps 0°)
                    frc: 3, fow: 3,
                    lfrcnp: Some(7),
                    dnp: Some(LinearInterval { lb: 0.0, ub: 1000.0 }),
                    pos_offset: None, neg_offset: None,
                    pos_offset_raw: None, neg_offset_raw: None,
                },
                Lrp {
                    coord: (0.0015, 0.001),
                    bearing: CircularInterval { lb_deg: 255.0, ub_deg: 285.0  }, // west (backward)
                    frc: 3, fow: 3,
                    lfrcnp: None, dnp: None,
                    pos_offset: None, neg_offset: None,
                    pos_offset_raw: None, neg_offset_raw: None,
                },
            ]);

        let mut params = DecodeParams::preset(Preset::Permissive);
        params.trace_level = TraceLevel::Off;

        let result = decode(&loc_ref, &g, &params, 12).expect("decode failed");
        assert_eq!(
            result.path,
            vec![SegmentId(1), SegmentId(2), SegmentId(3)],
            "path should be [A, B, C] with B (junction) appearing exactly once",
        );
    }
}
