#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct NodeId(pub u32);

#[derive(Debug, Clone)]
pub struct NetworkNode {
    pub id: NodeId,
    pub lon: f64,
    pub lat: f64,
    /// Stable 16-byte identifier used for cross-tile stitching.
    /// Overture data: GERS UUID (little-endian bytes).
    /// OSM data: bytes 0–7 = zeros, bytes 8–15 = OSM node ID as u64 LE.
    pub stable_id: [u8; 16],
    pub is_boundary: bool,
}
