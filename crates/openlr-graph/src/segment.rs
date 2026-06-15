/// Opaque stable segment identifier (tile-local index at runtime;
/// resolved from GERS id during tile load).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct SegmentId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Direction {
    Both,
    Forward,
    Backward,
}

/// A post-split, node-to-node road segment as stored in a loaded tile.
#[derive(Debug, Clone)]
pub struct NetworkSegment {
    pub id: SegmentId,
    pub start_node: super::NodeId,
    pub end_node: super::NodeId,
    /// Ordered WGS84 vertices (longitude, latitude).
    pub geometry: Vec<(f64, f64)>,
    /// Precomputed length in meters (from Overture; do not re-derive from stored geometry).
    pub length_m: f64,
    pub frc: u8,
    pub fow: u8,
    pub direction: Direction,
}
