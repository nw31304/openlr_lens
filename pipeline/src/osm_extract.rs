use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use osmpbf::{Element, ElementReader, RelMemberType};
use tracing::info;

use crate::extent::Bbox;
use crate::osm_schema::OsmSchemaMapping;
use openlr_graph::Direction;

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct OsmWay {
    pub id:        i64,
    pub node_ids:  Vec<i64>,
    pub frc:       u8,
    pub fow:       u8,
    pub direction: Direction,
}

#[derive(Debug, Clone, Copy)]
pub struct OsmNodeCoord {
    pub lon: f64,
    pub lat: f64,
}

/// A simple (via=single-node) prohibited-turn restriction.
/// Complex via-way restrictions and "only_*" restrictions are skipped for v1.
#[derive(Debug, Clone)]
pub struct OsmRestriction {
    pub from_way_id: i64,
    pub via_node_id: i64,
    pub to_way_id:   i64,
}

pub struct OsmData {
    pub ways:               Vec<OsmWay>,
    pub nodes:              HashMap<i64, OsmNodeCoord>,
    pub intersection_nodes: HashSet<i64>,
    pub restrictions:       Vec<OsmRestriction>,
}

// ── Pass 1 accumulator ────────────────────────────────────────────────────────

#[derive(Default)]
struct P1 {
    ways:         Vec<OsmWay>,
    ref_count:    HashMap<i64, u32>,
    restrictions: Vec<OsmRestriction>,
}

