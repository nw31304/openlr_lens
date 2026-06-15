use openlr_graph::{Graph, NodeId, SegmentId, haversine_m, interpolate_at, polyline_length_m};

/// Assemble a WKT `LINESTRING` from a decoded path, applying pos/neg offsets.
///
/// Offsets are measured relative to the LRP projection points, not segment endpoints:
/// - `first_lrp_arc_m`: arc offset of the first LRP on the first segment (traversal dir).
///   Actual start = `first_lrp_arc_m + pos_offset_m` along the path.
/// - `last_lrp_arc_m`: arc offset of the last LRP on the last segment (traversal dir).
///   Actual end = `last_lrp_arc_m - neg_offset_m` along the path.
///
/// Both offsets can overflow their segment — the excess carries into adjacent segments.
///
/// Returns `None` if any segment is missing from the graph or the trimmed
/// result collapses to fewer than 2 points.
pub fn path_to_wkt(
    path: &[SegmentId],
    pos_offset_m: f64,
    neg_offset_m: f64,
    first_lrp_arc_m: f64,
    last_lrp_arc_m: f64,
    graph: &Graph,
) -> Option<String> {
    if path.is_empty() {
        return None;
    }

    let n = path.len();

    // Resolve all segments up front; bail if any is missing.
    let segs: Vec<_> = path.iter().map(|id| graph.segments.get(id)).collect::<Option<Vec<_>>>()?;

    // Infer traversal direction (forward = stored order) for each segment from
    // node-connectivity.  We walk the path left-to-right: seg[0]'s direction is
    // determined by which of its endpoints it shares with seg[1]; subsequent
    // segments are oriented so their entry node equals the exit node of the
    // previous segment.
    let mut forward = vec![true; n];
    if n >= 2 {
        let s0 = segs[0];
        let s1 = segs[1];
        forward[0] = s0.end_node == s1.start_node || s0.end_node == s1.end_node;

        for i in 1..n {
            let prev_exit: NodeId = if forward[i - 1] { segs[i - 1].end_node } else { segs[i - 1].start_node };
            forward[i] = segs[i].start_node == prev_exit;
        }
    }

    // Precompute haversine lengths (same regardless of traversal direction).
    let actual_lens: Vec<f64> = segs.iter()
        .map(|seg| polyline_length_m(&seg.geometry))
        .collect();

    // ── Positive-offset cut: walk forward from (first_lrp_arc_m + pos_offset_m) ──
    // Finds the segment index and within-segment start where the location begins.
    let (pos_seg, pos_start_m) = {
        let mut rem = (first_lrp_arc_m + pos_offset_m).max(0.0);
        let mut result = (0usize, 0.0f64);
        for i in 0..n {
            if rem <= actual_lens[i] {
                result = (i, rem);
                break;
            }
            rem -= actual_lens[i];
        }
        result
    };

    // ── Negative-offset cut: walk backward from last LRP position ──
    // Finds the segment index and within-segment end where the location ends.
    let (neg_seg, neg_end_m) = {
        let lrp_arc = last_lrp_arc_m.min(actual_lens[n - 1]);
        let mut rem = neg_offset_m;
        if rem <= lrp_arc {
            // Trim lands within the last segment.
            (n - 1, lrp_arc - rem)
        } else {
            rem -= lrp_arc;
            let mut result = (0usize, 0.0f64); // fallback: entire path consumed
            'neg_cut: for i in (0..n - 1).rev() {
                let avail = actual_lens[i];
                if rem <= avail {
                    result = (i, avail - rem);
                    break 'neg_cut;
                }
                rem -= avail;
            }
            result
        }
    };

    // If the trim window is empty or inverted, the location has collapsed.
    if pos_seg > neg_seg { return None; }
    if pos_seg == neg_seg && pos_start_m >= neg_end_m { return None; }

    let mut pts: Vec<(f64, f64)> = Vec::new();

    for (i, (seg, &fwd)) in segs.iter().zip(forward.iter()).enumerate() {
        // Skip segments entirely outside the trim window.
        if i < pos_seg || i > neg_seg {
            continue;
        }

        let geom: Vec<(f64, f64)> = if fwd {
            seg.geometry.clone()
        } else {
            seg.geometry.iter().cloned().rev().collect()
        };

        let actual_len = actual_lens[i];
        let start_m = if i == pos_seg { pos_start_m } else { 0.0 };
        let end_m   = if i == neg_seg { neg_end_m   } else { actual_len };

        if end_m <= start_m {
            continue;
        }

        let seg_pts = segment_vertices(&geom, actual_len, start_m, end_m);

        if pts.is_empty() {
            pts.extend_from_slice(&seg_pts);
        } else if let Some(first) = seg_pts.first() {
            // Segments share a junction vertex — skip the duplicate.
            let last = *pts.last().unwrap();
            let dup = (last.0 - first.0).abs() < 1e-8 && (last.1 - first.1).abs() < 1e-8;
            pts.extend_from_slice(if dup { &seg_pts[1..] } else { &seg_pts });
        }
    }

    if pts.len() < 2 {
        return None;
    }

    let coords = pts.iter()
        .map(|(lon, lat)| format!("{lon:.7} {lat:.7}"))
        .collect::<Vec<_>>()
        .join(",");
    Some(format!("LINESTRING ({coords})"))
}

