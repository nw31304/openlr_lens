use std::collections::{HashMap, HashSet};
use std::io::Write as IoWrite;
use std::path::Path;

use anyhow::{Context, Result};
use flate2::{write::GzEncoder, Compression};
use tracing::{info, warn};

use crate::quantize::{QuantizedEdge, QuantizedNode};
use crate::restrictions::RestrictionTriple;
use openlr_graph::Direction;

// ── Slippy tile math ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

/// Web Mercator (lon, lat) → (x, y) slippy tile at zoom z.
fn lon_lat_to_tile_xy(lon_deg: f64, lat_deg: f64, z: u8) -> (u32, u32) {
    let n = (1u64 << z) as f64;
    let x = ((lon_deg + 180.0) / 360.0 * n).floor() as i64;
    let lat_rad = lat_deg.to_radians();
    let merc = (std::f64::consts::FRAC_PI_4 + lat_rad / 2.0).tan().ln();
    let y = ((1.0 - merc / std::f64::consts::PI) / 2.0 * n).floor() as i64;
    let max = (n as i64) - 1;
    (x.clamp(0, max) as u32, y.clamp(0, max) as u32)
}

/// Hilbert index of point (x, y) within an n×n grid (n must be a power of two).
/// Uses the Wikipedia rot(n, ...) convention — `n` is the full grid size at every step.
fn hilbert_d(n: u64, mut x: u64, mut y: u64) -> u64 {
    let mut s = n >> 1;
    let mut d = 0u64;
    while s > 0 {
        let rx = if x & s > 0 { 1u64 } else { 0 };
        let ry = if y & s > 0 { 1u64 } else { 0 };
        d += s * s * ((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                x = n - 1 - x;
                y = n - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        s >>= 1;
    }
    d
}

/// Convert (z, x, y) slippy coordinates to a PMTiles v3 Hilbert tile ID.
/// tile_id = (4^z − 1)/3 + hilbert_index(z, x, y)
pub fn xyz_to_tile_id(z: u8, x: u32, y: u32) -> u64 {
    if z == 0 {
        return 0;
    }
    let acc = ((1u64 << (2 * z as u32)) - 1) / 3;
    let n = 1u64 << z;
    acc + hilbert_d(n, x as u64, y as u64)
}

fn edge_tile_key(edge: &QuantizedEdge, z: u8) -> TileKey {
    let g = &edge.geometry;
    let (lon_e7, lat_e7) = g[g.len() / 2]; // midpoint vertex
    let lon = lon_e7 as f64 * 1e-7;
    let lat = lat_e7 as f64 * 1e-7;
    let (x, y) = lon_lat_to_tile_xy(lon, lat, z);
    TileKey { z, x, y }
}

// ── Per-tile binary payload ───────────────────────────────────────────────────

struct IntraTileRestriction {
    from_seg: u32,
    via_node: u32,
    to_seg: u32,
    flags: u8,
}

struct CrossTileRestriction {
    from_gers: [u8; 16],
    via_node_local: u32,
    to_gers: [u8; 16],
    flags: u8,
}

/// Compute the tile-local node ordering for a set of edges.
/// Returns (node_order, node_index) where node_order[i] is the GERS ID of the
/// i-th local node, and node_index maps GERS ID → local index.
fn compute_tile_node_order(
    tile_edge_indices: &[usize],
    edges: &[QuantizedEdge],
) -> (Vec<[u8; 16]>, HashMap<[u8; 16], u32>) {
    let mut node_order: Vec<[u8; 16]> = Vec::new();
    let mut node_index: HashMap<[u8; 16], u32> = HashMap::new();
    for &idx in tile_edge_indices {
        let e = &edges[idx];
        for gers in [e.start_node_gers, e.end_node_gers] {
            if !node_index.contains_key(&gers) {
                let i = node_order.len() as u32;
                node_order.push(gers);
                node_index.insert(gers, i);
            }
        }
    }
    (node_order, node_index)
}

fn pack_attrs(frc: u8, fow: u8, direction: Direction) -> u8 {
    let dir: u8 = match direction {
        Direction::Both     => 0,
        Direction::Forward  => 1,
        Direction::Backward => 2,
    };
    (frc & 0x07) | ((fow & 0x07) << 3) | (dir << 6)
}

fn build_tile_payload(
    tile_edge_indices: &[usize],
    edges: &[QuantizedEdge],
    node_order: &[[u8; 16]],
    node_index: &HashMap<[u8; 16], u32>,
    node_lookup: &HashMap<[u8; 16], (i32, i32)>,
    boundary_nodes: &HashSet<[u8; 16]>,
    intra: &[IntraTileRestriction],
    cross: &[CrossTileRestriction],
) -> Vec<u8> {
    let segment_count    = tile_edge_indices.len() as u32;
    let node_count       = node_order.len() as u32;
    let restriction_count  = intra.len() as u32;
    let xrestriction_count = cross.len() as u32;

    // Build geometry pool, segment records, and stable-id table (local index → source ID).
    // For OSM tiles the 16-byte id is the encoded OSM way ID (i64 LE in bytes 0-7, zeros 8-15).
    let mut geom_pool: Vec<(i32, i32)> = Vec::new();
    let mut seg_records: Vec<[u8; 32]> = Vec::with_capacity(tile_edge_indices.len());
    let mut seg_gers_ids: Vec<[u8; 16]> = Vec::with_capacity(tile_edge_indices.len());

    for &idx in tile_edge_indices {
        let e = &edges[idx];
        let geom_offset = geom_pool.len() as u32;
        let geom_len    = e.geometry.len() as u16;
        geom_pool.extend_from_slice(&e.geometry);

        let start_node = node_index[&e.start_node_gers];
        let end_node   = node_index[&e.end_node_gers];
        let packed     = pack_attrs(e.frc, e.fow, e.direction);

        let mut r = [0u8; 32];
        r[0..4].copy_from_slice(&start_node.to_le_bytes());
        r[4..8].copy_from_slice(&end_node.to_le_bytes());
        r[8..12].copy_from_slice(&geom_offset.to_le_bytes());
        r[12..14].copy_from_slice(&geom_len.to_le_bytes());
        r[14..18].copy_from_slice(&e.length_cm.to_le_bytes());
        r[18] = packed;
        r[19] = 0; // flags reserved
        // r[20..32] = 0 (reserved)
        seg_records.push(r);
        seg_gers_ids.push(e.parent_gers_id);
    }

    let geom_vertex_count = geom_pool.len() as u32;

    // 40-byte tile header.  Version 2 adds the stable-id table after the segment array.
    let mut hdr = [0u8; 40];
    hdr[0..4].copy_from_slice(&openlr_graph::tile::TILE_MAGIC);
    hdr[4] = 2; // version: 2 adds stable-id table
    hdr[8..12].copy_from_slice(&segment_count.to_le_bytes());
    hdr[12..16].copy_from_slice(&node_count.to_le_bytes());
    hdr[16..20].copy_from_slice(&restriction_count.to_le_bytes());
    hdr[20..24].copy_from_slice(&geom_vertex_count.to_le_bytes());
    hdr[24..28].copy_from_slice(&xrestriction_count.to_le_bytes());

    let cap = 40
        + seg_records.len() * 32
        + seg_gers_ids.len() * 16   // segment GERS-id table
        + geom_pool.len() * 8
        + node_order.len() * 28
        + intra.len() * 16
        + cross.len() * 40;
    let mut payload = Vec::with_capacity(cap);

    payload.extend_from_slice(&hdr);
    for r in &seg_records {
        payload.extend_from_slice(r);
    }
    // Stable-id table: one 16-byte entry per segment, indexed by local segment index.
    for gers in &seg_gers_ids {
        payload.extend_from_slice(gers.as_slice());
    }
    for (lon_e7, lat_e7) in &geom_pool {
        payload.extend_from_slice(&lon_e7.to_le_bytes());
        payload.extend_from_slice(&lat_e7.to_le_bytes());
    }
    for gers in node_order {
        let (lon_e7, lat_e7) = node_lookup.get(gers).copied().unwrap_or_else(|| {
            warn!(gers = %hex::encode(gers), "node not found in lookup, using (0,0)");
            (0, 0)
        });
        let is_boundary = boundary_nodes.contains(gers);
        payload.extend_from_slice(&lon_e7.to_le_bytes());
        payload.extend_from_slice(&lat_e7.to_le_bytes());
        payload.extend_from_slice(gers.as_slice());
        payload.push(u8::from(is_boundary)); // flags: bit 0 = boundary
        payload.extend_from_slice(&[0u8; 3]); // _pad
    }
    for r in intra {
        payload.extend_from_slice(&r.from_seg.to_le_bytes());
        payload.extend_from_slice(&r.via_node.to_le_bytes());
        payload.extend_from_slice(&r.to_seg.to_le_bytes());
        payload.push(r.flags);
        payload.extend_from_slice(&[0u8; 3]); // _pad
    }
    for r in cross {
        payload.extend_from_slice(&r.from_gers);
        payload.extend_from_slice(&r.via_node_local.to_le_bytes());
        payload.extend_from_slice(&r.to_gers);
        payload.push(r.flags);
        payload.extend_from_slice(&[0u8; 3]); // _pad
    }

    payload
}

// ── PMTiles v3 writer ─────────────────────────────────────────────────────────

fn write_uvarint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

/// Encode a PMTiles v3 directory.
///
/// `entries` is a slice of `(tile_id, offset, length, run_length)` where:
/// - `run_length >= 1` → tile entry covering `run_length` consecutive tile IDs
/// - `run_length == 0` → leaf directory pointer (`offset` into the leaf_dirs section,
///   `length` is the compressed byte length of that leaf directory)
///
/// Tile IDs are delta-coded; the offset field uses 0 to signal "immediately follows
/// the previous entry" (the PMTiles sequential-offset optimisation).
fn encode_directory(entries: &[(u64, u64, u32, u32)]) -> Vec<u8> {
    let mut raw = Vec::new();
    write_uvarint(&mut raw, entries.len() as u64);

    let mut last_id = 0u64;
    for &(id, _, _, _) in entries {
        write_uvarint(&mut raw, id - last_id);
        last_id = id;
    }
    for &(_, _, _, rl) in entries {
        write_uvarint(&mut raw, rl as u64);
    }
    for &(_, _, len, _) in entries {
        write_uvarint(&mut raw, len as u64);
    }
    for (i, &(_, offset, _length, _)) in entries.iter().enumerate() {
        if i > 0 {
            let (_, prev_off, prev_len, _) = entries[i - 1];
            if offset == prev_off + prev_len as u64 {
                write_uvarint(&mut raw, 0); // sequential
                continue;
            }
        }
        write_uvarint(&mut raw, offset + 1); // absolute, 1-indexed
    }
    raw
}

fn gzip_compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(data).context("gzip write")?;
    gz.finish().context("gzip finish")
}

/// Maximum tile entries per directory level.
/// At z12 world scale (~500k road tiles) this gives ~31 leaf directories, each
/// fitting comfortably in the root.  At z15 world scale (~5M tiles) it gives
/// ~306 leaf directories — still trivially within a single root.
const ENTRIES_PER_LEAF: usize = 16_384;

/// PMTiles v3 spec: root directory MUST fit within the first 16 384 bytes of the
/// archive.  Header is 127 bytes, leaving 16 257 bytes for the compressed root.
const MAX_ROOT_COMPRESSED_BYTES: usize = 16_384 - 127;

struct DirectoryParts {
    root_compressed: Vec<u8>,
    leaf_dirs_data: Vec<u8>, // compressed leaf directories concatenated
    n_tiles: u64,            // total tile entries (run_length ≥ 1)
}

/// Build the PMTiles v3 directory structure.
///
/// Tries a flat root first; falls back to a 2-level hierarchy if the compressed
/// root would exceed the 16 384-byte PMTiles header window.
///
/// 2 levels is sufficient for planet-scale at any zoom:
///   z12 world ~500k tiles → ~31 leaves; z15 world ~5M tiles → ~306 leaves.
fn build_directory(tile_entries: &[(u64, u64, u32)]) -> Result<DirectoryParts> {
    let n = tile_entries.len();

    if n <= ENTRIES_PER_LEAF {
        // ── Try flat ─────────────────────────────────────────────────────────
        let entries: Vec<(u64, u64, u32, u32)> = tile_entries
            .iter()
            .map(|&(id, off, len)| (id, off, len, 1))
            .collect();
        let root_compressed = gzip_compress(&encode_directory(&entries))?;
        if root_compressed.len() <= MAX_ROOT_COMPRESSED_BYTES {
            return Ok(DirectoryParts {
                root_compressed,
                leaf_dirs_data: Vec::new(),
                n_tiles: n as u64,
            });
        }
        // Flat root exceeds the 16 384-byte window — fall through to 2-level.
        tracing::debug!(
            n,
            compressed_bytes = root_compressed.len(),
            limit = MAX_ROOT_COMPRESSED_BYTES,
            "flat root directory too large; using leaf directories"
        );
    }

    {
        // ── 2-level ───────────────────────────────────────────────────────────
        let mut leaf_dirs_data: Vec<u8> = Vec::new();
        let mut root_entries: Vec<(u64, u64, u32, u32)> = Vec::new();

        for chunk in tile_entries.chunks(ENTRIES_PER_LEAF) {
            let first_tile_id = chunk[0].0;
            let leaf_offset = leaf_dirs_data.len() as u64;

            let leaf_entries: Vec<(u64, u64, u32, u32)> = chunk
                .iter()
                .map(|&(id, off, len)| (id, off, len, 1))
                .collect();
            let compressed_leaf = gzip_compress(&encode_directory(&leaf_entries))?;
            let leaf_len = compressed_leaf.len() as u32;
            leaf_dirs_data.extend_from_slice(&compressed_leaf);

            // run_length = 0 signals a leaf directory pointer.
            root_entries.push((first_tile_id, leaf_offset, leaf_len, 0));
        }

        let root_compressed = gzip_compress(&encode_directory(&root_entries))?;
        Ok(DirectoryParts {
            root_compressed,
            leaf_dirs_data,
            n_tiles: n as u64,
        })
    }
}

/// Write a PMTiles v3 archive.  Tiles must be sorted by tile_id ascending (clustering).
/// Handles arbitrarily large tile counts via a 2-level directory when needed.
pub(crate) fn write_pmtiles_file_pub(tiles: &[(u64, Vec<u8>)], output_path: &Path, tile_zoom: u8) -> Result<()> {
    write_pmtiles_file(tiles, output_path, tile_zoom)
}

fn write_pmtiles_file(tiles: &[(u64, Vec<u8>)], output_path: &Path, tile_zoom: u8) -> Result<()> {
    // Build tile data section and tile entry list.
    let mut tile_data: Vec<u8> = Vec::new();
    let mut tile_entries: Vec<(u64, u64, u32)> = Vec::with_capacity(tiles.len());

    for (tile_id, payload) in tiles {
        let offset = tile_data.len() as u64;
        tile_entries.push((*tile_id, offset, payload.len() as u32));
        tile_data.extend_from_slice(payload);
    }

    let dir = build_directory(&tile_entries)?;

    // Minimal metadata JSON (no tilejson in v1).
    let metadata = b"{}";

    // Section layout: [header 127B][root_dir][metadata][leaf_dirs][tile_data]
    let root_dir_offset: u64 = 127;
    let root_dir_length = dir.root_compressed.len() as u64;
    let metadata_offset = root_dir_offset + root_dir_length;
    let metadata_length = metadata.len() as u64;
    let leaf_dirs_offset = metadata_offset + metadata_length;
    let leaf_dirs_length = dir.leaf_dirs_data.len() as u64;
    let tile_data_offset = leaf_dirs_offset + leaf_dirs_length;
    let tile_data_length = tile_data.len() as u64;

    // Build 127-byte header.
    let mut hdr = [0u8; 127];
    hdr[0..7].copy_from_slice(b"PMTiles");
    hdr[7] = 3; // spec_version
    hdr[8..16].copy_from_slice(&root_dir_offset.to_le_bytes());
    hdr[16..24].copy_from_slice(&root_dir_length.to_le_bytes());
    hdr[24..32].copy_from_slice(&metadata_offset.to_le_bytes());
    hdr[32..40].copy_from_slice(&metadata_length.to_le_bytes());
    hdr[40..48].copy_from_slice(&leaf_dirs_offset.to_le_bytes());
    hdr[48..56].copy_from_slice(&leaf_dirs_length.to_le_bytes());
    hdr[56..64].copy_from_slice(&tile_data_offset.to_le_bytes());
    hdr[64..72].copy_from_slice(&tile_data_length.to_le_bytes());
    hdr[72..80].copy_from_slice(&dir.n_tiles.to_le_bytes()); // addressed_tiles_count
    hdr[80..88].copy_from_slice(&dir.n_tiles.to_le_bytes()); // tile_entries_count
    hdr[88..96].copy_from_slice(&dir.n_tiles.to_le_bytes()); // tile_contents_count
    hdr[96] = 1;         // clustered
    hdr[97] = 2;         // internal_compression = gzip
    hdr[98] = 1;         // tile_compression = none
    hdr[99] = 0;         // tile_type = unknown/custom
    hdr[100] = tile_zoom; // min_zoom
    hdr[101] = tile_zoom; // max_zoom

    let mut f = std::fs::File::create(output_path)
        .with_context(|| format!("create {}", output_path.display()))?;
    f.write_all(&hdr).context("write header")?;
    f.write_all(&dir.root_compressed).context("write root dir")?;
    f.write_all(metadata).context("write metadata")?;
    f.write_all(&dir.leaf_dirs_data).context("write leaf dirs")?;
    f.write_all(&tile_data).context("write tile data")?;

    Ok(())
}

// ── Manifest ──────────────────────────────────────────────────────────────────

fn write_manifest(
    output_dir: &Path,
    archive_filename: &str,
    release: &str,
    extent_slug: &str,
    tile_zoom: u8,
) -> Result<()> {
    let manifest = serde_json::json!({
        "archive":    archive_filename,
        "release":    release,
        "extent":     extent_slug,
        "tile_zoom":  tile_zoom,
        "built_at":   chrono_now_utc(),
    });
    let path = output_dir.join("manifest.json");
    std::fs::write(&path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn chrono_now_utc() -> String {
    // Minimal ISO 8601 timestamp without pulling in chrono.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Format: YYYY-MM-DDTHH:MM:SSZ (approximate, good enough for a manifest)
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400; // days since 1970-01-01
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut y = 1970u64;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let dy = if leap { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let months = [31u64, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 1u64;
    for mdays in &months {
        if days < *mdays {
            break;
        }
        days -= mdays;
        mo += 1;
    }
    (y, mo, days + 1)
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn write_tiles(
    edges: Vec<QuantizedEdge>,
    nodes: Vec<QuantizedNode>,
    restrictions: Vec<RestrictionTriple>,
    tile_zoom: u8,
    output_dir: &Path,
    release: &str,
    extent_slug: &str,
    low_memory: bool,
) -> Result<()> {
    // Build node coordinate lookup: gers_id → (lon_e7, lat_e7).
    let node_lookup: HashMap<[u8; 16], (i32, i32)> = nodes
        .iter()
        .map(|n| (n.gers_id, (n.lon_e7, n.lat_e7)))
        .collect();

    // Bin edges into tiles by midpoint.
    let mut tile_bins: HashMap<TileKey, Vec<usize>> = HashMap::new();
    for (i, edge) in edges.iter().enumerate() {
        let key = edge_tile_key(edge, tile_zoom);
        tile_bins.entry(key).or_default().push(i);
    }

    info!(
        tiles = tile_bins.len(),
        "binned {} edges into {} tiles at z{}",
        edges.len(),
        tile_bins.len(),
        tile_zoom
    );

    // Determine boundary nodes: any node shared across tile boundaries.
    let mut node_tile_set: HashMap<[u8; 16], u32> = HashMap::new();
    for (tile_key, tile_indices) in &tile_bins {
        let tile_ord = tile_key.x ^ (tile_key.y << 16); // cheap tile identity scalar
        for &idx in tile_indices {
            let e = &edges[idx];
            for gers in [e.start_node_gers, e.end_node_gers] {
                let entry = node_tile_set.entry(gers).or_insert(tile_ord);
                if *entry != tile_ord {
                    *entry = u32::MAX; // sentinel: appears in multiple tiles
                }
            }
        }
    }
    let boundary_nodes: HashSet<[u8; 16]> = node_tile_set
        .into_iter()
        .filter(|(_, v)| *v == u32::MAX)
        .map(|(k, _)| k)
        .collect();

    info!(boundary_nodes = boundary_nodes.len(), "boundary nodes identified");

    // Pre-compute per-tile node orderings (needed for restriction local-index resolution).
    let tile_nodes: HashMap<TileKey, (Vec<[u8; 16]>, HashMap<[u8; 16], u32>)> = tile_bins
        .iter()
        .map(|(key, indices)| (*key, compute_tile_node_order(indices, &edges)))
        .collect();

    // Local edge index within each tile: global_edge_idx → local_seg_idx.
    let global_to_local_seg: HashMap<usize, u32> = tile_bins
        .values()
        .flat_map(|indices| indices.iter().enumerate().map(|(local, &global)| (global, local as u32)))
        .collect();

    // Via-node tile: node_gers → TileKey (routed by node coordinates).
    let node_to_tile: HashMap<[u8; 16], TileKey> = nodes
        .iter()
        .map(|n| {
            let lon = n.lon_e7 as f64 * 1e-7;
            let lat = n.lat_e7 as f64 * 1e-7;
            let (x, y) = lon_lat_to_tile_xy(lon, lat, tile_zoom);
            (n.gers_id, TileKey { z: tile_zoom, x, y })
        })
        .collect();

    // Resolve turn restrictions → per-tile intra/cross lists.
    let mut tile_intra: HashMap<TileKey, Vec<IntraTileRestriction>> = HashMap::new();
    let mut tile_cross: HashMap<TileKey, Vec<CrossTileRestriction>> = HashMap::new();

    // from-edge: the split edge with parent == from_seg that ends at via_connector.
    // to-edge:   the split edge with parent == to_seg   that starts at via_connector.
    let mut from_edge_map: HashMap<([u8; 16], [u8; 16]), usize> = HashMap::new();
    let mut to_edge_map:   HashMap<([u8; 16], [u8; 16]), usize> = HashMap::new();
    for (i, e) in edges.iter().enumerate() {
        from_edge_map.insert((e.parent_gers_id, e.end_node_gers), i);
        to_edge_map.insert(  (e.parent_gers_id, e.start_node_gers), i);
    }

    let mut n_resolved = 0usize;
    let mut n_skipped  = 0usize;
    for r in &restrictions {
        let via_bytes = r.via_connector_gers;
        let via_tile = match node_to_tile.get(&via_bytes) {
            Some(&t) => t,
            None     => { n_skipped += 1; continue; }
        };
        let Some(&from_global) = from_edge_map.get(&(r.from_segment_gers, via_bytes)) else {
            n_skipped += 1; continue;
        };
        let Some(&to_global) = to_edge_map.get(&(r.to_segment_gers, via_bytes)) else {
            n_skipped += 1; continue;
        };
        let via_node_local = match tile_nodes.get(&via_tile).and_then(|(_, idx)| idx.get(&via_bytes)) {
            Some(&i) => i,
            None     => { n_skipped += 1; continue; }
        };

        let from_tile = edge_tile_key(&edges[from_global], tile_zoom);
        let to_tile   = edge_tile_key(&edges[to_global],   tile_zoom);

        if from_tile == via_tile && to_tile == via_tile {
            let from_local = global_to_local_seg[&from_global];
            let to_local   = global_to_local_seg[&to_global];
            tile_intra.entry(via_tile).or_default().push(IntraTileRestriction {
                from_seg: from_local,
                via_node: via_node_local,
                to_seg:   to_local,
                flags:    r.flags,
            });
        } else {
            tile_cross.entry(via_tile).or_default().push(CrossTileRestriction {
                from_gers:      r.from_segment_gers,
                via_node_local,
                to_gers:        r.to_segment_gers,
                flags:          r.flags,
            });
        }
        n_resolved += 1;
    }

    if !restrictions.is_empty() {
        info!(
            total   = restrictions.len(),
            resolved = n_resolved,
            skipped  = n_skipped,
            "turn restrictions resolved"
        );
    }

    // Archive filename: openlrlens-{extent}-{release}.pmtiles
    let safe_release = release.replace('.', "-");
    let archive_filename = format!("openlrlens-{extent_slug}-{safe_release}.pmtiles");
    let archive_path = output_dir.join(&archive_filename);

    if low_memory {
        // ── DuckDB-buffered path ──────────────────────────────────────────────
        // Build payloads one tile at a time, inserting into an in-memory DuckDB
        // table that spills to disk automatically when the memory limit is hit.
        // Then stream back in Hilbert order via StreamingWriter — peak heap usage
        // is O(one tile payload) rather than O(all tile payloads).
        use duckdb::Connection;

        let avail_bytes = crate::partition::available_ram_bytes();
        // Use 40 % of currently available RAM for DuckDB; leave the rest for
        // the edges Vec (already allocated) and OS overhead.
        let limit_mb = ((avail_bytes as f64 * 0.40) / 1_048_576.0) as u64;
        let limit_mb = limit_mb.max(512); // floor at 512 MB

        let conn = Connection::open_in_memory()
            .context("open DuckDB connection")?;
        conn.execute_batch(&format!(
            "SET memory_limit='{limit_mb}MB'; \
             CREATE TABLE tile_buf (tile_id UBIGINT, payload BLOB);"
        ))
        .context("DuckDB setup")?;

        info!(
            tiles = tile_bins.len(),
            duckdb_limit_mb = limit_mb,
            "low-memory: buffering tile payloads via DuckDB"
        );

        {
            let mut stmt = conn
                .prepare("INSERT INTO tile_buf VALUES (?, ?)")
                .context("prepare INSERT")?;
            for (key, indices) in &tile_bins {
                let tile_id = xyz_to_tile_id(key.z, key.x, key.y);
                let (node_order, node_index) = &tile_nodes[key];
                let intra = tile_intra.get(key).map(Vec::as_slice).unwrap_or(&[]);
                let cross = tile_cross.get(key).map(Vec::as_slice).unwrap_or(&[]);
                let payload = build_tile_payload(
                    indices, &edges, node_order, node_index,
                    &node_lookup, &boundary_nodes, intra, cross,
                );
                stmt.execute(duckdb::params![tile_id, payload])
                    .context("INSERT tile")?;
            }
        }

        info!("low-memory: streaming tiles from DuckDB → PMTiles");

        let mut writer = crate::merge::StreamingWriter::new()
            .context("create StreamingWriter")?;
        {
            let mut stmt = conn
                .prepare("SELECT tile_id, payload FROM tile_buf ORDER BY tile_id")
                .context("prepare SELECT")?;
            let mut rows = stmt.query([]).context("query tiles")?;
            while let Some(row) = rows.next().context("next row")? {
                let tile_id: u64 = row.get(0).context("get tile_id")?;
                let payload: Vec<u8> = row.get(1).context("get payload")?;
                writer.add_tile(tile_id, &payload).context("add tile")?;
            }
        }
        writer.finish(&archive_path, tile_zoom).context("finish PMTiles")?;
    } else {
        // ── Default in-memory path ────────────────────────────────────────────
        // Build all payloads into a Vec, sort by Hilbert tile_id, write at once.
        let mut tile_vec: Vec<(u64, Vec<u8>)> = tile_bins
            .iter()
            .map(|(key, indices)| {
                let tile_id = xyz_to_tile_id(key.z, key.x, key.y);
                let (node_order, node_index) = &tile_nodes[key];
                let intra = tile_intra.get(key).map(Vec::as_slice).unwrap_or(&[]);
                let cross = tile_cross.get(key).map(Vec::as_slice).unwrap_or(&[]);
                let payload = build_tile_payload(
                    indices, &edges, node_order, node_index,
                    &node_lookup, &boundary_nodes, intra, cross,
                );
                (tile_id, payload)
            })
            .collect();
        tile_vec.sort_by_key(|(id, _)| *id);
        write_pmtiles_file(&tile_vec, &archive_path, tile_zoom)?;
    }
    info!(path = %archive_path.display(), "PMTiles archive written");

    write_manifest(output_dir, &archive_filename, release, extent_slug, tile_zoom)?;
    info!(path = %output_dir.join("manifest.json").display(), "manifest written");

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tile ID ───────────────────────────────────────────────────────────────

    #[test]
    fn z0_is_id_zero() {
        assert_eq!(xyz_to_tile_id(0, 0, 0), 0);
    }

    #[test]
    fn z1_accumulator() {
        // z=1 tiles start at id 1 (after 1 tile at z=0).
        let acc = ((1u64 << 2) - 1) / 3; // = 1
        // The Hilbert curve for a 2×2 grid visits in a Z shape:
        // (0,0)→0, (0,1)→1, (1,1)→2, (1,0)→3
        assert_eq!(xyz_to_tile_id(1, 0, 0), acc + 0);
        assert_eq!(xyz_to_tile_id(1, 0, 1), acc + 1);
        assert_eq!(xyz_to_tile_id(1, 1, 1), acc + 2);
        assert_eq!(xyz_to_tile_id(1, 1, 0), acc + 3);
    }

    #[test]
    fn z1_tile_ids_are_distinct() {
        let ids: HashSet<u64> = (0..2)
            .flat_map(|x| (0..2).map(move |y| xyz_to_tile_id(1, x, y)))
            .collect();
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn z12_ids_are_distinct_for_nz_bbox() {
        // Spot-check a handful of tiles in the NZ bounding box.
        let coords = [(166.0f64, -47.5f64), (178.5, -34.0), (174.77, -41.28)];
        let ids: HashSet<u64> = coords
            .iter()
            .map(|&(lon, lat)| {
                let (x, y) = lon_lat_to_tile_xy(lon, lat, 12);
                xyz_to_tile_id(12, x, y)
            })
            .collect();
        assert_eq!(ids.len(), 3);
    }

    // ── Slippy tile ───────────────────────────────────────────────────────────

    #[test]
    fn wellington_nz_at_z12() {
        // Wellington: lon≈174.78, lat≈-41.29
        // x = floor((174.78+180)/360 * 4096) = floor(354.78/360 * 4096) ≈ 4037
        // y = floor((1 - ln(tan(π/4 + lat/2))/π)/2 * 4096) ≈ 2572
        let (x, y) = lon_lat_to_tile_xy(174.78, -41.29, 12);
        assert!(x >= 4034 && x <= 4040, "x={x}");
        assert!(y >= 2561 && y <= 2567, "y={y}");
    }

    #[test]
    fn prime_meridian_equator_at_z1() {
        let (x, y) = lon_lat_to_tile_xy(0.0, 0.0, 1);
        assert_eq!(x, 1); // east of the prime meridian
        assert_eq!(y, 1); // south of equator (Mercator y increases northward)
    }

    // ── Pack attrs ────────────────────────────────────────────────────────────

    #[test]
    fn pack_roundtrip() {
        let frc = 3u8;
        let fow = 5u8;
        let packed = pack_attrs(frc, fow, Direction::Forward);
        assert_eq!(packed & 0x07, frc);
        assert_eq!((packed >> 3) & 0x07, fow);
        assert_eq!((packed >> 6) & 0x03, 1); // Forward = 1
    }

    #[test]
    fn pack_frc7_fow7_backward() {
        let packed = pack_attrs(7, 7, Direction::Backward);
        assert_eq!(packed & 0x07, 7);
        assert_eq!((packed >> 3) & 0x07, 7);
        assert_eq!((packed >> 6) & 0x03, 2); // Backward = 2
    }

    // ── Tile payload ──────────────────────────────────────────────────────────

    #[test]
    fn tile_payload_header_magic_and_counts() {
        use openlr_graph::tile::TILE_MAGIC;
        let edges = vec![
            QuantizedEdge {
                start_node_gers: [1u8; 16],
                end_node_gers:   [2u8; 16],
                geometry:        vec![(1_747_700_000i32, -366_000_000), (1_748_000_000, -366_000_000)],
                length_cm:       10_000,
                frc: 3,
                fow: 3,
                direction: Direction::Both,
                parent_gers_id: [0u8; 16],
            },
        ];
        let node_lookup: HashMap<[u8; 16], (i32, i32)> = HashMap::from([
            ([1u8; 16], (1_747_700_000, -366_000_000)),
            ([2u8; 16], (1_748_000_000, -366_000_000)),
        ]);
        let boundary = HashSet::new();
        let (node_order, node_index) = compute_tile_node_order(&[0], &edges);
        let payload = build_tile_payload(
            &[0], &edges, &node_order, &node_index, &node_lookup, &boundary, &[], &[],
        );

        // Magic
        assert_eq!(&payload[0..4], &TILE_MAGIC);
        // Version
        assert_eq!(payload[4], 2);
        // segment_count at bytes 8–11
        let seg_count = u32::from_le_bytes(payload[8..12].try_into().unwrap());
        assert_eq!(seg_count, 1);
        // node_count at bytes 12–15
        let node_count = u32::from_le_bytes(payload[12..16].try_into().unwrap());
        assert_eq!(node_count, 2);
        // geom_vertex_count at bytes 20–23
        let geom_vc = u32::from_le_bytes(payload[20..24].try_into().unwrap());
        assert_eq!(geom_vc, 2);
    }

    #[test]
    fn tile_payload_size_formula() {
        // 1 edge, 3 vertices, 2 nodes → header(40) + seg(32) + seg_gers(16) + geom(3×8) + node(2×28)
        let edges = vec![QuantizedEdge {
            start_node_gers: [1u8; 16],
            end_node_gers:   [2u8; 16],
            geometry:        vec![(0i32, 0), (1, 0), (2, 0)],
            length_cm:       100,
            frc: 2, fow: 3,
            direction: Direction::Both,
            parent_gers_id: [0u8; 16],
        }];
        let node_lookup = HashMap::from([
            ([1u8; 16], (0i32, 0i32)),
            ([2u8; 16], (2i32, 0i32)),
        ]);
        let boundary = HashSet::new();
        let (node_order, node_index) = compute_tile_node_order(&[0], &edges);
        let payload = build_tile_payload(
            &[0], &edges, &node_order, &node_index, &node_lookup, &boundary, &[], &[],
        );
        let expected = 40 + 32 + 16 + 3 * 8 + 2 * 28;
        assert_eq!(payload.len(), expected, "payload len {}", payload.len());
    }

    // ── PMTiles writer ────────────────────────────────────────────────────────

    #[test]
    fn pmtiles_file_header_magic() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let tiles = vec![(0u64, vec![0u8; 10])];
        write_pmtiles_file(&tiles, tmp.path(), 12).unwrap();

        let bytes = std::fs::read(tmp.path()).unwrap();
        assert!(bytes.len() >= 127, "file too short");
        assert_eq!(&bytes[0..7], b"PMTiles");
        assert_eq!(bytes[7], 3); // version
        assert_eq!(bytes[96], 1); // clustered
        assert_eq!(bytes[97], 2); // internal_compression = gzip
        assert_eq!(bytes[98], 1); // tile_compression = none
    }

    #[test]
    fn encode_directory_first_entry_absolute() {
        // (tile_id, offset, length, run_length)
        let entries = vec![(5u64, 0u64, 100u32, 1u32)];
        let encoded = encode_directory(&entries);
        // n_entries(1) + id_delta(1) + run_length(1) + length(1) + offset(0+1=1→1) = 5
        assert_eq!(encoded.len(), 5);
        assert_eq!(encoded[4], 1); // offset = 0+1 = 1
    }

    #[test]
    fn encode_directory_sequential_offset_is_zero() {
        let entries = vec![
            (0u64, 0u64, 50u32, 1u32),
            (1u64, 50u64, 50u32, 1u32), // immediately follows
        ];
        let encoded = encode_directory(&entries);
        // n(1) + id_deltas[0,1](1+1) + run_lengths[1,1](1+1) + lengths[50,50](1+1) + offsets[1,0](1+1) = 9
        assert_eq!(encoded.len(), 9);
        assert_eq!(encoded[8], 0); // second entry: sequential → 0
    }

    #[test]
    fn encode_directory_leaf_pointer_has_run_length_zero() {
        // Leaf pointer: run_length = 0.
        let entries = vec![(42u64, 0u64, 256u32, 0u32)];
        let encoded = encode_directory(&entries);
        // n(1) + id=42(1) + run_length=0(1) + length=256(2: >127) + offset=1(1) = 6
        assert!(encoded.len() >= 5);
        // run_length section is the 3rd varint section; for 1 entry it's encoded[2].
        assert_eq!(encoded[2], 0); // run_length = 0 for leaf pointer
    }

    #[test]
    fn build_directory_flat_for_small_count() {
        let entries: Vec<(u64, u64, u32)> =
            (0..3u64).map(|i| (i, i * 100, 100)).collect();
        let dir = build_directory(&entries).unwrap();
        assert!(dir.leaf_dirs_data.is_empty(), "should be flat");
        assert_eq!(dir.n_tiles, 3);
    }

    #[test]
    fn build_directory_two_level_above_threshold() {
        // ENTRIES_PER_LEAF + 1 tiles forces leaf directories.
        let n = ENTRIES_PER_LEAF + 1;
        let entries: Vec<(u64, u64, u32)> =
            (0..n as u64).map(|i| (i, i * 10, 10)).collect();
        let dir = build_directory(&entries).unwrap();
        assert!(!dir.leaf_dirs_data.is_empty(), "should have leaf dirs");
        assert_eq!(dir.n_tiles, n as u64);
    }

    // ── ISO 8601 timestamp ────────────────────────────────────────────────────

    #[test]
    fn timestamp_format_looks_right() {
        let ts = chrono_now_utc();
        assert_eq!(ts.len(), 20); // "YYYY-MM-DDTHH:MM:SSZ"
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[19..20], "Z");
    }
}
