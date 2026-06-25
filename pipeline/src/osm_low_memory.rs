/// DuckDB-backed low-memory OSM PBF → PMTiles pipeline.
///
/// Invoked by `build::run_osm` when `--low-memory` is set.  The entire pipeline
/// is driven through a DuckDB scratch database so no large Vec/HashMap structures
/// accumulate in the Rust heap.  Peak Rust heap per stage is O(one batch) rather
/// than O(all data).
///
/// Stages and their DuckDB tables:
///   Pass 1 (PBF ways+relations) → ways, restrictions_raw; node counts kept in
///                                  a Rust HashMap, written to node_ref_deltas
///                                  as pre-aggregated rows after the scan
///   Derived                      → intersection_nodes, unique_refs
///   Pass 2 (PBF nodes)          → node_coords
///   Bbox filter                  → prunes ways, node_coords in-place
///   Adapt+split+quantize         → q_edges, q_nodes, restriction_triples
///   Tile                         → PMTiles via StreamingWriter

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use indicatif::{ProgressBar, ProgressStyle};
use osmpbf::{Element, ElementReader, RelMemberType};
use tracing::{info, warn};

// ── Byte-counting reader ──────────────────────────────────────────────────────

/// Wraps any `Read` and atomically tracks how many bytes have been consumed.
/// Used to drive a file-size-based progress bar without re-scanning the file.
struct CountingReader<R: Read> {
    inner: R,
    count: Arc<AtomicU64>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

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
    node_to_tile: &HashMap<[u8; 16], (u32, u32)>,
    tile_x: u32,
    tile_y: u32,
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
        let is_boundary = node_to_tile.get(gers) != Some(&(tile_x, tile_y));
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
            .template("{spinner:.cyan} {msg} [{elapsed_precise}]")
            .expect("valid template"),
    );
    pb.set_message(msg);
    pb.enable_steady_tick(Duration::from_millis(120));
    pb
}

