/// DuckDB-backed low-memory OSM PBF → PMTiles pipeline.
///
/// Invoked by `build::run_osm` when `--low-memory` is set.  The entire pipeline
/// is driven through a DuckDB scratch database so no large Vec/HashMap structures
/// accumulate in the Rust heap.  Peak Rust heap per stage is O(one batch) rather
/// than O(all data).
///
/// Stages and their DuckDB tables:
///   Pass 1 (PBF ways+relations) → ways, node_ref_deltas, restrictions_raw
///   Derived                      → intersection_nodes, unique_refs
///   Pass 2 (PBF nodes)          → node_coords
///   Bbox filter                  → prunes ways, node_coords in-place
///   Adapt+split+quantize         → q_edges, q_nodes, restriction_triples
///   Tile                         → PMTiles via StreamingWriter

use std::collections::{HashMap, HashSet};
use std::fmt::Write as FmtWrite;
use std::path::Path;

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use indicatif::{ProgressBar, ProgressStyle};
use osmpbf::{Element, ElementReader, RelMemberType};
use tracing::{info, warn};

use crate::extent::Bbox;
use crate::merge::StreamingWriter;
use crate::osm_adapt::{encode_node_id, encode_way_id};
use crate::osm_schema::OsmSchemaMapping;
use crate::partition::available_ram_bytes;
use crate::quantize::quantize_coord;
use crate::restrictions::{encode_restriction_flags, HEADING_ANY};
use crate::split::polyline_length_m;
use crate::tile::{lon_lat_to_tile_xy, xyz_to_tile_id};

// ── Batch sizes ───────────────────────────────────────────────────────────────

/// Ways flushed to DuckDB per batch during Pass 1.
const WAY_BATCH: usize = 5_000;
/// Node-coord rows batched before DuckDB flush during Pass 2.
const NODE_BATCH: usize = 200_000;
/// Ways processed per adapt+split+quantize iteration.
const ADAPT_BATCH: usize = 1_000;

// ── Internal structs ──────────────────────────────────────────────────────────

struct WayRecord {
    id: i64,
    frc: u8,
    fow: u8,
    direction: u8, // 0=Both 1=Forward 2=Backward
    node_ids: Vec<u8>, // LE i64 blob — call blob_to_node_ids to decode
}

/// Edge data fetched from DuckDB for tile payload building.
struct LmEdge {
    edge_idx: u32,
    start_gers: [u8; 16],
    end_gers: [u8; 16],
    parent_gers: [u8; 16],
    geom: Vec<(i32, i32)>,
    length_cm: u32,
    frc: u8,
    fow: u8,
    direction: u8,
}

struct LmIntraTile {
    from_seg: u32,
    via_node: u32,
    to_seg: u32,
    flags: u8,
}

struct LmCrossTile {
    from_gers: [u8; 16],
    via_node_local: u32,
    to_gers: [u8; 16],
    flags: u8,
}

struct ResolvedRestriction {
    from_gers: [u8; 16],
    via_gers: [u8; 16],
    to_gers: [u8; 16],
    flags: u8,
    from_edge_idx: u32,
    to_edge_idx: u32,
    via_tile_x: u32,
    via_tile_y: u32,
}

// ── BLOB ↔ Rust helpers ───────────────────────────────────────────────────────

pub(crate) fn geom_to_blob(geom: &[(i32, i32)]) -> Vec<u8> {
    let mut b = Vec::with_capacity(geom.len() * 8);
    for (x, y) in geom {
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
    }
    b
}

fn blob_to_geom(blob: &[u8]) -> Vec<(i32, i32)> {
    blob.chunks_exact(8)
        .map(|c| {
            let x = i32::from_le_bytes(c[0..4].try_into().unwrap());
            let y = i32::from_le_bytes(c[4..8].try_into().unwrap());
            (x, y)
        })
        .collect()
}

fn node_ids_to_blob(ids: &[i64]) -> Vec<u8> {
    let mut b = Vec::with_capacity(ids.len() * 8);
    for id in ids {
        b.extend_from_slice(&id.to_le_bytes());
    }
    b
}

