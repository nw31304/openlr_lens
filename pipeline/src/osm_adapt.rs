use std::collections::{HashMap, HashSet};

use rayon::prelude::*;
use tracing::{trace, warn};

use crate::osm_extract::{OsmData, OsmNodeCoord, OsmWay};
use crate::restrictions::{encode_restriction_flags, RestrictionTriple, HEADING_ANY};
use crate::split::{polyline_length_m, NodeRecord, SplitEdge};
use openlr_graph::Direction;

// ── OSM ID encoding ───────────────────────────────────────────────────────────
//
// 16-byte stable IDs derived from OSM numeric IDs (Invariant 2).
//
// Node ID:   [0u8; 8]  ++ (node_id as u64).to_le_bytes()
// Way ID:    (way_id as u64).to_le_bytes() ++ [0u8; 8]
//
// The two spaces are disjoint: a node ID always has zeroes in bytes 0–7 and a
// way ID always has zeroes in bytes 8–15, so they can never accidentally collide.
// The tile writer uses `(parent_gers_id, end_node_gers)` as the restriction lookup
// key, which requires from_segment_gers == parent_gers_id (way encoding) and
// via_connector_gers == end/start_node_gers (node encoding).

pub(crate) fn encode_node_id(id: i64) -> [u8; 16] {
    let mut buf = [0u8; 16];
    buf[8..16].copy_from_slice(&(id as u64).to_le_bytes());
    buf
}

pub(crate) fn encode_way_id(id: i64) -> [u8; 16] {
    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&(id as u64).to_le_bytes());
    buf
}

// ── Way splitting ─────────────────────────────────────────────────────────────