/// Progress bar that displays `bytes_read / total_bytes` with a percentage bar.
/// Used for the two PBF scan passes where file size is the natural denominator.
pub(crate) fn make_bytes_bar(show: bool, total_bytes: u64, msg: &'static str) -> ProgressBar {
    if !show { return ProgressBar::hidden(); }
    let pb = ProgressBar::new(total_bytes);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{msg:32} [{bar:40.cyan/blue}] {bytes}/{total_bytes}  eta {eta}")
            .expect("valid template")
            .progress_chars("█▉▊▋▌▍▎▏ "),
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

fn setup_duckdb(memory_mb_override: Option<u64>, temp_dir: &Path) -> Result<Connection> {
    let limit_mb = match memory_mb_override {
        Some(mb) => mb,
        None => {
            let avail = available_ram_bytes();
            // Default: 40 % of currently available RAM, floor 1 GB.
            let mb = ((avail as f64 * 0.40) / 1_048_576.0) as u64;
            mb.max(1_024)
        }
    };

    // Use a file-backed DuckDB database so table data lives on disk rather than
    // in RAM.  The memory_limit then controls only the buffer pool (how much
    // data is cached in RAM at once).  This means the 300-400M-row GROUP BY on
    // node_ref_deltas can spill naturally through the buffer manager without any
    // extra temp_directory configuration — file-backed storage IS the spill
    // target.  In-memory DuckDB does not spill regardless of temp_directory.
    std::fs::create_dir_all(temp_dir).context("create DuckDB temp dir")?;
    let db_file = temp_dir.join("pipeline.duckdb");

    let conn = Connection::open(&db_file).context("open DuckDB")?;
    conn.execute_batch(&format!(
        "PRAGMA threads={threads}; \
         SET memory_limit='{limit_mb}MB'; \
         SET preserve_insertion_order=false; \
         CREATE TABLE ways (id BIGINT, frc INTEGER, fow INTEGER, direction INTEGER, node_ids BLOB); \
         CREATE TABLE restrictions_raw (from_way_id BIGINT, via_node_id BIGINT, to_way_id BIGINT); \
         CREATE TABLE node_ref_deltas (node_id BIGINT, delta BIGINT); \
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
    {
        let mut app = conn.appender("ways").context("appender ways")?;
        for w in ways.iter() {
            app.append_row(params![w.id, w.frc as i64, w.fow as i64, w.direction as i64, &w.node_ids])
                .context("append way")?;
        }
        app.flush().context("flush ways")?;
    }
    ways.clear();
    if !deltas.is_empty() {
        let mut app = conn.appender("node_ref_deltas").context("appender node_ref_deltas")?;
        for (nid, d) in deltas.iter() {
            app.append_row(params![*nid, *d]).context("append delta")?;
        }
        app.flush().context("flush node_ref_deltas")?;
        deltas.clear();
    }
    Ok(())
}

fn extract_pass1(pbf_path: &Path, schema: &OsmSchemaMapping, conn: &Connection, show_progress: bool) -> Result<usize> {
    let file_size = std::fs::metadata(pbf_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let bytes_read = Arc::new(AtomicU64::new(0));
    let pb = make_bytes_bar(show_progress, file_size, "Pass 1/2  scanning ways ");
    let mut ways_scanned: u64 = 0;
    let mut elements_seen: u64 = 0;

    let file = std::fs::File::open(pbf_path)
        .with_context(|| format!("open {}", pbf_path.display()))?;
    let counting = CountingReader {
        inner: std::io::BufReader::new(file),
        count: Arc::clone(&bytes_read),
    };
    let reader = ElementReader::new(counting);

    let mut way_batch: Vec<WayRecord>               = Vec::with_capacity(WAY_BATCH + 64);
    let mut delta_batch: Vec<(i64, i64)>            = Vec::with_capacity(WAY_BATCH * 12);
    let mut restriction_batch: Vec<(i64, i64, i64)> = Vec::with_capacity(8_192);
    let mut err: Option<anyhow::Error>              = None;

    reader.for_each(|el| {
        if err.is_some() { return; }
        elements_seen += 1;
        if elements_seen % 50_000 == 0 {
            pb.set_position(bytes_read.load(Ordering::Relaxed));
        }
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

                let last = node_ids.len() - 1;
                for (i, &nid) in node_ids.iter().enumerate() {
                    let delta: i64 = if i == 0 || i == last { 2 } else { 1 };
                    delta_batch.push((nid, delta));
                }

                way_batch.push(WayRecord { id: w.id(), frc, fow, direction, node_ids: node_ids_to_blob(&node_ids) });
                ways_scanned += 1;

                if way_batch.len() >= WAY_BATCH {
                    if let Err(e) = flush_way_batch(conn, &mut way_batch, &mut delta_batch) {
                        err = Some(e); return;
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
    pb.finish_and_clear(); // reading done; dismiss bar before slow DuckDB flush

    // Flush remaining ways and deltas.
    flush_way_batch(conn, &mut way_batch, &mut delta_batch)?;

    // Insert restrictions via Appender.
    if !restriction_batch.is_empty() {
        let mut app = conn.appender("restrictions_raw").context("appender restrictions_raw")?;
        for (f, v, t) in &restriction_batch {
            app.append_row(params![f, v, t]).context("append restriction")?;
        }
        app.flush().context("flush restrictions_raw")?;
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
    info!(ways = way_count, node_ref_deltas = delta_count, restrictions = restr_count,
          "Pass 1 complete");
    Ok(way_count as usize)
}

// ── Derived tables ────────────────────────────────────────────────────────────

fn compute_derived_tables(conn: &Connection, show_progress: bool) -> Result<usize> {
    let pb = make_spinner(show_progress, "Building intersection index");

    // A single GROUP BY over all ~100-150M unique node_ids needs a ~6-8 GB hash
    // table, which exhausts the 8 GB memory limit even with file-backed storage.
    // Solution: partition by node_id % CHUNKS.  Each chunk's hash table covers
    // only 1/CHUNKS of the unique keys (~750 MB at CHUNKS=8) — well within budget.
    // Each chunk does one full scan of node_ref_deltas (on disk); CHUNKS=8 means
    // 8 sequential scans, fast on any SSD.
    const CHUNKS: i64 = 8;

    conn.execute_batch(
        "CREATE TABLE intersection_nodes (node_id BIGINT); \
         CREATE TABLE unique_refs (node_id BIGINT);"
    ).context("create derived tables")?;

    for chunk in 0..CHUNKS {
        if show_progress {
            pb.set_message(format!("Building intersection index ({}/{})", chunk + 1, CHUNKS));
        }
        conn.execute_batch(&format!(
            // One scan builds a temp aggregate for this partition, then we split
            // it into the two output tables without a second scan.
            "CREATE TEMP TABLE _agg AS \
                 SELECT node_id, SUM(delta) AS total \
                 FROM node_ref_deltas WHERE node_id % {CHUNKS} = {chunk} \
                 GROUP BY node_id; \
             INSERT INTO unique_refs        SELECT node_id FROM _agg; \
             INSERT INTO intersection_nodes SELECT node_id FROM _agg WHERE total >= 2; \
             DROP TABLE _agg;"
        )).with_context(|| format!("compute derived tables chunk {chunk}/{CHUNKS}"))?;
    }

    conn.execute_batch(
        "CREATE INDEX idx_unique_refs    ON unique_refs(node_id); \
         CREATE INDEX idx_intersection   ON intersection_nodes(node_id);"
    ).context("create derived indexes")?;

    pb.finish_and_clear();

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

fn extract_pass2(
    pbf_path:   &Path,
    conn:       &Connection,
    unique_ids: &HashSet<i64>,
    show_progress: bool,
) -> Result<usize> {
    let file_size = std::fs::metadata(pbf_path).map(|m| m.len()).unwrap_or(0);
    let bytes_read = Arc::new(AtomicU64::new(0));
    let pb = make_bytes_bar(show_progress, file_size, "Pass 2/2  scanning nodes");
    let mut elements_seen: u64 = 0;
    let mut err: Option<anyhow::Error> = None;

    // Write matching nodes directly to node_coords via Appender — no staging
    // table, no DuckDB hash join.  Membership is checked against the Rust
    // HashSet loaded from unique_refs, which is O(1) per node and uses ~1 GB.
    let mut app = conn.appender("node_coords").context("appender node_coords")?;

    let file = std::fs::File::open(pbf_path)
        .with_context(|| format!("open {}", pbf_path.display()))?;
    let counting = CountingReader { inner: std::io::BufReader::new(file), count: Arc::clone(&bytes_read) };
    let reader = ElementReader::new(counting);

    reader.for_each(|el| {
        if err.is_some() { return; }
        elements_seen += 1;
        if elements_seen % 50_000 == 0 {
            pb.set_position(bytes_read.load(Ordering::Relaxed));
        }
        let (id, lon, lat) = match el {
            Element::Node(n)      => (n.id(), n.lon(), n.lat()),
            Element::DenseNode(n) => (n.id(), n.lon(), n.lat()),
            _ => return,
        };
        if unique_ids.contains(&id) {
            if let Err(e) = app.append_row(params![id, lon, lat]) {
                err = Some(anyhow::anyhow!("append node_coords: {e}"));
            }
        }
    })?;

    if let Some(e) = err { return Err(e); }
    pb.finish_and_clear();
    app.flush().context("flush node_coords")?;

    let stored: i64 = conn
        .query_row("SELECT COUNT(*) FROM node_coords", [], |r| r.get(0))
        .context("count node_coords")?;
    info!(nodes_loaded = stored, "Pass 2 complete");
    Ok(stored as usize)
}

// ── Bbox filter ───────────────────────────────────────────────────────────────

fn apply_bbox_filter(bbox: Bbox, conn: &Connection, show_progress: bool) -> Result<()> {
    let pb = make_spinner(show_progress, "Applying bbox filter     ");
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

    pb.finish_and_clear();
    let ways_left: i64 = conn.query_row("SELECT COUNT(*) FROM ways", [], |r| r.get(0))?;
    let nodes_left: i64 = conn.query_row("SELECT COUNT(*) FROM node_coords", [], |r| r.get(0))?;
    info!(ways = ways_left, nodes = nodes_left, "after bbox filter");
    Ok(())
}

// ── Phase 3: Adapt + split + quantize ─────────────────────────────────────────

pub(crate) fn adapt_split_quantize(conn: &Connection, tile_zoom: u8, duckdb_memory_mb: Option<u64>, show_progress: bool) -> Result<usize> {
    // The heavy DuckDB operations (GROUP BY, index builds) are done by this point.
    // Adapt only does sequential scans + Appender writes, so drop the buffer pool
    // limit to 2 GB — freeing ~6 GB on a 32 GB machine before nc_map is allocated.
    conn.execute_batch("SET memory_limit='2048MB';")
        .context("lower DuckDB memory limit for adapt stage")?;

    // Index for cursor-based way streaming (avoids O(N²) LIMIT/OFFSET scan).
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_ways_id ON ways(id);")
        .context("create ways id index")?;

    // Load intersection_nodes into a Rust HashSet — replaces per-batch DuckDB
    // lookups that each build an IN-list query against the disk-backed table.
    let ix_count: i64 = conn.query_row("SELECT COUNT(*) FROM intersection_nodes", [], |r| r.get(0))?;
    let mut ix_nodes: HashSet<i64> = HashSet::with_capacity(ix_count as usize + 64);
    {
        let mut stmt = conn.prepare("SELECT node_id FROM intersection_nodes")
            .context("prepare ix_nodes")?;
        for row in stmt.query_map([], |r| r.get::<_, i64>(0))
            .context("query ix_nodes")?
        {
            ix_nodes.insert(row?);
        }
    }
    info!(count = ix_nodes.len(), "intersection nodes loaded into RAM");

    // Load node_coords as quantized i32 pairs — replaces per-batch IN-list
    // random reads against 338M rows on disk.  Storing as i32 (1e-7°) halves
    // the per-entry size vs f64; we reconvert to f64 at use time (sub-cm error).
    let nc_count: i64 = conn.query_row("SELECT COUNT(*) FROM node_coords", [], |r| r.get(0))?;
    let mut nc_map: HashMap<i64, (i32, i32)> = HashMap::with_capacity(nc_count as usize + 64);
    {
        let mut stmt = conn.prepare("SELECT id, lon, lat FROM node_coords")
            .context("prepare nc_map")?;
        for row in stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?))
        }).context("query nc_map")? {
            let (id, lon, lat) = row?;
            nc_map.insert(id, (quantize_coord(lon), quantize_coord(lat)));
        }
    }
    info!(count = nc_map.len(), "node coords loaded into RAM");

    let way_count: i64 = conn.query_row("SELECT COUNT(*) FROM ways", [], |r| r.get(0))?;
    let pb = make_bar(show_progress, way_count as u64, "Adapt/split/quantize");
    let mut edge_idx: u32 = 0;
    let mut seen_nodes: HashSet<[u8; 16]> = HashSet::new();

    // Appenders for bulk output — replaces per-edge/node conn.execute() calls.
    let mut edge_app = conn.appender("q_edges").context("appender q_edges")?;
    let mut node_app = conn.appender("q_nodes").context("appender q_nodes")?;

    // Cursor-based streaming: WHERE id > last_id ORDER BY id LIMIT N.
    // Each batch is O(log N + batch_size) with the index, not O(offset + batch_size).
    const STREAM_BATCH: i64 = 50_000;
    let mut last_id: i64 = i64::MIN;

    loop {
        let batch: Vec<WayRecord> = {
            let mut stmt = conn.prepare(
                "SELECT id, frc, fow, direction, node_ids \
                 FROM ways WHERE id > ? ORDER BY id LIMIT ?"
            ).context("prepare ways cursor")?;
            stmt.query_map(params![last_id, STREAM_BATCH], |r| Ok(WayRecord {
                id:        r.get(0)?,
                frc:       r.get::<_, i64>(1)? as u8,
                fow:       r.get::<_, i64>(2)? as u8,
                direction: r.get::<_, i64>(3)? as u8,
                node_ids:  r.get::<_, Vec<u8>>(4)?,
            }))
            .context("query ways cursor")?
            .collect::<duckdb::Result<Vec<_>>>()
            .context("collect ways batch")?
        };
        if batch.is_empty() { break; }

        for way_row in &batch {
            let way_id    = way_row.id;
            let node_ids  = blob_to_node_ids(&way_row.node_ids);
            let frc       = way_row.frc as u8;
            let fow       = way_row.fow as u8;
            let direction = way_row.direction as u8;

            if node_ids.len() < 2 { continue; }

            let parent_gers = encode_way_id(way_id);
            let last = node_ids.len() - 1;

            let mut split_starts: Vec<usize> = vec![0];
            for (i, &nid) in node_ids[1..last].iter().enumerate() {
                if ix_nodes.contains(&nid) {
                    split_starts.push(i + 1);
                }
            }

            for (k, &start_idx) in split_starts.iter().enumerate() {
                let end_idx = if k + 1 < split_starts.len() { split_starts[k + 1] } else { last };

                // Build geometry from preloaded map (no DuckDB round-trip).
                let mut geom_q: Vec<(i32, i32)> = Vec::with_capacity(end_idx - start_idx + 1);
                let mut ok = true;
                for &nid in &node_ids[start_idx..=end_idx] {
                    if let Some(&c) = nc_map.get(&nid) {
                        geom_q.push(c);
                    } else {
                        warn!(way = way_id, node = nid, "missing coords, sub-edge skipped");
                        ok = false;
                        break;
                    }
                }
                if !ok || geom_q.len() < 2 { continue; }

                // Compute length from f64 (reconvert quantized coords).
                let geom_f64: Vec<(f64, f64)> = geom_q.iter()
                    .map(|&(x, y)| (x as f64 * 1e-7, y as f64 * 1e-7))
                    .collect();
                let length_m  = polyline_length_m(&geom_f64);
                let length_cm = (length_m * 100.0).round() as u32;

                let geom_q    = remove_collinear_lm(geom_q);
                let geom_blob = geom_to_blob(&geom_q);

                let start_nid  = node_ids[start_idx];
                let end_nid    = node_ids[end_idx];
                let start_gers = encode_node_id(start_nid);
                let end_gers   = encode_node_id(end_nid);

                // Assign to the tile of the start node, and also to the tile of the
                // end node if different.  This ensures A* can always find all segments
                // incident to a node by loading that node's home tile.
                let (stx, sty) = lon_lat_to_tile_xy(geom_f64[0].0, geom_f64[0].1, tile_zoom);
                let (etx, ety) = lon_lat_to_tile_xy(
                    geom_f64.last().unwrap().0, geom_f64.last().unwrap().1, tile_zoom,
                );
                edge_app.append_row(params![
                    edge_idx as i64,
                    start_gers.as_slice(), end_gers.as_slice(), parent_gers.as_slice(),
                    geom_blob.as_slice(), length_cm as i64,
                    frc as i64, fow as i64, direction as i64,
                    stx as i64, sty as i64, xyz_to_tile_id(tile_zoom, stx, sty) as i64
                ]).context("append q_edge start-tile")?;
                if (etx, ety) != (stx, sty) {
                    edge_app.append_row(params![
                        edge_idx as i64,
                        start_gers.as_slice(), end_gers.as_slice(), parent_gers.as_slice(),
                        geom_blob.as_slice(), length_cm as i64,
                        frc as i64, fow as i64, direction as i64,
                        etx as i64, ety as i64, xyz_to_tile_id(tile_zoom, etx, ety) as i64
                    ]).context("append q_edge end-tile")?;
                }
                edge_idx += 1;

                for (ngers, (nlon, nlat)) in [
                    (start_gers, geom_f64[0]),
                    (end_gers,   *geom_f64.last().unwrap()),
                ] {
                    if seen_nodes.insert(ngers) {
                        let lon_e7 = quantize_coord(nlon);
                        let lat_e7 = quantize_coord(nlat);
                        let (ntx, nty) = lon_lat_to_tile_xy(nlon, nlat, tile_zoom);
                        node_app.append_row(params![
                            ngers.as_slice(), lon_e7, lat_e7,
                            ntx as i64, nty as i64
                        ]).context("append q_node")?;
                    }
                }
            }
        }

        last_id = batch.last().unwrap().id;
        pb.inc(batch.len() as u64);
    }

    edge_app.flush().context("flush q_edges")?;
    node_app.flush().context("flush q_nodes")?;
    drop(edge_app);
    drop(node_app);

    // Free the large maps, then restore the DuckDB memory limit to whatever the
    // user requested.  The checkpoint DuckDB runs before building indexes needs
    // the same headroom that earlier passes used — it has to flush all Appender
    // WAL data (70M+ edge rows) to disk in one pass.
    drop(ix_nodes);
    drop(nc_map);
    {
        let restore_mb = match duckdb_memory_mb {
            Some(mb) => mb,
            None => {
                let avail = available_ram_bytes();
                ((avail as f64 * 0.40) / 1_048_576.0) as u64
            }
        };
        conn.execute_batch(&format!("SET memory_limit='{restore_mb}MB';"))
            .context("restore DuckDB memory limit before index build")?;
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

    // ── Scan edge metadata to build edge maps and tile count ─────────────────────
    let mut seen_tile_ids: HashSet<u64>  = HashSet::new();
    let mut from_edge_map: HashMap<([u8; 16], [u8; 16]), u32> = HashMap::new();
    let mut to_edge_map:   HashMap<([u8; 16], [u8; 16]), u32> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT edge_idx, start_gers, end_gers, parent_gers, tile_id FROM q_edges"
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let edge_idx: u32 = row.get::<_, i64>(0)? as u32;
            let start_blob:  Vec<u8> = row.get(1)?;
            let end_blob:    Vec<u8> = row.get(2)?;
            let parent_blob: Vec<u8> = row.get(3)?;
            let tile_id: u64 = row.get::<_, i64>(4)? as u64;

            let start_gers  = blob_to_gers(&start_blob);
            let end_gers    = blob_to_gers(&end_blob);
            let parent_gers = blob_to_gers(&parent_blob);

            seen_tile_ids.insert(tile_id);

            // Same edge may appear twice (start tile + end tile); HashMap insert is
            // idempotent here since both rows carry identical (parent_gers, end/start_gers).
            from_edge_map.insert((parent_gers, end_gers),   edge_idx);
            to_edge_map.insert(  (parent_gers, start_gers), edge_idx);
        }
    }
    let total_tiles = seen_tile_ids.len();
    drop(seen_tile_ids);

    info!(tiles = total_tiles, "tile metadata scanned");

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

    // ── Stream all edges ordered by Hilbert tile_id — one scan, zero per-tile queries ──
    // DuckDB sorts 70M rows by tile_id (in-memory with the available budget) once,
    // then streams them back.  We accumulate edges for the current tile and flush
    // each tile as soon as the tile_id changes, so peak RAM is one tile's edges.
    let safe_release = release_label.replace('.', "-");
    let archive_filename = format!("openlrlens-{extent_slug}-{safe_release}.pmtiles");
    let archive_path = output_dir.join(&archive_filename);

    let mut writer = StreamingWriter::new().context("create StreamingWriter")?;
    let pb = make_bar(show_progress, total_tiles as u64, "Tiling              ");
    let mut done_tiles = 0usize;

    {
        let mut stmt = conn.prepare(
            "SELECT edge_idx, start_gers, end_gers, parent_gers, geom_blob, length_cm, \
                    frc, fow, direction, tile_x, tile_y, tile_id \
             FROM q_edges ORDER BY tile_id, edge_idx"
        ).context("prepare tiling scan")?;
        let mut rows = stmt.query([]).context("execute tiling scan")?;

        let mut cur_tile:  Option<(u32, u32, u64)> = None; // (tile_x, tile_y, tile_id)
        let mut cur_edges: Vec<LmEdge>              = Vec::new();

        while let Some(row) = rows.next()? {
            let edge_idx: u32 = row.get::<_, i64>(0)? as u32;
            let start:  Vec<u8> = row.get(1)?;
            let end:    Vec<u8> = row.get(2)?;
            let parent: Vec<u8> = row.get(3)?;
            let geom:   Vec<u8> = row.get(4)?;
            let len_cm: u32     = row.get::<_, i64>(5)? as u32;
            let frc:    u8      = row.get::<_, i64>(6)? as u8;
            let fow:    u8      = row.get::<_, i64>(7)? as u8;
            let dir:    u8      = row.get::<_, i64>(8)? as u8;
            let tile_x: u32     = row.get::<_, i64>(9)? as u32;
            let tile_y: u32     = row.get::<_, i64>(10)? as u32;
            let tile_id: u64    = row.get::<_, i64>(11)? as u64;

            if let Some((cx, cy, cid)) = cur_tile {
                if (tile_x, tile_y) != (cx, cy) {
                    flush_tile(cid, cx, cy, &cur_edges,
                               &node_lookup, &node_to_tile, &tile_restrictions,
                               &mut writer)?;
                    cur_edges.clear();
                    done_tiles += 1;
                    pb.inc(1);
                }
            }
            cur_tile = Some((tile_x, tile_y, tile_id));
            cur_edges.push(LmEdge {
                edge_idx,
                start_gers:  blob_to_gers(&start),
                end_gers:    blob_to_gers(&end),
                parent_gers: blob_to_gers(&parent),
                geom:        blob_to_geom(&geom),
                length_cm:   len_cm,
                frc, fow, direction: dir,
            });
        }

        // Flush the final tile.
        if let Some((cx, cy, cid)) = cur_tile {
            if !cur_edges.is_empty() {
                flush_tile(cid, cx, cy, &cur_edges,
                           &node_lookup, &node_to_tile, &tile_restrictions,
                           &mut writer)?;
                done_tiles += 1;
                pb.inc(1);
            }
        }
    }

    pb.finish_and_clear();
    writer.finish(&archive_path, tile_zoom).context("finish PMTiles")?;
    info!(path = %archive_path.display(), tiles = done_tiles, "PMTiles archive written");

    // Write manifest.json
    write_lm_manifest(output_dir, &archive_filename, release_label, extent_slug, tile_zoom)?;
    Ok(())
}

fn flush_tile(
    tile_id: u64,
    tile_x: u32,
    tile_y: u32,
    edges: &[LmEdge],
    node_lookup: &HashMap<[u8; 16], (i32, i32)>,
    node_to_tile: &HashMap<[u8; 16], (u32, u32)>,
    tile_restrictions: &HashMap<(u32, u32), Vec<&ResolvedRestriction>>,
    writer: &mut StreamingWriter,
) -> Result<()> {
    let (node_order, node_index) = compute_tile_nodes_lm(edges);

    let mut intra: Vec<LmIntraTile> = Vec::new();
    let mut cross: Vec<LmCrossTile> = Vec::new();

    if let Some(restrs) = tile_restrictions.get(&(tile_x, tile_y)) {
        // Build local edge index once per tile (replaces the O(N) tile_bins scan).
        let local_for_edge: HashMap<u32, u32> = edges
            .iter()
            .enumerate()
            .map(|(i, e)| (e.edge_idx, i as u32))
            .collect();

        for r in restrs {
            let Some(&via_node_local) = node_index.get(&r.via_gers) else { continue };

            // A restriction is intra-tile iff both its from and to edges are in this tile.
            let is_intra = local_for_edge.contains_key(&r.from_edge_idx)
                        && local_for_edge.contains_key(&r.to_edge_idx);

            if is_intra {
                if let (Some(&fl), Some(&tl)) = (
                    local_for_edge.get(&r.from_edge_idx),
                    local_for_edge.get(&r.to_edge_idx),
                ) {
                    intra.push(LmIntraTile { from_seg: fl, via_node: via_node_local, to_seg: tl, flags: r.flags });
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
        edges, &node_order, &node_index,
        node_lookup, node_to_tile, tile_x, tile_y, &intra, &cross,
    );
    writer.add_tile(tile_id, &payload).context("add_tile")?;
    Ok(())
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
    duckdb_temp_dir:  Option<&Path>,
    show_progress:    bool,
) -> Result<()> {
    std::fs::create_dir_all(output_dir)?;
    // Default spill directory is a subdirectory of the output dir so it lands
    // on the same disk as the output archive (not tmpfs).
    let default_tmp = output_dir.join(format!(".duckdb_tmp_{}", std::process::id()));
    let temp_dir    = duckdb_temp_dir.unwrap_or(&default_tmp);
    let conn = setup_duckdb(duckdb_memory_mb, temp_dir)?;

    // Phase 1: extract ways and relations.
    info!("low-memory: Pass 1 — extract ways");
    extract_pass1(pbf_path, schema, &conn, show_progress)?;

    // Build intersection_nodes and unique_refs.
    info!("low-memory: computing intersection nodes");
    let ref_count = compute_derived_tables(&conn, show_progress)?;

    // Load unique_refs into a Rust HashSet so pass 2 can filter nodes without
    // a DuckDB hash join.  The semi-join approach (WHERE id IN unique_refs)
    // forces DuckDB to materialise a ~2-3 GB hash table on every batch call,
    // which exceeds the budget when the buffer pool is already loaded.
    // A Rust HashSet<i64> for ~100M nodes costs ~1 GB and is checked in O(1).
    info!("low-memory: loading unique node refs");
    let mut unique_ids: HashSet<i64> = HashSet::with_capacity(ref_count + 64);
    {
        let mut stmt = conn.prepare("SELECT node_id FROM unique_refs")
            .context("prepare unique_refs query")?;
        for row in stmt.query_map([], |r| r.get::<_, i64>(0))
            .context("query unique_refs")?
        {
            unique_ids.insert(row.context("read unique_refs row")?);
        }
    }
    info!(loaded = unique_ids.len(), "unique node refs ready");

    // Phase 2: extract node coordinates.
    info!("low-memory: Pass 2 — extract node coordinates");
    extract_pass2(pbf_path, &conn, &unique_ids, show_progress)?;
    drop(unique_ids);

    // Optional bbox filter.
    if let Some(b) = bbox {
        info!(?b, "low-memory: applying bbox filter");
        apply_bbox_filter(b, &conn, show_progress)?;
    }

    // Phase 3: adapt + split + quantize.
    info!("low-memory: adapt + split + quantize");
    adapt_split_quantize(&conn, tile_zoom, duckdb_memory_mb, show_progress)?;

    // Phase 4: tile and write PMTiles.
    info!("low-memory: tiling");
    tile_from_duckdb(&conn, tile_zoom, output_dir, extent_slug, release_label, show_progress)?;

    // Drop the connection before removing the spill directory (DuckDB closes
    // its temp files on drop; removing them while open would fail on Windows).
    drop(conn);
    if duckdb_temp_dir.is_none() {
        // Only clean up the default temp dir we created; leave user-supplied dirs alone.
        let _ = std::fs::remove_dir_all(&default_tmp);
    }

    Ok(())
}
