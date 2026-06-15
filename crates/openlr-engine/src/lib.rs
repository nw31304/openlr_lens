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
pub use wkt::path_to_wkt;

use std::collections::HashMap;

use openlr_codec::lrp::{LocationReference, Lrp};
use openlr_graph::{Graph, NodeId, SegmentId};

use astar::find_route;
use candidate::select_candidates;
use route_generator::RouteGenerator as Gen;
use trace::{DecodeTrace as Trace, ScoredCandidate};
use validation::{apply_offset, validate_dnp};

// (exit_node, entry_node, effective_lfrcnp) → None (A* found no path) or Some((segs, interior_length_m))
type RouteCache = HashMap<(NodeId, NodeId, u8), Option<(Vec<SegmentId>, f64)>>;

// ── Public result types ───────────────────────────────────────────────────────

/// The decoded line location.
#[derive(Debug, Clone)]
pub struct DecodedLocation {
    /// Ordered segment IDs making up the matched path (includes partial first/last segments).
    pub path: Vec<SegmentId>,
    /// Positive offset — raw codec value: meters from the first LRP's position forward to the
    /// actual start of the location.
    pub pos_offset_m: f64,
    /// Negative offset — raw codec value: meters backward from the last LRP's position to the
    /// actual end of the location.
    pub neg_offset_m: f64,
    /// Arc offset of the first LRP on the first path segment, in the traversal direction (m).
    /// Combined with `pos_offset_m` to determine the trim start: `first_lrp_arc_m + pos_offset_m`.
    pub first_lrp_arc_m: f64,
    /// Arc offset of the last LRP on the last path segment, in the traversal direction (m).
    /// Used as the reference point for `neg_offset_m`: `last_lrp_arc_m - neg_offset_m`.
    pub last_lrp_arc_m: f64,
    /// Trace of every decision made during this decode.
    /// `None` when `params.trace_level == TraceLevel::Off`.
    pub trace: Option<DecodeTrace>,
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
) -> Result<DecodedLocation, DecodeError> {
    let lrps = &loc_ref.lrps;
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
            return Err(DecodeError::NoCandidates(i));
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
    let (path, winning_indices): (Vec<SegmentId>, Vec<usize>) = {
        let t = trace.get_or_insert_with(|| Trace::new(params.clone()));
        let mut route_cache: RouteCache = HashMap::new();
        'search: {
            for indices in Gen::new(&all_candidates) {
                if let Some(p) = try_route_combination(
                    &indices, &all_candidates, lrps, graph, params, t, &mut route_cache,
                ) {
                    break 'search (p, indices);
                }
            }
            let outcome = DecodeOutcome::NoRoute { leg: 0 };
            t.push_summary(DecodeEvent::DecodeComplete(outcome));
            return Err(DecodeError::AllCombinationsFailed(0));
        }
    };

    // ── 3. Offsets ──────────────────────────────────────────────────────────
    let t = trace.get_or_insert_with(|| Trace::new(params.clone()));

    let pos_offset_m = lrps.first()
        .and_then(|l| l.pos_offset)
        .map(|interval| apply_offset(true, interval, &path, graph, t))
        .unwrap_or(0.0);

    let neg_offset_m = lrps.last()
        .and_then(|l| l.neg_offset)
        .map(|interval| apply_offset(false, interval, &path, graph, t))
        .unwrap_or(0.0);

    // ── 4. LRP arc offsets in traversal direction ───────────────────────────
    // These tell path_to_wkt where each LRP projects on its segment so that
    // pos/neg offsets are applied relative to the LRP position, not the
    // segment's endpoint node.  For standard node-snapping encoders the LRP
    // is at the segment endpoint so these equal 0 / segment_length respectively,
    // and the trim is unchanged.  For mid-segment LRPs the correction matters.
    // arc_offset_m is always measured from traversal entry (geometry is reversed
    // before projection for Backward candidates), so no direction conversion needed.
    let first_lrp_arc_m = all_candidates[0][winning_indices[0]].projection.arc_offset_m;
    let last_lrp_arc_m  = all_candidates[lrps.len() - 1][*winning_indices.last().unwrap()]
        .projection.arc_offset_m;

    // ── 5. Emit completion ──────────────────────────────────────────────────
    let outcome = DecodeOutcome::Success {
        path: path.clone(),
        pos_offset_m: if pos_offset_m > 0.0 { Some(pos_offset_m) } else { None },
        neg_offset_m: if neg_offset_m > 0.0 { Some(neg_offset_m) } else { None },
    };
    if let Some(t) = &mut trace {
        t.push_summary(DecodeEvent::DecodeComplete(outcome));
    }

    Ok(DecodedLocation { path, pos_offset_m, neg_offset_m, first_lrp_arc_m, last_lrp_arc_m, trace })
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
) -> Option<Vec<SegmentId>> {
    let mut path: Vec<SegmentId> = Vec::new();

    for leg in 0..lrps.len() - 1 {
        let from = &all_candidates[leg][indices[leg]];
        let to   = &all_candidates[leg + 1][indices[leg + 1]];
        let lrp  = &lrps[leg];

        let lfrcnp_raw = lrp.lfrcnp.unwrap_or(7);
        let lfrcnp     = lfrcnp_raw.saturating_add(params.lfrcnp_tolerance).min(7);
        let dnp        = lrp.dnp.unwrap_or(openlr_codec::LinearInterval { lb: 0.0, ub: 15_000.0 });

        let key = (from.exit_node, to.entry_node, lfrcnp);

        // Route cache: avoids re-running A* for the same (exit, entry, lfrcnp) triple.
        // We cache only A* success/failure (None = no path exists); DNP validation is
        // NOT cached because it depends on the partial edge lengths of the specific
        // candidate pair being tested (different arc offsets → different full lengths).
        let cached = cache.get(&key).cloned();
        let (segments, interior_m) = match cached {
            Some(Some((segs, len))) => (segs, len),
            Some(None) => return None,
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
                ) {
                    Ok(result) => {
                        let segs = result.segments;
                        let len  = result.length_m;
                        cache.insert(key, Some((segs.clone(), len)));
                        (segs, len)
                    }
                    Err(reason) => {
                        trace.push_summary(DecodeEvent::RouteFailed { leg, reason });
                        cache.insert(key, None);  // true A* failure — no path
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
            return None;
        }

        trace.push_summary(DecodeEvent::RouteFound {
            leg,
            path: segments.clone(),
            length_m: full_length_m,
        });

        if path.is_empty() {
            path.push(from.segment_id);
        }
        path.extend_from_slice(&segments);
        path.push(to.segment_id);
    }

    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_codec::{CircularInterval, LinearInterval};
    use openlr_codec::lrp::{LocationReference, Lrp};
    use openlr_graph::{Direction, Graph, NetworkNode, NetworkSegment, NodeId, SegmentId};

    fn node(id: u32, lon: f64, lat: f64) -> NetworkNode {
        NetworkNode { id: NodeId(id), lon, lat, stable_id: [0; 16], is_boundary: false }
    }
    fn seg(id: u32, s: u32, e: u32, len: f64, geom: Vec<(f64,f64)>) -> NetworkSegment {
        NetworkSegment {
            id: SegmentId(id), start_node: NodeId(s), end_node: NodeId(e),
            geometry: geom, length_m: len, frc: 3, fow: 3, direction: Direction::Both,
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
        let loc_ref = LocationReference {
            lrps: vec![
                Lrp {
                    coord: (0.0005, 0.000),
                    bearing: CircularInterval { lb_deg: 75.0,  ub_deg: 105.0  }, // east
                    frc: 3, fow: 3,
                    lfrcnp: Some(7),
                    dnp: Some(LinearInterval { lb: 0.0, ub: 1000.0 }),
                    pos_offset: None, neg_offset: None,
                },
                Lrp {
                    coord: (0.001, 0.0005),
                    bearing: CircularInterval { lb_deg: 345.0, ub_deg: 15.0   }, // north (wraps 0°)
                    frc: 3, fow: 3,
                    lfrcnp: Some(7),
                    dnp: Some(LinearInterval { lb: 0.0, ub: 1000.0 }),
                    pos_offset: None, neg_offset: None,
                },
                Lrp {
                    coord: (0.0015, 0.001),
                    bearing: CircularInterval { lb_deg: 255.0, ub_deg: 285.0  }, // west (backward)
                    frc: 3, fow: 3,
                    lfrcnp: None, dnp: None,
                    pos_offset: None, neg_offset: None,
                },
            ],
        };

        let mut params = DecodeParams::preset(Preset::Permissive);
        params.trace_level = TraceLevel::Off;

        let result = decode(&loc_ref, &g, &params).expect("decode failed");
        assert_eq!(
            result.path,
            vec![SegmentId(1), SegmentId(2), SegmentId(3)],
            "path should be [A, B, C] with B (junction) appearing exactly once",
        );
    }
}