fn blob_to_node_ids(blob: &[u8]) -> Vec<i64> {
    blob.chunks_exact(8)
        .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

fn blob_to_gers(blob: &[u8]) -> [u8; 16] {
    blob.try_into().expect("GERS blob must be 16 bytes")
}

// ── Tile payload building (mirrors tile.rs; kept here to avoid cross-module coupling) ─

fn pack_attrs_lm(frc: u8, fow: u8, direction: u8) -> u8 {
    (frc & 0x07) | ((fow & 0x07) << 3) | ((direction & 0x03) << 6)
}

fn compute_tile_nodes_lm(edges: &[LmEdge]) -> (Vec<[u8; 16]>, HashMap<[u8; 16], u32>) {
    let mut order: Vec<[u8; 16]> = Vec::new();
    let mut index: HashMap<[u8; 16], u32> = HashMap::new();
    for e in edges {
        for &gers in &[e.start_gers, e.end_gers] {
            if !index.contains_key(&gers) {
                let i = order.len() as u32;
                order.push(gers);
                index.insert(gers, i);
            }
        }
    }
    (order, index)
}

fn build_lm_tile_payload(
    edges: &[LmEdge],
    node_order: &[[u8; 16]],
    node_index: &HashMap<[u8; 16], u32>,
    node_lookup: &HashMap<[u8; 16], (i32, i32)>,
    boundary_nodes: &HashSet<[u8; 16]>,
    intra: &[LmIntraTile],
    cross: &[LmCrossTile],
) -> Vec<u8> {
    let segment_count    = edges.len() as u32;
    let node_count       = node_order.len() as u32;
    let restriction_count  = intra.len() as u32;
    let xrestriction_count = cross.len() as u32;

    let mut geom_pool:     Vec<(i32, i32)> = Vec::new();
    let mut seg_records:   Vec<[u8; 32]>   = Vec::with_capacity(edges.len());
    let mut seg_gers_ids:  Vec<[u8; 16]>   = Vec::with_capacity(edges.len());

    for e in edges {
        let geom_offset = geom_pool.len() as u32;
        let geom_len    = e.geom.len() as u16;
        geom_pool.extend_from_slice(&e.geom);

        let start_node = node_index[&e.start_gers];
        let end_node   = node_index[&e.end_gers];
        let packed     = pack_attrs_lm(e.frc, e.fow, e.direction);

        let mut r = [0u8; 32];
        r[0..4].copy_from_slice(&start_node.to_le_bytes());
        r[4..8].copy_from_slice(&end_node.to_le_bytes());
        r[8..12].copy_from_slice(&geom_offset.to_le_bytes());
        r[12..14].copy_from_slice(&geom_len.to_le_bytes());
        r[14..18].copy_from_slice(&e.length_cm.to_le_bytes());
        r[18] = packed;
        seg_records.push(r);
        seg_gers_ids.push(e.parent_gers);
    }

    let geom_vertex_count = geom_pool.len() as u32;

    let mut hdr = [0u8; 40];
    hdr[0..4].copy_from_slice(b"OLRL");
    hdr[4] = 2; // version 2: stable-id table present
    hdr[8..12].copy_from_slice(&segment_count.to_le_bytes());
    hdr[12..16].copy_from_slice(&node_count.to_le_bytes());
    hdr[16..20].copy_from_slice(&restriction_count.to_le_bytes());
    hdr[20..24].copy_from_slice(&geom_vertex_count.to_le_bytes());
    hdr[24..28].copy_from_slice(&xrestriction_count.to_le_bytes());

    let cap = 40
        + seg_records.len() * 32
        + seg_gers_ids.len() * 16
        + geom_pool.len() * 8
        + node_order.len() * 28
        + intra.len() * 16
        + cross.len() * 40;
    let mut payload = Vec::with_capacity(cap);

    payload.extend_from_slice(&hdr);
    for r in &seg_records   { payload.extend_from_slice(r); }
    for g in &seg_gers_ids  { payload.extend_from_slice(g.as_slice()); }
    for (lon_e7, lat_e7) in &geom_pool {
        payload.extend_from_slice(&lon_e7.to_le_bytes());
        payload.extend_from_slice(&lat_e7.to_le_bytes());
    }
    for gers in node_order {
        let (lon_e7, lat_e7) = node_lookup.get(gers).copied().unwrap_or_else(|| {
            warn!(gers = %hex::encode(gers), "node not found in lookup");
            (0, 0)
        });
        let is_boundary = boundary_nodes.contains(gers);
        payload.extend_from_slice(&lon_e7.to_le_bytes());
        payload.extend_from_slice(&lat_e7.to_le_bytes());
        payload.extend_from_slice(gers.as_slice());
        payload.push(u8::from(is_boundary));
        payload.extend_from_slice(&[0u8; 3]);
    }
    for r in intra {
        payload.extend_from_slice(&r.from_seg.to_le_bytes());
        payload.extend_from_slice(&r.via_node.to_le_bytes());
        payload.extend_from_slice(&r.to_seg.to_le_bytes());
        payload.push(r.flags);
        payload.extend_from_slice(&[0u8; 3]);
    }
    for r in cross {
        payload.extend_from_slice(&r.from_gers);
        payload.extend_from_slice(&r.via_node_local.to_le_bytes());
        payload.extend_from_slice(&r.to_gers);
        payload.push(r.flags);
        payload.extend_from_slice(&[0u8; 3]);
    }
    payload
}

// ── Progress bar helpers ──────────────────────────────────────────────────────

pub(crate) fn make_spinner(show: bool, msg: &'static str) -> ProgressBar {
    if !show { return ProgressBar::hidden(); }
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg} [{elapsed_precise}] {human_pos}")
            .expect("valid template"),
    );
    pb.set_message(msg);
    pb
}

pub(crate) fn make_bar(show: bool, total: u64, msg: &'static str) -> ProgressBar {
    if !show { return ProgressBar::hidden(); }
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{msg:32} [{bar:40.cyan/blue}] {human_pos}/{human_len}  eta {eta}")
            .expect("valid template")
            .progress_chars("█▉▊▋▌▍▎▏ "),
    );
    pb.set_message(msg);
    pb
}

// ── DuckDB setup ──────────────────────────────────────────────────────────────

fn setup_duckdb(memory_mb_override: Option<u64>) -> Result<Connection> {
    let limit_mb = match memory_mb_override {
        Some(mb) => mb,
        None => {
            let avail = available_ram_bytes();
            // Default: 40 % of currently available RAM, floor 1 GB.
            let mb = ((avail as f64 * 0.40) / 1_048_576.0) as u64;
            mb.max(1_024)
        }
    };

    let conn = Connection::open_in_memory().context("open DuckDB")?;
    conn.execute_batch(&format!(
        "PRAGMA threads={threads}; \
         SET memory_limit='{limit_mb}MB'; \
         CREATE TABLE ways (id BIGINT, frc INTEGER, fow INTEGER, direction INTEGER, node_ids BLOB); \
         CREATE TABLE restrictions_raw (from_way_id BIGINT, via_node_id BIGINT, to_way_id BIGINT); \
         CREATE TABLE node_ref_deltas (node_id BIGINT, delta INTEGER); \
         CREATE TABLE node_coords (id BIGINT, lon DOUBLE, lat DOUBLE); \
         CREATE TABLE q_edges ( \
             edge_idx INTEGER, \
             start_gers BLOB, end_gers BLOB, parent_gers BLOB, \
             geom_blob BLOB, length_cm INTEGER, \
             frc INTEGER, fow INTEGER, direction INTEGER, \
             tile_x INTEGER, tile_y INTEGER, tile_id BIGINT); \
         CREATE TABLE q_nodes (gers_id BLOB, lon_e7 INTEGER, lat_e7 INTEGER, tile_x INTEGER, tile_y INTEGER); \
         CREATE TABLE restriction_triples (from_gers BLOB, via_gers BLOB, to_gers BLOB, flags INTEGER);",
        threads = rayon::current_num_threads().min(8),
    ))
    .context("DuckDB schema")?;
    info!(limit_mb, "DuckDB scratch database ready");
    Ok(conn)
}

