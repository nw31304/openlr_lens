//! Parse OLRL v2 binary tile payloads into the in-memory Graph.
//!
//! Binary layout (all integers little-endian):
//!
//! Header       40 bytes
//! Segment array   segment_count × 32 bytes
//! GERS-id table   segment_count × 16 bytes   (v2 only)
//! Geometry pool   geom_vertex_count × 8 bytes
//! Node table      node_count × 28 bytes
//! Intra restrictions  restriction_count × 16 bytes
//! Cross-tile restrictions  xrestriction_count × 40 bytes

use std::collections::HashMap;

use openlr_graph::{
    Direction, Graph, NetworkNode, NetworkSegment, NodeId, SegmentId, TurnRestriction,
};

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum TileReadError {
    #[error("bad magic: expected OLRL, got {0:?}")]
    BadMagic([u8; 4]),
    #[error("unsupported tile version {0} (expected 2)")]
    UnsupportedVersion(u8),
    #[error("tile payload too short: need at least {need} bytes, have {have}")]
    TooShort { need: usize, have: usize },
    #[error("geometry index out of range: offset {offset} + len {len} > pool size {pool}")]
    GeomOutOfRange { offset: usize, len: usize, pool: usize },
    #[error("local node index {0} out of range")]
    NodeIndexOob(usize),
    #[error("local segment index {0} out of range")]
    SegIndexOob(usize),
}

// ── Tile loader (multi-tile, boundary-node stitching) ─────────────────────────

/// Accumulates tiles into an in-memory `Graph`, stitching boundary nodes across tiles
/// by matching their GERS IDs.
pub struct TileLoader {
    pub graph: Graph,
    /// GERS ID → global NodeId, for boundary nodes seen in previously loaded tiles.
    boundary_nodes: HashMap<[u8; 16], NodeId>,
    next_node_id: u32,
    next_seg_id: u32,
}

impl Default for TileLoader {
    fn default() -> Self { Self::new() }
}

impl TileLoader {
    pub fn new() -> Self {
        Self {
            graph: Graph::new(),
            boundary_nodes: HashMap::new(),
            next_node_id: 0,
            next_seg_id: 0,
        }
    }

    /// Parse one OLRL v2 tile payload and merge it into the graph.
    pub fn load_tile(&mut self, bytes: &[u8]) -> Result<(), TileReadError> {
        parse_tile(
            bytes,
            &mut self.graph,
            &mut self.boundary_nodes,
            &mut self.next_node_id,
            &mut self.next_seg_id,
        )
    }
}

// ── Parser ────────────────────────────────────────────────────────────────────

