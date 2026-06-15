use std::collections::HashMap;
use crate::{NetworkNode, NetworkSegment, NodeId, SegmentId, TurnRestriction, Direction};
use crate::geometry::{haversine_m, project_onto_polyline};

/// In-memory routing graph built from loaded tile data.
///
/// `outgoing[node]` lists every segment that can be *entered* from `node`:
///   - `Direction::Forward | Both` → reachable from `start_node`
///   - `Direction::Backward | Both` → reachable from `end_node`
pub struct Graph {
    pub segments: HashMap<SegmentId, NetworkSegment>,
    pub nodes: HashMap<NodeId, NetworkNode>,
    outgoing: HashMap<NodeId, Vec<SegmentId>>,
    restrictions: Vec<TurnRestriction>,
}

impl Default for Graph {
    fn default() -> Self { Self::new() }
}

impl Graph {
    pub fn new() -> Self {
        Self {
            segments: HashMap::new(),
            nodes: HashMap::new(),
            outgoing: HashMap::new(),
            restrictions: Vec::new(),
        }
    }

    pub fn add_segment(&mut self, seg: NetworkSegment) {
        let id    = seg.id;
        let start = seg.start_node;
        let end   = seg.end_node;
        match seg.direction {
            Direction::Forward | Direction::Both => {
                self.outgoing.entry(start).or_default().push(id);
            }
            Direction::Backward => {}
        }
        match seg.direction {
            Direction::Backward | Direction::Both => {
                self.outgoing.entry(end).or_default().push(id);
            }
            Direction::Forward => {}
        }
        self.segments.insert(id, seg);
    }

    pub fn add_node(&mut self, node: NetworkNode) {
        self.nodes.insert(node.id, node);
    }

    pub fn add_restriction(&mut self, r: TurnRestriction) {
        self.restrictions.push(r);
    }

    /// Segments within `radius_m` of `(lon, lat)`. Returns `(segment_id, distance_m)`.
    /// Linear scan — adequate for candidate selection on a typical tile region.
    pub fn segments_near(&self, lon: f64, lat: f64, radius_m: f64) -> Vec<(SegmentId, f64)> {
        self.segments
            .values()
            .filter_map(|seg| {
                let proj = project_onto_polyline(lon, lat, &seg.geometry)?;
                if proj.distance_m <= radius_m { Some((seg.id, proj.distance_m)) } else { None }
            })
            .collect()
    }

    /// Is the turn `from_seg → via_node → to_seg` explicitly restricted?
    pub fn is_restricted(&self, from_seg: SegmentId, via_node: NodeId, to_seg: SegmentId) -> bool {
        self.restrictions.iter().any(|r| {
            r.from_seg == from_seg && r.via_node == via_node && r.to_seg == to_seg
        })
    }

    /// Successor edges reachable from `(node, incoming_seg)`.
    ///
    /// Returns `(next_node, seg_id, length_m)` for every edge that:
    /// - can be entered from `node` in the direction it permits,
    /// - has `seg.frc ≤ lfrcnp` (LFRCNP floor — Invariant 9),
    /// - is not blocked by an explicit turn restriction.
    pub fn successors(
        &self,
        node: NodeId,
        incoming_seg: SegmentId,
        lfrcnp: u8,
    ) -> Vec<(NodeId, SegmentId, f64)> {
        let mut result = Vec::new();
        for &seg_id in self.outgoing.get(&node).into_iter().flatten() {
            let seg = match self.segments.get(&seg_id) {
                Some(s) => s,
                None => continue,
            };
            // FRC floor: only use roads at or above LFRCNP importance
            if seg.frc > lfrcnp {
                continue;
            }
            // Turn restriction
            if self.is_restricted(incoming_seg, node, seg_id) {
                continue;
            }
            // Determine the next node and verify the direction is traversable.
            let next_node = match seg.direction {
                Direction::Forward | Direction::Both if seg.start_node == node => seg.end_node,
                Direction::Backward | Direction::Both if seg.end_node == node   => seg.start_node,
                _ => continue,
            };
            result.push((next_node, seg_id, seg.length_m));
        }
        result
    }

    /// Haversine distance from node to (lon, lat). Returns None if node not loaded.
    pub fn node_dist_m(&self, node: NodeId, lon: f64, lat: f64) -> Option<f64> {
        self.nodes.get(&node).map(|n| haversine_m(n.lon, n.lat, lon, lat))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::Direction;

    fn make_seg(id: u32, start: u32, end: u32, frc: u8, dir: Direction) -> NetworkSegment {
        NetworkSegment {
            id: SegmentId(id),
            start_node: NodeId(start),
            end_node: NodeId(end),
            geometry: vec![(0.0, 0.0), (0.001, 0.001)],
            length_m: 100.0,
            frc,
            fow: 3,
            direction: dir,
        }
    }

    #[test]
    fn successors_respects_lfrcnp() {
        let mut g = Graph::new();
        g.add_segment(make_seg(1, 0, 1, 2, Direction::Both));
        g.add_segment(make_seg(2, 1, 2, 4, Direction::Both)); // FRC=4
        g.add_segment(make_seg(3, 1, 3, 3, Direction::Both)); // FRC=3

        // From node 1 via seg 1, LFRCNP=3 → seg 2 (FRC 4 > 3) should be excluded
        let succs = g.successors(NodeId(1), SegmentId(1), 3);
        let ids: Vec<_> = succs.iter().map(|s| s.1.0).collect();
        assert!(ids.contains(&3), "seg 3 should be included");
        assert!(!ids.contains(&2), "seg 2 FRC=4 exceeds lfrcnp=3");
    }

    #[test]
    fn segments_near_filters_by_radius() {
        let mut g = Graph::new();
        let mut far = make_seg(10, 0, 1, 2, Direction::Both);
        far.geometry = vec![(10.0, 10.0), (10.001, 10.001)]; // far away
        g.add_segment(make_seg(1, 0, 1, 2, Direction::Both));
        g.add_segment(far);
        let near = g.segments_near(0.0005, 0.0005, 200.0);
        assert_eq!(near.len(), 1);
        assert_eq!(near[0].0, SegmentId(1));
    }
}
