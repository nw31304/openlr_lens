use openlr_codec::lrp::Lrp;
use openlr_graph::{bearing_at_offset, project_onto_polyline, Graph, SegmentId, Direction};

use crate::params::DecodeParams;
use crate::trace::{
    CandidateScore, DecodeEvent, GateVerdict, ProjectionResult, ScoredCandidate,
    TraversalDir, DecodeTrace,
};

/// Select and rank candidate segments for one LRP.
///
/// Returns the accepted candidates sorted ascending by total score (best first).
/// Rejects are counted but not returned individually unless `trace_level == Full`
/// (they appear in `CandidateEvaluated` events).
pub fn select_candidates(
    lrp_idx: usize,
    lrp: &Lrp,
    is_last_lrp: bool,
    graph: &Graph,
    params: &DecodeParams,
    trace: &mut DecodeTrace,
) -> Vec<ScoredCandidate> {
    let (lon, lat) = lrp.coord;

    trace.push_summary(DecodeEvent::CandidateSearchStarted {
        lrp_idx,
        coord: lrp.coord,
        radius_m: params.candidate_search_radius_m,
    });

    let nearby = graph.segments_near(lon, lat, params.candidate_search_radius_m);

    let mut accepted: Vec<ScoredCandidate> = Vec::new();
    let mut rejected_count = 0usize;

    for (seg_id, _coarse_dist) in nearby {
        let seg = match graph.segments.get(&seg_id) {
            Some(s) => s,
            None => continue,
        };

        // A bidirectional segment can match in either direction; a one-way only in its direction.
        let dirs: &[TraversalDir] = match seg.direction {
            Direction::Both     => &[TraversalDir::Forward, TraversalDir::Backward],
            Direction::Forward  => &[TraversalDir::Forward],
            Direction::Backward => &[TraversalDir::Backward],
        };

        for &dir in dirs {
            match evaluate_candidate(lrp, lrp_idx, seg_id, dir, is_last_lrp, seg, params, graph) {
                Ok(scored) => {
                    trace.push_full(DecodeEvent::CandidateEvaluated {
                        lrp_idx,
                        segment_id: seg_id,
                        traversal: dir,
                        projection: scored.projection.clone(),
                        verdict: GateVerdict::Pass,
                        score: Some(scored.score.clone()),
                    });
                    accepted.push(scored);
                }
                Err(verdict) => {
                    rejected_count += 1;
                    trace.push_full(DecodeEvent::CandidateEvaluated {
                        lrp_idx,
                        segment_id: seg_id,
                        traversal: dir,
                        projection: ProjectionResult {
                            arc_offset_m: 0.0,
                            point: (0.0, 0.0),
                            distance_m: 0.0,
                            bearing_deg: 0.0,
                        },
                        verdict,
                        score: None,
                    });
                }
            }
        }
    }

    // Sort ascending by total score (lower = better), then cap to max_candidates_per_lrp.
    accepted.sort_by(|a, b| {
        a.score.total.partial_cmp(&b.score.total).unwrap_or(std::cmp::Ordering::Equal)
    });
    if params.max_candidates_per_lrp > 0 {
        accepted.truncate(params.max_candidates_per_lrp);
    }

    trace.push_summary(DecodeEvent::CandidatesRanked {
        lrp_idx,
        accepted: accepted.clone(),
        rejected_count,
    });

    accepted
}

// ── Internal ──────────────────────────────────────────────────────────────────