/// Extract vertices from a polyline between [start_m, end_m] arc-length offsets.
///
/// `actual_len` is the pre-computed haversine length of the polyline (avoids
/// recomputing inside the function when the caller already has it).
fn segment_vertices(
    geom: &[(f64, f64)],
    actual_len: f64,
    start_m: f64,
    end_m: f64,
) -> Vec<(f64, f64)> {
    // Snap to exact endpoints when we're not trimming — avoids FP drift from
    // interpolate_at when stored segment length differs from haversine length.
    let start_pt = if start_m <= 0.0 { geom[0] } else { interpolate_at(geom, start_m) };
    let end_pt   = if end_m >= actual_len { *geom.last().unwrap() } else { interpolate_at(geom, end_m) };

    let mut out = vec![start_pt];
    let mut acc = 0.0;
    for w in geom.windows(2) {
        acc += haversine_m(w[0].0, w[0].1, w[1].0, w[1].1);
        // Include vertex w[1] only when its arc-length is strictly inside the window.
        if acc > start_m && acc < end_m {
            out.push(w[1]);
        }
    }

    // Append the end point unless it's already the last collected point.
    let last = *out.last().unwrap();
    if (last.0 - end_pt.0).abs() > 1e-9 || (last.1 - end_pt.1).abs() > 1e-9 {
        out.push(end_pt);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use openlr_graph::{Direction, Graph, NetworkNode, NetworkSegment, NodeId, SegmentId};

    fn node(id: u32, lon: f64, lat: f64) -> NetworkNode {
        NetworkNode { id: NodeId(id), lon, lat, stable_id: [0; 16], is_boundary: false }
    }
    fn seg_g(id: u32, s: u32, e: u32, geom: Vec<(f64, f64)>) -> NetworkSegment {
        let len = polyline_length_m(&geom);
        NetworkSegment {
            id: SegmentId(id), start_node: NodeId(s), end_node: NodeId(e),
            geometry: geom, length_m: len, frc: 3, fow: 3, direction: Direction::Both,
        }
    }

    #[test]
    fn no_offsets_two_segments() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.001, 0.0));
        g.add_node(node(2, 0.002, 0.0));
        g.add_segment(seg_g(1, 0, 1, vec![(0.0, 0.0), (0.001, 0.0)]));
        g.add_segment(seg_g(2, 1, 2, vec![(0.001, 0.0), (0.002, 0.0)]));

        let seg1_len = polyline_length_m(&[(0.0_f64, 0.0_f64), (0.001, 0.0)]);
        let seg2_len = polyline_length_m(&[(0.001_f64, 0.0_f64), (0.002, 0.0)]);
        // LRPs at nodes: first_lrp_arc = 0, last_lrp_arc = seg2_len.
        let wkt = path_to_wkt(&[SegmentId(1), SegmentId(2)], 0.0, 0.0, 0.0, seg2_len, &g).unwrap();
        // start, junction (deduped), end → 3 points
        assert!(wkt.starts_with("LINESTRING ("), "{wkt}");
        let n_pts = wkt.split(',').count();
        assert_eq!(n_pts, 3, "expected 3 points (start, junction, end): {wkt}");
        let _ = seg1_len; // suppress unused-variable warning
    }

    #[test]
    fn pos_offset_trims_start() {
        let mut g = Graph::new();
        g.add_node(node(0, 0.0, 0.0));
        g.add_node(node(1, 0.01, 0.0)); // ~1.1 km
        g.add_segment(seg_g(1, 0, 1, vec![(0.0, 0.0), (0.01, 0.0)]));

        let len = polyline_length_m(&[(0.0_f64, 0.0_f64), (0.01, 0.0)]);
        // Trim the first 20 % from the start.  LRP at node 0 (arc = 0), last LRP at node 1 (arc = len).
        let offset = len * 0.2;
        let wkt = path_to_wkt(&[SegmentId(1)], offset, 0.0, 0.0, len, &g).unwrap();
        // The start point should be offset from (0,0).
        assert!(!wkt.contains("0.0000000 0.0000000"), "start should be trimmed: {wkt}");
    }

    #[test]
    fn empty_path_returns_none() {
        let g = Graph::new();
        assert!(path_to_wkt(&[], 0.0, 0.0, 0.0, 0.0, &g).is_none());
    }
}