// ── Phase 1: Extract ways and relations ──────────────────────────────────────

fn flush_way_batch(
    conn: &Connection,
    ways: &mut Vec<WayRecord>,
    deltas: &mut Vec<(i64, i64)>,
) -> Result<()> {
    if ways.is_empty() { return Ok(()); }

    // Ways: prepared statement inside a transaction.
    conn.execute_batch("BEGIN").context("BEGIN ways")?;
    let result: Result<()> = (|| {
        let mut stmt = conn
            .prepare("INSERT INTO ways VALUES (?, ?, ?, ?, ?)")
            .context("prepare INSERT ways")?;
        for w in ways.iter() {
            stmt.execute(params![w.id, w.frc as i64, w.fow as i64, w.direction as i64, &w.node_ids])
                .context("INSERT way")?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK");
        return result;
    }
    conn.execute_batch("COMMIT").context("COMMIT ways")?;
    ways.clear();

    // Node-ref deltas: bulk VALUES string (no BLOBs, just integers — safe).
    if !deltas.is_empty() {
        let mut sql = String::with_capacity(deltas.len() * 18);
        sql.push_str("INSERT INTO node_ref_deltas VALUES ");
        for (i, (nid, d)) in deltas.iter().enumerate() {
            if i > 0 { sql.push(','); }
            write!(sql, "({},{})", nid, d).unwrap();
        }
        conn.execute_batch(&sql).context("INSERT node_ref_deltas")?;
        deltas.clear();
    }
    Ok(())
}

fn extract_pass1(pbf_path: &Path, schema: &OsmSchemaMapping, conn: &Connection, show_progress: bool) -> Result<usize> {
    let pb = make_spinner(show_progress, "Pass 1/2  scanning ways ");
    let mut ways_scanned: u64 = 0;

    let reader = ElementReader::from_path(pbf_path)?;

    let mut way_batch: Vec<WayRecord>        = Vec::with_capacity(WAY_BATCH + 64);
    let mut delta_batch: Vec<(i64, i64)>     = Vec::with_capacity(WAY_BATCH * 12);
    let mut restriction_batch: Vec<(i64, i64, i64)> = Vec::with_capacity(8_192);
    let mut err: Option<anyhow::Error>        = None;

    reader.for_each(|el| {
        if err.is_some() { return; }
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
                        "dual_carriageway" => { if val == "yes" { dual_carriageway = true; } }
                        other => {
                            if let Some(excl_vals) = schema.exclusions.get(other) {
                                if excl_vals.iter().any(|ev| ev == val) { excluded = true; }
                            }
                        }
                    }
                }
                if excluded { return; }
                let hw = match highway { Some(h) => h, None => return };
                let (frc, base_fow, is_vehicular) = match schema.lookup(hw) {
                    Some(a) => a, None => return,
                };
                if !is_vehicular { return; }

                let fow = if is_roundabout { 4 } else if dual_carriageway { 2 } else { base_fow };
                let direction: u8 = if is_roundabout {
                    1
                } else {
                    match oneway { 1 => 1, -1 => 2, _ => 0 }
                };

                let node_ids: Vec<i64> = w.refs().collect();
                if node_ids.len() < 2 { return; }

                // Node-ref deltas: endpoints get delta=2, interior get delta=1.
                let last = node_ids.len() - 1;
                for (i, &nid) in node_ids.iter().enumerate() {
                    let delta: i64 = if i == 0 || i == last { 2 } else { 1 };
                    delta_batch.push((nid, delta));
                }

                way_batch.push(WayRecord { id: w.id(), frc, fow, direction, node_ids: node_ids_to_blob(&node_ids) });
                ways_scanned += 1;
                if ways_scanned % (WAY_BATCH as u64) == 0 {
                    pb.set_position(ways_scanned);
                }

                if way_batch.len() >= WAY_BATCH {
                    if let Err(e) = flush_way_batch(conn, &mut way_batch, &mut delta_batch) {
                        err = Some(e);
                    }
                }
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
                if !is_restriction || !is_no_turn { return; }

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
                    restriction_batch.push((f, v, t));
                }
            }
            _ => {}
        }
    })?;

    if let Some(e) = err { return Err(e); }

    // Flush remaining ways and deltas.
    flush_way_batch(conn, &mut way_batch, &mut delta_batch)?;

    // Insert restrictions (small, use prepared statement).
    if !restriction_batch.is_empty() {
        conn.execute_batch("BEGIN").context("BEGIN restrictions")?;
        let res: Result<()> = (|| {
            let mut stmt = conn
                .prepare("INSERT INTO restrictions_raw VALUES (?, ?, ?)")
                .context("prepare INSERT restrictions_raw")?;
            for (f, v, t) in &restriction_batch {
                stmt.execute(params![f, v, t]).context("INSERT restriction")?;
            }
            Ok(())
        })();
        if let Err(e) = res {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
        conn.execute_batch("COMMIT").context("COMMIT restrictions")?;
    }

    let way_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ways", [], |r| r.get(0))
        .context("count ways")?;
    let delta_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM node_ref_deltas", [], |r| r.get(0))
        .context("count deltas")?;
    let restr_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM restrictions_raw", [], |r| r.get(0))
        .context("count restrictions")?;
    pb.finish_and_clear();
    info!(ways = way_count, node_ref_deltas = delta_count, restrictions = restr_count,
          "Pass 1 complete");
    Ok(way_count as usize)
}

// ── Derived tables ────────────────────────────────────────────────────────────