fn parse_tile(
    b: &[u8],
    graph: &mut Graph,
    boundary_nodes: &mut HashMap<[u8; 16], NodeId>,
    next_node: &mut u32,
    next_seg: &mut u32,
) -> Result<(), TileReadError> {
    require(b, 40)?;

    let magic: [u8; 4] = b[0..4].try_into().unwrap();
    if &magic != b"OLRL" {
        return Err(TileReadError::BadMagic(magic));
    }
    if b[4] != 2 {
        return Err(TileReadError::UnsupportedVersion(b[4]));
    }

    let seg_count   = u32_le(b, 8)  as usize;
    let node_count  = u32_le(b, 12) as usize;
    let restr_count = u32_le(b, 16) as usize;
    let geom_count  = u32_le(b, 20) as usize;
    let xrestr_count= u32_le(b, 24) as usize;

    // Compute section offsets.
    let seg_off   = 40;
    let gers_off  = seg_off  + seg_count   * 32;  // GERS-id table (v2)
    let geom_off  = gers_off + seg_count   * 16;
    let node_off  = geom_off + geom_count  * 8;
    let restr_off = node_off + node_count  * 28;
    let xrestr_off= restr_off + restr_count * 16;
    let min_len   = xrestr_off + xrestr_count * 40;

    require(b, min_len)?;

    // ── Geometry pool ────────────────────────────────────────────────────────
    let geom_pool: Vec<(f64, f64)> = (0..geom_count)
        .map(|i| {
            let o = geom_off + i * 8;
            let lon = i32_le(b, o)     as f64 / 1e7;
            let lat = i32_le(b, o + 4) as f64 / 1e7;
            (lon, lat)
        })
        .collect();

    // ── Node table ───────────────────────────────────────────────────────────
    let mut local_node: Vec<NodeId> = Vec::with_capacity(node_count);
    for i in 0..node_count {
        let o = node_off + i * 28;
        let lon = i32_le(b, o)     as f64 / 1e7;
        let lat = i32_le(b, o + 4) as f64 / 1e7;
        let stable_id: [u8; 16] = b[o+8..o+24].try_into().unwrap();
        let is_boundary = b[o + 24] & 0x01 != 0;

        let node_id = if is_boundary {
            *boundary_nodes.entry(stable_id).or_insert_with(|| {
                let id = NodeId(*next_node);
                *next_node += 1;
                id
            })
        } else {
            let id = NodeId(*next_node);
            *next_node += 1;
            id
        };

        if !graph.nodes.contains_key(&node_id) {
            graph.add_node(NetworkNode { id: node_id, lon, lat, stable_id, is_boundary });
        }
        local_node.push(node_id);
    }

    // ── Segment array ────────────────────────────────────────────────────────
    let mut local_seg: Vec<SegmentId> = Vec::with_capacity(seg_count);
    for i in 0..seg_count {
        let o = seg_off + i * 32;
        let start_local = u32_le(b, o)     as usize;
        let end_local   = u32_le(b, o + 4) as usize;
        let geom_idx    = u32_le(b, o + 8) as usize;
        let geom_len    = u16_le(b, o + 12) as usize;
        let length_cm   = u32_le(b, o + 14);
        let attrs       = b[o + 18];

        if start_local >= node_count { return Err(TileReadError::NodeIndexOob(start_local)); }
        if end_local   >= node_count { return Err(TileReadError::NodeIndexOob(end_local)); }
        if geom_idx + geom_len > geom_count {
            return Err(TileReadError::GeomOutOfRange { offset: geom_idx, len: geom_len, pool: geom_count });
        }

        let frc = attrs & 0x07;
        let fow = (attrs >> 3) & 0x07;
        let direction = match (attrs >> 6) & 0x03 {
            1 => Direction::Forward,
            2 => Direction::Backward,
            _ => Direction::Both,
        };
        let geometry = geom_pool[geom_idx..geom_idx + geom_len].to_vec();

        let seg_id = SegmentId(*next_seg);
        *next_seg += 1;
        local_seg.push(seg_id);

        graph.add_segment(NetworkSegment {
            id: seg_id,
            start_node: local_node[start_local],
            end_node:   local_node[end_local],
            geometry,
            length_m: length_cm as f64 / 100.0,
            frc,
            fow,
            direction,
        });
    }

    // ── Intra-tile restrictions ───────────────────────────────────────────────
    for i in 0..restr_count {
        let o = restr_off + i * 16;
        let from = u32_le(b, o)     as usize;
        let via  = u32_le(b, o + 4) as usize;
        let to   = u32_le(b, o + 8) as usize;

        if from >= seg_count  { return Err(TileReadError::SegIndexOob(from)); }
        if to   >= seg_count  { return Err(TileReadError::SegIndexOob(to)); }
        if via  >= node_count { return Err(TileReadError::NodeIndexOob(via)); }

        graph.add_restriction(TurnRestriction {
            from_seg: local_seg[from],
            via_node: local_node[via],
            to_seg:   local_seg[to],
        });
    }

    // Cross-tile restrictions: stored but not yet stitched in v1 (requires a
    // second pass once all tiles are loaded; see TileLoader::stitch_cross_tile).

    Ok(())
}