impl P1 {
    fn merge(mut self, other: P1) -> P1 {
        self.ways.extend(other.ways);
        for (id, cnt) in other.ref_count {
            *self.ref_count.entry(id).or_insert(0) += cnt;
        }
        self.restrictions.extend(other.restrictions);
        self
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Read an OSM PBF file and extract highway ways, node coordinates, and turn restrictions.
///
/// If `bbox` is given, only ways that have at least one node inside the bbox are kept;
/// all nodes referenced by kept ways are retained (including nodes slightly outside the bbox
/// that are part of roads crossing the boundary).
///
/// Attribute derivation (FRC, FOW, direction) is performed during pass 1 using the schema,
/// so `OsmWay` carries pre-derived attributes rather than raw tags.
pub fn extract(path: &Path, bbox: Option<Bbox>, schema: &OsmSchemaMapping) -> Result<OsmData> {
    let schema_arc = Arc::new(schema.clone());

    // ── Pass 1: ways and relations ────────────────────────────────────────────
    let reader1 = ElementReader::from_path(path)?;
    let schema1 = Arc::clone(&schema_arc);

    let p1: P1 = reader1.par_map_reduce(
        move |el| {
            let mut acc = P1::default();
            match el {
                Element::Way(w) => {
                    let mut highway:          Option<&str> = None;
                    let mut is_roundabout:    bool         = false;
                    let mut oneway:           i8           = 0;
                    let mut dual_carriageway: bool         = false;
                    let mut excluded:         bool         = false;

                    for (key, val) in w.tags() {
                        match key {
                            "highway" => highway = Some(val),
                            "junction" => {
                                if val == "roundabout" || val == "mini_roundabout" {
                                    is_roundabout = true;
                                }
                            }
                            "oneway" => {
                                oneway = match val {
                                    "yes" | "true" | "1" => 1,
                                    "-1" | "reverse"     => -1,
                                    _                    => 0,
                                };
                            }
                            "dual_carriageway" => {
                                if val == "yes" {
                                    dual_carriageway = true;
                                }
                            }
                            other => {
                                if let Some(exclusion_vals) = schema1.exclusions.get(other) {
                                    if exclusion_vals.iter().any(|ev| ev == val) {
                                        excluded = true;
                                    }
                                }
                            }
                        }
                    }

                    // Skip if excluded, no highway tag, or schema returns None / non-vehicular
                    if excluded {
                        return acc;
                    }
                    let hw = match highway {
                        Some(h) => h,
                        None    => return acc,
                    };
                    let (frc, base_fow, is_vehicular) = match schema1.lookup(hw) {
                        Some(attrs) => attrs,
                        None        => return acc,
                    };
                    if !is_vehicular {
                        return acc;
                    }

                    let fow = if is_roundabout {
                        4
                    } else if dual_carriageway {
                        2
                    } else {
                        base_fow
                    };

                    let direction = if is_roundabout {
                        Direction::Forward
                    } else {
                        match oneway {
                            1  => Direction::Forward,
                            -1 => Direction::Backward,
                            _  => Direction::Both,
                        }
                    };

                    let node_ids: Vec<i64> = w.refs().collect();
                    if node_ids.len() < 2 {
                        return acc;
                    }

                    // Build ref_count: endpoints get +2; interior nodes get +1.
                    // This ensures endpoints are always intersection nodes (cnt >= 2).
                    let last = node_ids.len() - 1;
                    for (i, &nid) in node_ids.iter().enumerate() {
                        let delta: u32 = if i == 0 || i == last { 2 } else { 1 };
                        *acc.ref_count.entry(nid).or_insert(0) += delta;
                    }

                    acc.ways.push(OsmWay { id: w.id(), node_ids, frc, fow, direction });
                }
                Element::Relation(r) => {
                    let mut is_restriction = false;
                    let mut is_no_turn    = false;
                    for (k, v) in r.tags() {
                        match k {
                            "type"        => is_restriction = v == "restriction",
                            "restriction" => is_no_turn     = v.starts_with("no_"),
                            _ => {}
                        }
                    }
                    if !is_restriction || !is_no_turn {
                        return acc;
                    }

                    let mut from_way = None;
                    let mut via_node = None;
                    let mut to_way   = None;
                    for member in r.members() {
                        let role = member.role().unwrap_or("");
                        match (member.member_type, role) {
                            (RelMemberType::Way,  "from") => from_way = Some(member.member_id),
                            (RelMemberType::Node, "via")  => via_node = Some(member.member_id),
                            (RelMemberType::Way,  "to")   => to_way   = Some(member.member_id),
                            _ => {}
                        }
                    }
                    if let (Some(f), Some(v), Some(t)) = (from_way, via_node, to_way) {
                        acc.restrictions.push(OsmRestriction {
                            from_way_id: f,
                            via_node_id: v,
                            to_way_id:   t,
                        });
                    }
                }
                // Skip nodes in pass 1
                Element::Node(_) | Element::DenseNode(_) => {}
            }
            acc
        },
        P1::default,
        P1::merge,
    )?;

    info!(
        ways         = p1.ways.len(),
        needed_nodes = p1.ref_count.len(),
        restrictions = p1.restrictions.len(),
        "OSM pass 1 complete"
    );

    // ── Pass 2: nodes only ────────────────────────────────────────────────────
    let ref_count_arc = Arc::new(p1.ref_count);
    let rc2 = Arc::clone(&ref_count_arc);

    let reader2 = ElementReader::from_path(path)?;
    let all_node_coords: HashMap<i64, OsmNodeCoord> = reader2.par_map_reduce(
        move |el| {
            let mut map: HashMap<i64, OsmNodeCoord> = HashMap::new();
            match el {
                Element::Node(n) => {
                    if rc2.contains_key(&n.id()) {
                        map.insert(n.id(), OsmNodeCoord { lon: n.lon(), lat: n.lat() });
                    }
                }
                Element::DenseNode(n) => {
                    if rc2.contains_key(&n.id()) {
                        map.insert(n.id(), OsmNodeCoord { lon: n.lon(), lat: n.lat() });
                    }
                }
                Element::Way(_) | Element::Relation(_) => {}
            }
            map
        },
        HashMap::new,
        |mut a, b| { a.extend(b); a },
    )?;

    info!(nodes_loaded = all_node_coords.len(), "OSM pass 2 complete");

    // ── Build intersection_nodes BEFORE bbox filtering ────────────────────────
    let intersection_nodes: HashSet<i64> = ref_count_arc
        .iter()
        .filter(|(_, &cnt)| cnt >= 2)
        .map(|(&id, _)| id)
        .collect();

    let P1 { mut ways, ref_count: _, restrictions } = p1;
    let mut all_node_coords = all_node_coords;

    // ── Bbox filter (applied after intersection_nodes computed) ───────────────
    if let Some(b) = bbox {
        let bbox_node_set: HashSet<i64> = all_node_coords
            .iter()
            .filter(|(_, c)| {
                c.lon >= b.west && c.lon <= b.east && c.lat >= b.south && c.lat <= b.north
            })
            .map(|(id, _)| *id)
            .collect();

        ways.retain(|w| w.node_ids.iter().any(|id| bbox_node_set.contains(id)));

        let referenced: HashSet<i64> = ways
            .iter()
            .flat_map(|w| w.node_ids.iter().copied())
            .collect();
        all_node_coords.retain(|id, _| referenced.contains(id));

        info!(
            ways  = ways.len(),
            nodes = all_node_coords.len(),
            "after bbox filter"
        );
    }

    Ok(OsmData {
        ways,
        nodes: all_node_coords,
        intersection_nodes,
        restrictions,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_coord(lon: f64, lat: f64) -> OsmNodeCoord {
        OsmNodeCoord { lon, lat }
    }

    #[test]
    fn bbox_filter_keeps_ways_with_any_node_inside() {
        let bbox = Bbox { west: 170.0, south: -40.0, east: 175.0, north: -35.0 };
        let mut nodes = HashMap::new();
        nodes.insert(1, make_coord(172.0, -38.0)); // inside
        nodes.insert(2, make_coord(180.0, -38.0)); // outside
        nodes.insert(3, make_coord(173.0, -39.0)); // inside
        nodes.insert(4, make_coord(169.0, -38.0)); // outside

        let mut ways = vec![
            OsmWay { id: 10, node_ids: vec![1, 2], frc: 2, fow: 3, direction: Direction::Both },
            OsmWay { id: 11, node_ids: vec![2, 4], frc: 2, fow: 3, direction: Direction::Both },
            OsmWay { id: 12, node_ids: vec![1, 3], frc: 2, fow: 3, direction: Direction::Both },
        ];

        let bbox_node_set: std::collections::HashSet<i64> = nodes.iter()
            .filter(|(_, c)| c.lon >= bbox.west && c.lon <= bbox.east && c.lat >= bbox.south && c.lat <= bbox.north)
            .map(|(id, _)| *id)
            .collect();
        ways.retain(|w| w.node_ids.iter().any(|id| bbox_node_set.contains(id)));

        // Ways 10 and 12 have node 1 (inside bbox); way 11 has no nodes in bbox.
        assert_eq!(ways.len(), 2);
        assert!(ways.iter().any(|w| w.id == 10));
        assert!(ways.iter().any(|w| w.id == 12));
        assert!(ways.iter().all(|w| w.id != 11));
    }

    #[test]
    fn bbox_filter_retains_boundary_crossing_nodes() {
        // Way with one node inside, one outside: both nodes must be kept after filter.
        let bbox = Bbox { west: 170.0, south: -40.0, east: 175.0, north: -35.0 };
        let mut nodes = HashMap::new();
        nodes.insert(1, make_coord(172.0, -38.0)); // inside
        nodes.insert(2, make_coord(180.0, -38.0)); // outside

        let ways = vec![
            OsmWay { id: 10, node_ids: vec![1, 2], frc: 2, fow: 3, direction: Direction::Both },
        ];

        let bbox_node_set: HashSet<i64> = nodes.iter()
            .filter(|(_, c)| c.lon >= bbox.west && c.lon <= bbox.east && c.lat >= bbox.south && c.lat <= bbox.north)
            .map(|(id, _)| *id)
            .collect();

        let kept_ways: Vec<_> = ways.iter()
            .filter(|w| w.node_ids.iter().any(|id| bbox_node_set.contains(id)))
            .collect();
        let referenced: HashSet<i64> = kept_ways.iter()
            .flat_map(|w| w.node_ids.iter().copied())
            .collect();
        nodes.retain(|id, _| referenced.contains(id));

        assert_eq!(nodes.len(), 2, "boundary node 2 must be retained");
        assert!(nodes.contains_key(&1));
        assert!(nodes.contains_key(&2));
    }
}