fn compute_derived_tables(conn: &Connection) -> Result<usize> {
    conn.execute_batch(
        "CREATE TABLE intersection_nodes AS \
             SELECT node_id FROM node_ref_deltas GROUP BY node_id HAVING SUM(delta) >= 2; \
         CREATE TABLE unique_refs AS \
             SELECT DISTINCT node_id FROM node_ref_deltas; \
         CREATE INDEX idx_unique_refs ON unique_refs(node_id); \
         CREATE INDEX idx_intersection ON intersection_nodes(node_id);"
    )
    .context("compute derived tables")?;

    let ix_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM intersection_nodes", [], |r| r.get(0))
        .context("count intersection_nodes")?;
    let ref_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM unique_refs", [], |r| r.get(0))
        .context("count unique_refs")?;
    info!(intersection_nodes = ix_count, referenced_nodes = ref_count, "derived tables ready");
    Ok(ref_count as usize)
}

// ── Phase 2: Extract node coordinates ─────────────────────────────────────────

fn extract_pass2(pbf_path: &Path, conn: &Connection, show_progress: bool) -> Result<usize> {
    let pb = make_spinner(show_progress, "Pass 2/2  scanning nodes");
    let mut nodes_scanned: u64 = 0;
    // Staging table for batch semi-joins.
    conn.execute_batch(
        "CREATE TEMP TABLE _node_staging (id BIGINT, lon DOUBLE, lat DOUBLE)"
    )
    .context("create _node_staging")?;

    let mut batch: Vec<(i64, f64, f64)> = Vec::with_capacity(NODE_BATCH);
    let mut err: Option<anyhow::Error> = None;

    let reader = ElementReader::from_path(pbf_path)?;
    reader.for_each(|el| {
        if err.is_some() { return; }
        let (id, lon, lat) = match el {
            Element::Node(n)      => (n.id(), n.lon(), n.lat()),
            Element::DenseNode(n) => (n.id(), n.lon(), n.lat()),
            _ => return,
        };
        batch.push((id, lon, lat));
        nodes_scanned += 1;
        if nodes_scanned % (NODE_BATCH as u64) == 0 {
            pb.set_position(nodes_scanned);
        }
        if batch.len() >= NODE_BATCH {
            if let Err(e) = flush_node_batch(conn, &mut batch) {
                err = Some(e);
            }
        }
    })?;
    if let Some(e) = err { return Err(e); }
    flush_node_batch(conn, &mut batch)?;

    conn.execute_batch("DROP TABLE _node_staging").context("drop staging")?;

    let stored: i64 = conn
        .query_row("SELECT COUNT(*) FROM node_coords", [], |r| r.get(0))
        .context("count node_coords")?;
    pb.finish_and_clear();
    info!(nodes_loaded = stored, "Pass 2 complete");
    Ok(stored as usize)
}

fn flush_node_batch(conn: &Connection, batch: &mut Vec<(i64, f64, f64)>) -> Result<usize> {
    if batch.is_empty() { return Ok(0); }

    // Bulk-insert into staging.
    let mut sql = String::with_capacity(batch.len() * 40);
    sql.push_str("INSERT INTO _node_staging VALUES ");
    for (i, (id, lon, lat)) in batch.iter().enumerate() {
        if i > 0 { sql.push(','); }
        write!(sql, "({},{},{})", id, lon, lat).unwrap();
    }
    conn.execute_batch(&sql).context("INSERT _node_staging")?;

    // Semi-join: only keep nodes referenced by road ways.
    conn.execute_batch(
        "INSERT INTO node_coords \
         SELECT s.id, s.lon, s.lat FROM _node_staging s \
         WHERE s.id IN (SELECT node_id FROM unique_refs); \
         DELETE FROM _node_staging;"
    )
    .context("node_coords semi-join flush")?;

    let n = batch.len();
    batch.clear();
    Ok(n)
}

// ── Bbox filter ───────────────────────────────────────────────────────────────

fn apply_bbox_filter(bbox: Bbox, conn: &Connection) -> Result<()> {
    // 1. Find node IDs inside the bbox.
    let bbox_nodes: HashSet<i64> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM node_coords WHERE lon >= ? AND lon <= ? AND lat >= ? AND lat <= ?",
        )?;
        stmt.query_map(
            params![bbox.west, bbox.east, bbox.south, bbox.north],
            |r| r.get(0),
        )?
        .collect::<duckdb::Result<HashSet<i64>>>()
        .context("collect bbox nodes")?
    };

    if bbox_nodes.is_empty() {
        warn!("bbox filter removed all nodes — bounding box may be incorrect");
        return Ok(());
    }

    // 2. Find way IDs that have at least one node inside the bbox.
    let mut keep_ways: HashSet<i64> = HashSet::new();
    {
        let mut stmt = conn.prepare("SELECT id, node_ids FROM ways")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let way_id: i64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            let node_ids = blob_to_node_ids(&blob);
            if node_ids.iter().any(|nid| bbox_nodes.contains(nid)) {
                keep_ways.insert(way_id);
            }
        }
    }

    // 3. Delete ways outside bbox.
    if keep_ways.len() < {
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM ways", [], |r| r.get(0))?;
        total as usize
    } {
        let drop_ids: Vec<i64> = {
            let mut stmt = conn.prepare("SELECT id FROM ways")?;
            let all: Vec<i64> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<duckdb::Result<Vec<i64>>>()?;
            all.into_iter().filter(|id| !keep_ways.contains(id)).collect()
        };
        if !drop_ids.is_empty() {
            // Build a bulk-delete statement.
            let ids_str = drop_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",");
            conn.execute_batch(&format!("DELETE FROM ways WHERE id IN ({})", ids_str))
                .context("DELETE ways outside bbox")?;
        }
    }

    // 4. Prune node_coords that are far outside the bbox (add a 1-degree margin to
    //    retain nodes of roads that straddle the boundary).
    conn.execute(
        "DELETE FROM node_coords WHERE lon < ? OR lon > ? OR lat < ? OR lat > ?",
        params![bbox.west - 1.0, bbox.east + 1.0, bbox.south - 1.0, bbox.north + 1.0],
    )
    .context("prune node_coords")?;

    let ways_left: i64 = conn.query_row("SELECT COUNT(*) FROM ways", [], |r| r.get(0))?;
    let nodes_left: i64 = conn.query_row("SELECT COUNT(*) FROM node_coords", [], |r| r.get(0))?;
    info!(ways = ways_left, nodes = nodes_left, "after bbox filter");
    Ok(())
}