fn split_way(
    way: &OsmWay,
    intersection_nodes: &HashSet<i64>,
    node_coords: &HashMap<i64, OsmNodeCoord>,
    frc: u8,
    fow: u8,
    direction: Direction,
) -> (Vec<SplitEdge>, Vec<NodeRecord>) {
    if way.node_ids.len() < 2 {
        return (vec![], vec![]);
    }

    let parent_gers = encode_way_id(way.id);

    // Collect the start-indices of each sub-edge: always 0, plus every interior
    // node that is a road intersection (shared by 2+ ways).
    let mut split_starts: Vec<usize> = vec![0];
    let last = way.node_ids.len() - 1;
    for (i, &nid) in way.node_ids[1..last].iter().enumerate() {
        if intersection_nodes.contains(&nid) {
            split_starts.push(i + 1); // convert slice index to way-node index
        }
    }

    let mut edges: Vec<SplitEdge>  = Vec::with_capacity(split_starts.len());
    let mut nodes: Vec<NodeRecord> = Vec::with_capacity(split_starts.len() + 1);

    for (k, &start_idx) in split_starts.iter().enumerate() {
        let end_idx = if k + 1 < split_starts.len() { split_starts[k + 1] } else { last };

        // Collect geometry for this sub-edge.
        let mut geom: Vec<(f64, f64)> = Vec::with_capacity(end_idx - start_idx + 1);
        let mut ok = true;
        for &nid in &way.node_ids[start_idx..=end_idx] {
            if let Some(c) = node_coords.get(&nid) {
                geom.push((c.lon, c.lat));
            } else {
                warn!(way = way.id, node = nid, "missing node coordinates, sub-edge skipped");
                ok = false;
                break;
            }
        }
        if !ok || geom.len() < 2 {
            continue;
        }

        let start_nid = way.node_ids[start_idx];
        let end_nid   = way.node_ids[end_idx];
        let start_gers = encode_node_id(start_nid);
        let end_gers   = encode_node_id(end_nid);
        let length_m   = polyline_length_m(&geom);

        trace!(way = way.id, start = start_nid, end = end_nid, length_m, "sub-edge");

        nodes.push(NodeRecord { gers_id: start_gers, lon: geom[0].0,              lat: geom[0].1 });
        nodes.push(NodeRecord { gers_id: end_gers,   lon: geom.last().unwrap().0, lat: geom.last().unwrap().1 });

        edges.push(SplitEdge {
            start_node_gers: start_gers,
            end_node_gers:   end_gers,
            geometry:        geom,
            length_m,
            frc,
            fow,
            direction,
            parent_gers_id:  parent_gers,
        });
    }

    (edges, nodes)
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Convert raw OSM data into the tile-pipeline's edge/node/restriction types.
///
/// Attribute derivation (FRC, FOW, direction) was already performed during extract;
/// this function goes straight to parallel splitting.
pub fn adapt(data: OsmData) -> (Vec<SplitEdge>, Vec<NodeRecord>, Vec<RestrictionTriple>) {
    let OsmData { ways, nodes, intersection_nodes, restrictions } = data;

    // ── Split ways in parallel ────────────────────────────────────────────────

    let results: Vec<(Vec<SplitEdge>, Vec<NodeRecord>)> = ways
        .par_iter()
        .map(|wa| split_way(wa, &intersection_nodes, &nodes, wa.frc, wa.fow, wa.direction))
        .collect();

    let mut all_edges: Vec<SplitEdge>                = Vec::new();
    let mut node_map:  HashMap<[u8; 16], NodeRecord> = HashMap::new();
    for (edges, node_records) in results {
        all_edges.extend(edges);
        for n in node_records {
            node_map.insert(n.gers_id, n); // last writer wins; coords should agree
        }
    }
    let all_nodes: Vec<NodeRecord> = node_map.into_values().collect();

    // ── Convert turn restrictions ─────────────────────────────────────────────
    //
    // from_segment_gers = encode_way_id(from_way_id)  → matches parent_gers_id of FROM sub-edge
    // via_connector_gers = encode_node_id(via_node_id) → matches end_node_gers of FROM sub-edge
    //                                                    and start_node_gers of TO sub-edge
    // to_segment_gers   = encode_way_id(to_way_id)   → matches parent_gers_id of TO sub-edge
    //
    // No heading conditions in basic OSM restrictions (those are in restriction:conditional).

    let all_restrictions: Vec<RestrictionTriple> = restrictions
        .iter()
        .map(|r| RestrictionTriple {
            from_segment_gers:  encode_way_id(r.from_way_id),
            via_connector_gers: encode_node_id(r.via_node_id),
            to_segment_gers:    encode_way_id(r.to_way_id),
            flags: encode_restriction_flags(HEADING_ANY, HEADING_ANY),
        })
        .collect();

    (all_edges, all_nodes, all_restrictions)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osm_extract::{OsmData, OsmNodeCoord};

    fn coord(lon: f64, lat: f64) -> OsmNodeCoord {
        OsmNodeCoord { lon, lat }
    }

    fn make_nodes(pairs: &[(i64, f64, f64)]) -> HashMap<i64, OsmNodeCoord> {
        pairs.iter().map(|&(id, lon, lat)| (id, coord(lon, lat))).collect()
    }

    fn way(id: i64, node_ids: Vec<i64>, frc: u8, fow: u8, direction: Direction) -> OsmWay {
        OsmWay { id, node_ids, frc, fow, direction }
    }

    // ── ID encoding ───────────────────────────────────────────────────────────

    #[test]
    fn node_id_encoding_is_stable() {
        let id = 123_456_789i64;
        let enc = encode_node_id(id);
        assert_eq!(&enc[0..8], &[0u8; 8]); // zeroes in first 8 bytes
        let back = i64::from_le_bytes(enc[8..16].try_into().unwrap());
        assert_eq!(back, id);
    }

    #[test]
    fn way_id_encoding_is_stable() {
        let id = 987_654_321i64;
        let enc = encode_way_id(id);
        assert_eq!(&enc[8..16], &[0u8; 8]); // zeroes in last 8 bytes
        let back = i64::from_le_bytes(enc[0..8].try_into().unwrap());
        assert_eq!(back, id);
    }

    #[test]
    fn node_and_way_encodings_are_disjoint() {
        // A node ID that equals a way ID must produce different 16-byte values.
        let same_numeric = 42i64;
        assert_ne!(encode_node_id(same_numeric), encode_way_id(same_numeric));
    }

    // ── Way splitting ─────────────────────────────────────────────────────────

    #[test]
    fn simple_way_no_interior_intersection_gives_one_edge() {
        // Way A–B–C; B is not in any other way.
        let w = way(1, vec![1, 2, 3], 1, 3, Direction::Both);
        let nodes = make_nodes(&[(1, 174.0, -36.0), (2, 174.5, -36.0), (3, 175.0, -36.0)]);
        let mut intersections = HashSet::new();
        intersections.insert(1i64); // endpoints
        intersections.insert(3i64);

        let (edges, node_records) = split_way(&w, &intersections, &nodes, 1, 3, Direction::Both);
        assert_eq!(edges.len(), 1);
        assert_eq!(node_records.len(), 2);
        assert_eq!(edges[0].geometry.len(), 3); // all original vertices kept
        assert!(edges[0].length_m > 0.0);
    }

    #[test]
    fn interior_intersection_splits_into_two_edges() {
        // Way A–B–C–D; B is shared with another way.
        let w = way(1, vec![10, 20, 30, 40], 1, 3, Direction::Both);
        let nodes = make_nodes(&[
            (10, 174.0, -36.0),
            (20, 174.25, -36.0),
            (30, 174.5, -36.0),
            (40, 175.0, -36.0),
        ]);
        let mut intersections = HashSet::new();
        intersections.insert(10i64);
        intersections.insert(20i64); // interior intersection
        intersections.insert(40i64);

        let (edges, _) = split_way(&w, &intersections, &nodes, 1, 3, Direction::Both);
        assert_eq!(edges.len(), 2);
        // Sub-edge 1: nodes 10,20 → 2 geometry points
        assert_eq!(edges[0].geometry.len(), 2);
        // Sub-edge 2: nodes 20,30,40 → 3 geometry points
        assert_eq!(edges[1].geometry.len(), 3);
    }

    #[test]
    fn roundabout_edges_are_forward_fow4() {
        // Roundabout: fow=4, direction=Forward already set at extract time.
        let w = way(99, vec![1, 2, 3], 2, 4, Direction::Forward);
        let nodes = make_nodes(&[
            (1, 174.0, -36.0),
            (2, 174.1, -36.0),
            (3, 174.2, -36.0),
        ]);
        let mut intersections = HashSet::new();
        intersections.insert(1i64);
        intersections.insert(3i64);
        let (edges, _) = split_way(&w, &intersections, &nodes, 2, 4, Direction::Forward);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].fow, 4);
        assert_eq!(edges[0].direction, Direction::Forward);
    }

    // ── Full adapt pass ───────────────────────────────────────────────────────

    #[test]
    fn adapt_correctly_splits_at_intersection_node() {
        // Single vehicular way with one interior intersection node → 2 edges.
        let mut intersection_nodes = HashSet::new();
        intersection_nodes.insert(1i64);
        intersection_nodes.insert(2i64); // interior intersection
        intersection_nodes.insert(3i64);

        let data = OsmData {
            ways: vec![
                way(1, vec![1, 2, 3], 2, 3, Direction::Both),
            ],
            nodes: make_nodes(&[(1, 174.0, -36.0), (2, 174.5, -36.0), (3, 175.0, -36.0)]),
            intersection_nodes,
            restrictions: vec![],
        };
        let (edges, _, _) = adapt(data);
        assert_eq!(edges.len(), 2, "way should be split into 2 edges at intersection node");
    }

    #[test]
    fn adapt_produces_restriction_triple() {
        use crate::osm_extract::OsmRestriction as Restriction;

        let mut intersection_nodes = HashSet::new();
        intersection_nodes.insert(1i64);
        intersection_nodes.insert(5i64);
        intersection_nodes.insert(2i64);
        intersection_nodes.insert(3i64);

        let data = OsmData {
            ways: vec![
                way(100, vec![1, 5, 2], 2, 3, Direction::Both),
                way(200, vec![5, 3],    2, 3, Direction::Both),
            ],
            nodes: make_nodes(&[
                (1, 174.0, -36.0),
                (5, 174.5, -36.0),
                (2, 175.0, -36.0),
                (3, 174.5, -36.5),
            ]),
            intersection_nodes,
            restrictions: vec![Restriction {
                from_way_id: 100,
                via_node_id: 5,
                to_way_id:   200,
            }],
        };
        let (_, _, restrictions) = adapt(data);
        assert_eq!(restrictions.len(), 1);
        assert_eq!(restrictions[0].from_segment_gers,  encode_way_id(100));
        assert_eq!(restrictions[0].via_connector_gers, encode_node_id(5));
        assert_eq!(restrictions[0].to_segment_gers,    encode_way_id(200));
    }
}