fn evaluate_candidate(
    lrp: &Lrp,
    _lrp_idx: usize,
    seg_id: SegmentId,
    dir: TraversalDir,
    is_last_lrp: bool,
    seg: &openlr_graph::NetworkSegment,
    params: &DecodeParams,
    _graph: &Graph,
) -> Result<ScoredCandidate, GateVerdict> {
    let (lon, lat) = lrp.coord;

    // Geometry for this traversal direction.
    let geom: Vec<(f64, f64)> = match dir {
        TraversalDir::Forward  => seg.geometry.clone(),
        TraversalDir::Backward => seg.geometry.iter().cloned().rev().collect(),
    };

    // Project LRP coordinate onto the segment.
    let proj = project_onto_polyline(lon, lat, &geom)
        .ok_or(GateVerdict::FailDirection)?;

    // Hard gate: search radius.
    if proj.distance_m > params.candidate_search_radius_m {
        return Err(GateVerdict::FailRadius {
            distance_m: proj.distance_m,
            radius_m: params.candidate_search_radius_m,
        });
    }

    // Bearing at the projection point: forward for non-last LRP, backward for last.
    let forward_bearing = !is_last_lrp;
    let bearing = bearing_at_offset(&geom, proj.arc_offset_m, forward_bearing);

    // Hard gate: bearing within widened interval.
    let widened = lrp.bearing.widen(params.bearing_tolerance_deg);
    if !widened.contains(bearing) {
        return Err(GateVerdict::FailBearing { bearing_deg: bearing, widened });
    }

    // Soft penalty: bearing excess outside the bare encoding interval.
    let bearing_excess = lrp.bearing.excess(bearing);

    // Soft penalty: FRC / FOW mismatch.
    let frc_diff = (seg.frc as i32 - lrp.frc as i32).unsigned_abs() as f64;
    let fow_match = seg.fow == lrp.fow;
    let frc_penalty = frc_diff * params.frc_penalty_per_step;
    let fow_penalty = if fow_match { 0.0 } else { params.fow_penalty };

    let total = proj.distance_m + bearing_excess + frc_penalty + fow_penalty;

    let projection = ProjectionResult {
        arc_offset_m: proj.arc_offset_m,
        point: proj.point,
        distance_m: proj.distance_m,
        bearing_deg: bearing,
    };

    // Entry/exit nodes for A* state machine.
    let (entry_node, exit_node) = match dir {
        TraversalDir::Forward  => (seg.start_node, seg.end_node),
        TraversalDir::Backward => (seg.end_node,   seg.start_node),
    };

    Ok(ScoredCandidate {
        segment_id: seg_id,
        traversal: dir,
        projection,
        score: CandidateScore {
            positional_m: proj.distance_m,
            bearing_excess_deg: bearing_excess,
            frc_penalty,
            fow_penalty,
            total,
        },
        exit_node,
        entry_node,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_codec::{CircularInterval, LinearInterval};
    use openlr_codec::lrp::Lrp;
    use openlr_graph::{NetworkNode, NetworkSegment, NodeId};

    fn simple_graph() -> Graph {
        let mut g = Graph::new();
        g.add_node(NetworkNode { id: NodeId(0), lon: 0.0,   lat: 0.0,   stable_id: [0;16], is_boundary: false });
        g.add_node(NetworkNode { id: NodeId(1), lon: 0.001, lat: 0.0,   stable_id: [0;16], is_boundary: false });
        g.add_segment(NetworkSegment {
            id: SegmentId(1),
            start_node: NodeId(0),
            end_node:   NodeId(1),
            geometry: vec![(0.0, 0.0), (0.001, 0.0)],
            length_m: 100.0,
            frc: 3,
            fow: 3,
            direction: Direction::Both,
        });
        g
    }

    fn lrp_near_origin(bearing_lb: f64) -> Lrp {
        Lrp {
            coord: (0.0005, 0.0001),
            bearing: CircularInterval { lb_deg: bearing_lb, ub_deg: bearing_lb + 11.25 },
            frc: 3,
            fow: 3,
            lfrcnp: Some(5),
            dnp: Some(LinearInterval { lb: 58.0, ub: 117.0 }),
            pos_offset: None,
            neg_offset: None,
        }
    }

    #[test]
    fn finds_candidate_for_eastbound_bearing() {
        let g = simple_graph();
        let lrp = lrp_near_origin(82.0); // east-ish sector
        let mut trace = DecodeTrace::new(DecodeParams::default());
        let candidates = select_candidates(0, &lrp, false, &g, &DecodeParams::default(), &mut trace);
        assert!(!candidates.is_empty(), "should find at least one candidate");
    }

    #[test]
    fn rejects_candidate_when_bearing_wrong() {
        let g = simple_graph();
        // Bearing pointing north; segment is east-west → should be rejected
        let lrp = lrp_near_origin(0.0);
        let mut params = DecodeParams::default();
        params.bearing_tolerance_deg = 5.0;  // tight
        let mut trace = DecodeTrace::new(params.clone());
        let candidates = select_candidates(0, &lrp, false, &g, &params, &mut trace);
        assert!(candidates.is_empty(), "north-pointing LRP should not match east-west segment");
    }
}