// ── Phase 3: Adapt + split + quantize ─────────────────────────────────────────

pub(crate) fn adapt_split_quantize(conn: &Connection, tile_zoom: u8, show_progress: bool) -> Result<usize> {
    // Build indexes for fast coord and intersection lookups.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_nc ON node_coords(id); \
         CREATE INDEX IF NOT EXISTS idx_ic ON intersection_nodes(node_id);"
    )
    .context("create adapt indexes")?;

    let way_count: i64 = conn.query_row("SELECT COUNT(*) FROM ways", [], |r| r.get(0))?;
    let pb = make_bar(show_progress, way_count as u64, "Adapt/split/quantize");
    let mut edge_idx: u32 = 0;
    let mut offset: i64  = 0;

    // Track unique nodes — deduplicate across batches via a local HashSet to avoid
    // duplicate inserts (DuckDB has no INSERT OR IGNORE / ON CONFLICT DO NOTHING).
    let mut seen_nodes: HashSet<[u8; 16]> = HashSet::new();

    while offset < way_count {
        let batch = fetch_ways_batch(conn, offset, ADAPT_BATCH as i64)?;
        if batch.is_empty() { break; }

        // Collect all node IDs referenced by this batch.
        let all_node_ids: Vec<i64> = batch
            .iter()
            .flat_map(|w| blob_to_node_ids(&w.node_ids))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        // Fetch coords and intersection flags for this node set.
        let node_coords  = fetch_node_coords(conn, &all_node_ids)?;
        let ix_nodes     = fetch_intersection_nodes(conn, &all_node_ids)?;

        // Process each way.
        for way_row in &batch {
            let way_id    = way_row.id;
            let node_ids  = blob_to_node_ids(&way_row.node_ids);
            let frc       = way_row.frc as u8;
            let fow       = way_row.fow as u8;
            let direction = way_row.direction as u8;

            if node_ids.len() < 2 { continue; }

            let parent_gers = encode_way_id(way_id);
            let last = node_ids.len() - 1;

            // Determine split points: index 0, any interior intersection node, last.
            let mut split_starts: Vec<usize> = vec![0];
            for (i, &nid) in node_ids[1..last].iter().enumerate() {
                if ix_nodes.contains(&nid) {
                    split_starts.push(i + 1);
                }
            }

            for (k, &start_idx) in split_starts.iter().enumerate() {
                let end_idx = if k + 1 < split_starts.len() { split_starts[k + 1] } else { last };

                // Build geometry.
                let mut geom_f64: Vec<(f64, f64)> = Vec::with_capacity(end_idx - start_idx + 1);
                let mut ok = true;
                for &nid in &node_ids[start_idx..=end_idx] {
                    if let Some(&(lon, lat)) = node_coords.get(&nid) {
                        geom_f64.push((lon, lat));
                    } else {
                        warn!(way = way_id, node = nid, "missing coords, sub-edge skipped");
                        ok = false;
                        break;
                    }
                }
                if !ok || geom_f64.len() < 2 { continue; }

                let start_nid  = node_ids[start_idx];
                let end_nid    = node_ids[end_idx];
                let start_gers = encode_node_id(start_nid);
                let end_gers   = encode_node_id(end_nid);
                let length_m   = polyline_length_m(&geom_f64);
                let length_cm  = (length_m * 100.0).round() as u32;

                // Quantize geometry and remove collinear vertices.
                let raw_q: Vec<(i32, i32)> = geom_f64
                    .iter()
                    .map(|&(lon, lat)| (quantize_coord(lon), quantize_coord(lat)))
                    .collect();
                let geom_q = remove_collinear_lm(raw_q);

                // Tile key from midpoint.
                let mid = geom_q[geom_q.len() / 2];
                let (tile_x, tile_y) = lon_lat_to_tile_xy(
                    mid.0 as f64 * 1e-7, mid.1 as f64 * 1e-7, tile_zoom,
                );
                let tile_id = xyz_to_tile_id(tile_zoom, tile_x, tile_y);

                let geom_blob = geom_to_blob(&geom_q);

                conn.execute(
                    "INSERT INTO q_edges VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
                    params![
                        edge_idx as i64,
                        start_gers.as_slice(), end_gers.as_slice(), parent_gers.as_slice(),
                        geom_blob,
                        length_cm as i64,
                        frc as i64, fow as i64, direction as i64,
                        tile_x as i64, tile_y as i64, tile_id as i64
                    ],
                )
                .context("INSERT q_edge")?;
                edge_idx += 1;

                // Insert start and end nodes (deduplicated).
                for (ngers, (nlon, nlat)) in [
                    (start_gers, (geom_f64[0].0, geom_f64[0].1)),
                    (end_gers,   (*geom_f64.last().unwrap())),
                ] {
                    if !seen_nodes.contains(&ngers) {
                        seen_nodes.insert(ngers);
                        let lon_e7 = quantize_coord(nlon);
                        let lat_e7 = quantize_coord(nlat);
                        let (ntx, nty) = lon_lat_to_tile_xy(nlon, nlat, tile_zoom);
                        conn.execute(
                            "INSERT INTO q_nodes VALUES (?,?,?,?,?)",
                            params![
                                ngers.as_slice(),
                                lon_e7, lat_e7,
                                ntx as i64, nty as i64
                            ],
                        )
                        .context("INSERT q_node")?;
                    }
                }
            }
        }

        let batch_len = batch.len() as i64;
        offset += batch_len;
        pb.inc(batch_len as u64);
        if offset % 50_000 == 0 {
            info!(progress = offset, way_count, edges = edge_idx, "adapt+split progress");
        }
    }

    // Resolve restrictions from OSM.
    let mut stmt = conn.prepare("SELECT from_way_id, via_node_id, to_way_id FROM restrictions_raw")?;
    let mut rows = stmt.query([])?;
    let mut restr_stmt = conn.prepare("INSERT INTO restriction_triples VALUES (?,?,?,?)")?;
    let mut restr_count = 0usize;
    while let Some(row) = rows.next()? {
        let from_way: i64 = row.get(0)?;
        let via_node: i64 = row.get(1)?;
        let to_way:   i64 = row.get(2)?;
        let from_gers = encode_way_id(from_way);
        let via_gers  = encode_node_id(via_node);
        let to_gers   = encode_way_id(to_way);
        let flags     = encode_restriction_flags(HEADING_ANY, HEADING_ANY);
        restr_stmt.execute(params![
            from_gers.as_slice(), via_gers.as_slice(), to_gers.as_slice(), flags as i64
        ])?;
        restr_count += 1;
    }
    drop(restr_stmt);
    drop(stmt);

    // Indexes for the tile stage.
    conn.execute_batch(
        "CREATE INDEX idx_q_edges_tile ON q_edges(tile_x, tile_y); \
         CREATE INDEX idx_q_edges_from ON q_edges(parent_gers, end_gers); \
         CREATE INDEX idx_q_edges_to   ON q_edges(parent_gers, start_gers); \
         CREATE INDEX idx_q_nodes_gers ON q_nodes(gers_id);"
    )
    .context("adapt stage indexes")?;

    pb.finish_and_clear();
    info!(edges = edge_idx, nodes = seen_nodes.len(), restrictions = restr_count,
          "adapt+split+quantize complete");
    Ok(edge_idx as usize)
}