// ── Byte helpers ──────────────────────────────────────────────────────────────

fn require(b: &[u8], n: usize) -> Result<(), TileReadError> {
    if b.len() < n { Err(TileReadError::TooShort { need: n, have: b.len() }) } else { Ok(()) }
}

fn u32_le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o+1], b[o+2], b[o+3]])
}
fn i32_le(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o+1], b[o+2], b[o+3]])
}
fn u16_le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o+1]])
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal 2-segment, 3-node tile payload for testing.
    fn minimal_tile() -> Vec<u8> {
        let seg_count: u32 = 2;
        let node_count: u32 = 3;
        let restr_count: u32 = 0;
        let xrestr_count: u32 = 0;
        // geometry: 2 vertices per segment = 4 total
        let geom_count: u32 = 4;

        let mut buf = vec![0u8; 0];

        // Header (40 bytes)
        buf.extend_from_slice(b"OLRL");     // magic
        buf.push(2);                        // version
        buf.push(0);                        // flags
        buf.extend_from_slice(&[0u8; 2]);   // pad
        buf.extend_from_slice(&seg_count.to_le_bytes());
        buf.extend_from_slice(&node_count.to_le_bytes());
        buf.extend_from_slice(&restr_count.to_le_bytes());
        buf.extend_from_slice(&geom_count.to_le_bytes());
        buf.extend_from_slice(&xrestr_count.to_le_bytes());
        buf.extend_from_slice(&[0u8; 12]); // reserved
        assert_eq!(buf.len(), 40);

        // Segment array (2 × 32 bytes)
        // Seg 0: nodes 0→1, geom [0,2), len 100 m (10000 cm), FRC=3, FOW=3, Both
        let attrs0: u8 = 3 | (3 << 3) | (0 << 6); // frc=3, fow=3, dir=Both
        let mut seg0 = [0u8; 32];
        seg0[0..4].copy_from_slice(&0u32.to_le_bytes());  // start_node local 0
        seg0[4..8].copy_from_slice(&1u32.to_le_bytes());  // end_node local 1
        seg0[8..12].copy_from_slice(&0u32.to_le_bytes()); // geom_offset 0
        seg0[12..14].copy_from_slice(&2u16.to_le_bytes());// geom_len 2
        seg0[14..18].copy_from_slice(&10_000u32.to_le_bytes()); // 100m
        seg0[18] = attrs0;
        buf.extend_from_slice(&seg0);

        // Seg 1: nodes 1→2, geom [2,4), len 150 m (15000 cm), FRC=3, FOW=3, Forward
        let attrs1: u8 = 3 | (3 << 3) | (1 << 6);
        let mut seg1 = [0u8; 32];
        seg1[0..4].copy_from_slice(&1u32.to_le_bytes());
        seg1[4..8].copy_from_slice(&2u32.to_le_bytes());
        seg1[8..12].copy_from_slice(&2u32.to_le_bytes());
        seg1[12..14].copy_from_slice(&2u16.to_le_bytes());
        seg1[14..18].copy_from_slice(&15_000u32.to_le_bytes());
        seg1[18] = attrs1;
        buf.extend_from_slice(&seg1);

        // GERS-id table (2 × 16 bytes = 32 bytes), all zeros fine for test
        buf.extend_from_slice(&[0u8; 32]);

        // Geometry pool (4 × 8 bytes)
        // Vertex 0: lon=174.0 lat=-36.0 → 1740000000, -360000000
        let lon0: i32 = 1_740_000_000;
        let lat0: i32 = -360_000_000;
        let lon1: i32 = 1_740_010_000;
        let lat1: i32 = -360_000_000;
        let lon2: i32 = 1_740_010_000;
        let lat2: i32 = -360_010_000;
        let lon3: i32 = 1_740_020_000;
        let lat3: i32 = -360_010_000;
        for &(lon, lat) in &[(lon0,lat0),(lon1,lat1),(lon2,lat2),(lon3,lat3)] {
            buf.extend_from_slice(&lon.to_le_bytes());
            buf.extend_from_slice(&lat.to_le_bytes());
        }

        // Node table (3 × 28 bytes)
        for (lon_e7, lat_e7) in [(lon0,lat0),(lon1,lat1),(lon2,lat2)] {
            buf.extend_from_slice(&lon_e7.to_le_bytes());
            buf.extend_from_slice(&lat_e7.to_le_bytes());
            buf.extend_from_slice(&[0u8; 16]); // stable_id (zeros for test)
            buf.push(0); // flags: not boundary
            buf.extend_from_slice(&[0u8; 3]); // pad
        }

        buf
    }

    #[test]
    fn parse_minimal_tile() {
        let bytes = minimal_tile();
        let mut loader = TileLoader::new();
        loader.load_tile(&bytes).unwrap();
        let g = &loader.graph;
        assert_eq!(g.segments.len(), 2, "segment count");
        assert_eq!(g.nodes.len(), 3, "node count");
    }

    #[test]
    fn segment_lengths_correct() {
        let bytes = minimal_tile();
        let mut loader = TileLoader::new();
        loader.load_tile(&bytes).unwrap();
        let segs: Vec<_> = loader.graph.segments.values().collect();
        let lengths: std::collections::HashSet<u32> =
            segs.iter().map(|s| s.length_m as u32).collect();
        assert!(lengths.contains(&100), "100 m segment");
        assert!(lengths.contains(&150), "150 m segment");
    }

    #[test]
    fn direction_decoded_correctly() {
        let bytes = minimal_tile();
        let mut loader = TileLoader::new();
        loader.load_tile(&bytes).unwrap();
        let segs: Vec<_> = loader.graph.segments.values().collect();
        let dirs: Vec<Direction> = segs.iter().map(|s| s.direction).collect();
        assert!(dirs.contains(&Direction::Both));
        assert!(dirs.contains(&Direction::Forward));
    }

    #[test]
    fn bad_magic_rejected() {
        let mut bytes = minimal_tile();
        bytes[0] = b'X';
        let mut loader = TileLoader::new();
        assert!(matches!(loader.load_tile(&bytes), Err(TileReadError::BadMagic(_))));
    }

    #[test]
    fn wrong_version_rejected() {
        let mut bytes = minimal_tile();
        bytes[4] = 1;
        let mut loader = TileLoader::new();
        assert!(matches!(loader.load_tile(&bytes), Err(TileReadError::UnsupportedVersion(1))));
    }

    #[test]
    fn boundary_nodes_stitched_across_tiles() {
        let mut tile1 = minimal_tile();
        let mut tile2 = minimal_tile();

        // Make node 2 in tile1 and node 0 in tile2 both boundary with the same GERS ID.
        let shared_gers = [0xAB; 16];
        // Node 2 in tile1: starts at node_off + 2*28
        // Header=40, segs=2*32=64, gers=32, geom=4*8=32, node_off = 40+64+32+32 = 168
        let node_off = 40 + 2*32 + 2*16 + 4*8;
        let n2_off = node_off + 2 * 28;
        tile1[n2_off + 8..n2_off + 24].copy_from_slice(&shared_gers);
        tile1[n2_off + 24] = 1; // is_boundary

        // Node 0 in tile2
        tile2[node_off + 8..node_off + 24].copy_from_slice(&shared_gers);
        tile2[node_off + 24] = 1; // is_boundary

        let mut loader = TileLoader::new();
        loader.load_tile(&tile1).unwrap();
        loader.load_tile(&tile2).unwrap();

        // Two tiles with 3 nodes each, but node 2 of tile1 = node 0 of tile2 → 5 unique nodes.
        assert_eq!(loader.graph.nodes.len(), 5, "boundary node stitched: 3+3-1=5");
    }
}