fn fetch_ways_batch(conn: &Connection, offset: i64, limit: i64) -> Result<Vec<WayRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, frc, fow, direction, node_ids FROM ways LIMIT ? OFFSET ?"
    )?;
    let rows = stmt
        .query_map(params![limit, offset], |r| {
            Ok(WayRecord {
                id:        r.get(0)?,
                frc:       r.get::<_, i64>(1)? as u8,
                fow:       r.get::<_, i64>(2)? as u8,
                direction: r.get::<_, i64>(3)? as u8,
                node_ids:  r.get::<_, Vec<u8>>(4)?,
            })
        })?
        .collect::<duckdb::Result<Vec<_>>>()
        .context("fetch_ways_batch")?;
    Ok(rows)
}

fn fetch_node_coords(conn: &Connection, ids: &[i64]) -> Result<HashMap<i64, (f64, f64)>> {
    if ids.is_empty() { return Ok(HashMap::new()); }
    let ids_str = ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",");
    let sql = format!("SELECT id, lon, lat FROM node_coords WHERE id IN ({})", ids_str);
    let mut stmt = conn.prepare(&sql)?;
    let map = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, (r.get(1)?, r.get(2)?))))?
        .collect::<duckdb::Result<HashMap<i64, (f64, f64)>>>()
        .context("fetch_node_coords")?;
    Ok(map)
}

fn fetch_intersection_nodes(conn: &Connection, ids: &[i64]) -> Result<HashSet<i64>> {
    if ids.is_empty() { return Ok(HashSet::new()); }
    let ids_str = ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",");
    let sql = format!("SELECT node_id FROM intersection_nodes WHERE node_id IN ({})", ids_str);
    let mut stmt = conn.prepare(&sql)?;
    let set = stmt
        .query_map([], |r| r.get(0))?
        .collect::<duckdb::Result<HashSet<i64>>>()
        .context("fetch_intersection_nodes")?;
    Ok(set)
}

/// Lossless collinear-vertex removal (mirror of quantize::remove_collinear).
pub(crate) fn remove_collinear_lm(pts: Vec<(i32, i32)>) -> Vec<(i32, i32)> {
    if pts.len() <= 2 { return pts; }
    let mut out = Vec::with_capacity(pts.len());
    out.push(pts[0]);
    for i in 1..pts.len() - 1 {
        let (x0, y0) = out.last().copied().unwrap();
        let (x1, y1) = pts[i];
        let (x2, y2) = pts[i + 1];
        let cross = (x1 - x0) as i64 * (y2 - y0) as i64
                  - (y1 - y0) as i64 * (x2 - x0) as i64;
        if cross != 0 { out.push(pts[i]); }
    }
    out.push(*pts.last().unwrap());
    out
}

// ── Phase 4: Tile from DuckDB → PMTiles ──────────────────────────────────────

pub(crate) fn tile_from_duckdb(
    conn: &Connection,
    tile_zoom: u8,
    output_dir: &Path,
    extent_slug: &str,
    release_label: &str,
    show_progress: bool,
) -> Result<()> {
    // ── Load all nodes into RAM ───────────────────────────────────────────────
    // For Europe this is ~20 M nodes × (16+8+4+4) bytes ≈ 640 MB.  Acceptable.
    let mut node_lookup:  HashMap<[u8; 16], (i32, i32)>  = HashMap::new();
    let mut node_to_tile: HashMap<[u8; 16], (u32, u32)>  = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT gers_id, lon_e7, lat_e7, tile_x, tile_y FROM q_nodes"
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let blob: Vec<u8> = row.get(0)?;
            let gers = blob_to_gers(&blob);
            let lon_e7: i32 = row.get(1)?;
            let lat_e7: i32 = row.get(2)?;
            let tile_x: u32 = row.get::<_, i64>(3)? as u32;
            let tile_y: u32 = row.get::<_, i64>(4)? as u32;
            node_lookup.insert(gers, (lon_e7, lat_e7));
            node_to_tile.insert(gers, (tile_x, tile_y));
        }
    }
    info!(nodes = node_lookup.len(), "node_lookup loaded");

    // ── Scan edge metadata to build tile_bins, boundary_nodes, edge maps ─────
    let mut tile_bins: HashMap<(u32, u32), Vec<u32>> = HashMap::new();
    let mut node_tile_id: HashMap<[u8; 16], u64> = HashMap::new(); // gers → first tile_id seen (u64::MAX = boundary)
    let mut from_edge_map: HashMap<([u8; 16], [u8; 16]), u32> = HashMap::new();
    let mut to_edge_map:   HashMap<([u8; 16], [u8; 16]), u32> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT edge_idx, start_gers, end_gers, parent_gers, tile_x, tile_y, tile_id FROM q_edges"
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let edge_idx: u32 = row.get::<_, i64>(0)? as u32;
            let start_blob: Vec<u8> = row.get(1)?;
            let end_blob:   Vec<u8> = row.get(2)?;
            let parent_blob: Vec<u8> = row.get(3)?;
            let tile_x: u32  = row.get::<_, i64>(4)? as u32;
            let tile_y: u32  = row.get::<_, i64>(5)? as u32;
            let tile_id: u64 = row.get::<_, i64>(6)? as u64;

            let start_gers  = blob_to_gers(&start_blob);
            let end_gers    = blob_to_gers(&end_blob);
            let parent_gers = blob_to_gers(&parent_blob);

            tile_bins.entry((tile_x, tile_y)).or_default().push(edge_idx);

            // Boundary node detection: node is boundary if it appears in 2+ different tiles.
            for gers in [start_gers, end_gers] {
                let entry = node_tile_id.entry(gers).or_insert(tile_id);
                if *entry != tile_id { *entry = u64::MAX; } // sentinel: boundary
            }

            from_edge_map.insert((parent_gers, end_gers),   edge_idx);
            to_edge_map.insert(  (parent_gers, start_gers), edge_idx);
        }
    }
    let boundary_nodes: HashSet<[u8; 16]> = node_tile_id
        .into_iter()
        .filter(|(_, v)| *v == u64::MAX)
        .map(|(k, _)| k)
        .collect();

    info!(
        tiles           = tile_bins.len(),
        boundary_nodes  = boundary_nodes.len(),
        "tile metadata scanned"
    );

    // ── Resolve restrictions ──────────────────────────────────────────────────
    let mut resolved: Vec<ResolvedRestriction> = Vec::new();
    let mut n_skipped = 0usize;
    {
        let mut stmt = conn.prepare("SELECT from_gers, via_gers, to_gers, flags FROM restriction_triples")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let from_blob: Vec<u8> = row.get(0)?;
            let via_blob:  Vec<u8> = row.get(1)?;
            let to_blob:   Vec<u8> = row.get(2)?;
            let flags: u8          = row.get::<_, i64>(3)? as u8;

            let from_gers = blob_to_gers(&from_blob);
            let via_gers  = blob_to_gers(&via_blob);
            let to_gers   = blob_to_gers(&to_blob);

            let from_edge_idx = match from_edge_map.get(&(from_gers, via_gers)) {
                Some(&i) => i, None => { n_skipped += 1; continue; }
            };
            let to_edge_idx = match to_edge_map.get(&(to_gers, via_gers)) {
                Some(&i) => i, None => { n_skipped += 1; continue; }
            };
            let (via_tile_x, via_tile_y) = match node_to_tile.get(&via_gers) {
                Some(&t) => t, None => { n_skipped += 1; continue; }
            };

            resolved.push(ResolvedRestriction {
                from_gers, via_gers, to_gers, flags,
                from_edge_idx, to_edge_idx,
                via_tile_x, via_tile_y,
            });
        }
    }
    if !resolved.is_empty() || n_skipped > 0 {
        info!(resolved = resolved.len(), skipped = n_skipped, "restrictions resolved");
    }

    // Group resolved restrictions by via tile.
    let mut tile_restrictions: HashMap<(u32, u32), Vec<&ResolvedRestriction>> = HashMap::new();
    for r in &resolved {
        tile_restrictions
            .entry((r.via_tile_x, r.via_tile_y))
            .or_default()
            .push(r);
    }

    // ── Sort tiles by Hilbert tile_id ─────────────────────────────────────────
    let mut tile_keys: Vec<(u32, u32)> = tile_bins.keys().copied().collect();
    tile_keys.sort_by_key(|(x, y)| xyz_to_tile_id(tile_zoom, *x, *y));

    // ── Stream tiles → PMTiles ────────────────────────────────────────────────
    let safe_release = release_label.replace('.', "-");
    let archive_filename = format!("openlrlens-{extent_slug}-{safe_release}.pmtiles");
    let archive_path = output_dir.join(&archive_filename);

    let mut writer = StreamingWriter::new().context("create StreamingWriter")?;

    let total_tiles = tile_keys.len();
    let pb = make_bar(show_progress, total_tiles as u64, "Tiling              ");
    let mut done_tiles = 0usize;

    for (tile_x, tile_y) in &tile_keys {
        let tile_id = xyz_to_tile_id(tile_zoom, *tile_x, *tile_y);
        let edge_indices = &tile_bins[&(*tile_x, *tile_y)];

        // Load full edge data for this tile.
        let edges = fetch_tile_edges(conn, *tile_x, *tile_y, edge_indices)?;

        let (node_order, node_index) = compute_tile_nodes_lm(&edges);

        // Build intra/cross restriction lists for this tile.
        let mut intra: Vec<LmIntraTile>  = Vec::new();
        let mut cross: Vec<LmCrossTile>  = Vec::new();

        if let Some(restrs) = tile_restrictions.get(&(*tile_x, *tile_y)) {
            // Local segment index: edge_idx → position in this tile's edge list.
            let local_for_edge: HashMap<u32, u32> = edges
                .iter()
                .enumerate()
                .map(|(i, e)| (e.edge_idx, i as u32))
                .collect();

            for r in restrs {
                let via_node_local = match node_index.get(&r.via_gers) {
                    Some(&i) => i, None => continue,
                };

                // Tile of the from and to edges.
                let from_tile = tile_bins.iter().find(|(_, v)| v.contains(&r.from_edge_idx)).map(|(k, _)| *k);
                let to_tile   = tile_bins.iter().find(|(_, v)| v.contains(&r.to_edge_idx)).map(|(k, _)| *k);

                let is_intra = from_tile == Some((*tile_x, *tile_y))
                            && to_tile   == Some((*tile_x, *tile_y));

                if is_intra {
                    if let (Some(&fl), Some(&tl)) = (
                        local_for_edge.get(&r.from_edge_idx),
                        local_for_edge.get(&r.to_edge_idx),
                    ) {
                        intra.push(LmIntraTile {
                            from_seg: fl,
                            via_node: via_node_local,
                            to_seg:   tl,
                            flags:    r.flags,
                        });
                    }
                } else {
                    cross.push(LmCrossTile {
                        from_gers: r.from_gers,
                        via_node_local,
                        to_gers: r.to_gers,
                        flags: r.flags,
                    });
                }
            }
        }

        let payload = build_lm_tile_payload(
            &edges, &node_order, &node_index,
            &node_lookup, &boundary_nodes, &intra, &cross,
        );
        writer.add_tile(tile_id, &payload).context("add_tile")?;

        done_tiles += 1;
        pb.inc(1);
        if done_tiles % 10_000 == 0 {
            info!(done = done_tiles, total = total_tiles, "tiling progress");
        }
    }

    pb.finish_and_clear();
    writer.finish(&archive_path, tile_zoom).context("finish PMTiles")?;
    info!(path = %archive_path.display(), tiles = total_tiles, "PMTiles archive written");

    // Write manifest.json
    write_lm_manifest(output_dir, &archive_filename, release_label, extent_slug, tile_zoom)?;
    Ok(())
}

fn fetch_tile_edges(
    conn: &Connection,
    tile_x: u32,
    tile_y: u32,
    edge_indices: &[u32],
) -> Result<Vec<LmEdge>> {
    if edge_indices.is_empty() { return Ok(vec![]); }

    // Build a query filtered by tile coordinates (uses idx_q_edges_tile).
    let ids_str = edge_indices.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT edge_idx, start_gers, end_gers, parent_gers, geom_blob, length_cm, frc, fow, direction \
         FROM q_edges WHERE tile_x = {} AND tile_y = {} AND edge_idx IN ({}) ORDER BY edge_idx",
        tile_x, tile_y, ids_str
    );

    struct TileEdgeRow {
        edge_idx: u32,
        start:    Vec<u8>,
        end:      Vec<u8>,
        parent:   Vec<u8>,
        geom:     Vec<u8>,
        len_cm:   u32,
        frc:      u8,
        fow:      u8,
        dir:      u8,
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<TileEdgeRow> = stmt
        .query_map([], |r| {
            Ok(TileEdgeRow {
                edge_idx: r.get::<_, i64>(0)? as u32,
                start:    r.get(1)?,
                end:      r.get(2)?,
                parent:   r.get(3)?,
                geom:     r.get(4)?,
                len_cm:   r.get::<_, i64>(5)? as u32,
                frc:      r.get::<_, i64>(6)? as u8,
                fow:      r.get::<_, i64>(7)? as u8,
                dir:      r.get::<_, i64>(8)? as u8,
            })
        })?
        .collect::<duckdb::Result<Vec<_>>>()
        .context("fetch_tile_edges")?;

    Ok(rows
        .into_iter()
        .map(|r| LmEdge {
            edge_idx:   r.edge_idx,
            start_gers: blob_to_gers(&r.start),
            end_gers:   blob_to_gers(&r.end),
            parent_gers: blob_to_gers(&r.parent),
            geom:        blob_to_geom(&r.geom),
            length_cm:   r.len_cm,
            frc:         r.frc,
            fow:         r.fow,
            direction:   r.dir,
        })
        .collect())
}

fn write_lm_manifest(
    output_dir: &Path,
    archive_filename: &str,
    release: &str,
    extent_slug: &str,
    tile_zoom: u8,
) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let built_at = format!("{}Z", secs); // epoch seconds, close enough for manifest

    let manifest = serde_json::json!({
        "archive":   archive_filename,
        "release":   release,
        "extent":    extent_slug,
        "tile_zoom": tile_zoom,
        "built_at":  built_at,
    });
    let path = output_dir.join("manifest.json");
    std::fs::write(&path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("write {}", path.display()))?;
    info!(path = %path.display(), "manifest written");
    Ok(())
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn run_pipeline(
    pbf_path:         &Path,
    bbox:             Option<Bbox>,
    schema:           &OsmSchemaMapping,
    output_dir:       &Path,
    extent_slug:      &str,
    release_label:    &str,
    tile_zoom:        u8,
    duckdb_memory_mb: Option<u64>,
    show_progress:    bool,
) -> Result<()> {
    std::fs::create_dir_all(output_dir)?;
    let conn = setup_duckdb(duckdb_memory_mb)?;

    // Phase 1: extract ways and relations.
    info!("low-memory: Pass 1 — extract ways");
    extract_pass1(pbf_path, schema, &conn, show_progress)?;

    // Build intersection_nodes and unique_refs.
    info!("low-memory: computing intersection nodes");
    compute_derived_tables(&conn)?;

    // Phase 2: extract node coordinates.
    info!("low-memory: Pass 2 — extract node coordinates");
    extract_pass2(pbf_path, &conn, show_progress)?;

    // Optional bbox filter.
    if let Some(b) = bbox {
        info!(?b, "low-memory: applying bbox filter");
        apply_bbox_filter(b, &conn)?;
    }

    // Phase 3: adapt + split + quantize.
    info!("low-memory: adapt + split + quantize");
    adapt_split_quantize(&conn, tile_zoom, show_progress)?;

    // Phase 4: tile and write PMTiles.
    info!("low-memory: tiling");
    tile_from_duckdb(&conn, tile_zoom, output_dir, extent_slug, release_label, show_progress)?;

    Ok(())
}
