/// DuckDB-backed GeoJSONL pipeline, activated when `--low-memory` is passed
/// with `--roads` input.
///
/// Unlike the OSM path there is no two-pass scan: every GeoJSONL line contains
/// its own geometry, attributes, and node IDs, so extract + quantize are a
/// single pass.  Restrictions from the optional CSV are loaded afterward.
/// Tiling reuses `osm_low_memory::tile_from_duckdb` unchanged.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use duckdb::{params, Connection};
use flate2::read::GzDecoder;
use serde_json::Value;
use tracing::{info, warn};

use crate::osm_low_memory::{
    geom_to_blob, make_bar, make_spinner, remove_collinear_lm, tile_from_duckdb,
};
use crate::partition::available_ram_bytes;
use crate::split::haversine_m;
use crate::tile::lon_lat_to_tile_xy;

// ── ID encoding (mirrors generic_extract.rs, private here) ───────────────────

fn segment_gers(id: i64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..8].copy_from_slice(&(id as u64).to_le_bytes());
    b
}

fn node_gers(id: i64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[8..16].copy_from_slice(&(id as u64).to_le_bytes());
    b
}

// ── DuckDB setup ──────────────────────────────────────────────────────────────

fn setup_duckdb(memory_mb_override: Option<u64>, temp_dir: &Path) -> Result<Connection> {
    let limit_mb = match memory_mb_override {
        Some(mb) => mb,
        None => {
            let avail = available_ram_bytes();
            ((avail as f64 * 0.40) / 1_048_576.0) as u64
        }
    }.max(1_024);

    std::fs::create_dir_all(temp_dir).context("create DuckDB temp dir")?;
    let db_file = temp_dir.join("pipeline.duckdb");
    let conn = Connection::open(&db_file).context("open DuckDB")?;
    conn.execute_batch(&format!(
        "PRAGMA threads={threads}; \
         SET memory_limit='{limit_mb}MB'; \
         SET preserve_insertion_order=false; \
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

// ── Edge batch ────────────────────────────────────────────────────────────────

struct EdgeBatch {
    start_gers: [u8; 16],
    end_gers:   [u8; 16],
    parent_gers:[u8; 16],
    geom:       Vec<(i32, i32)>,    // quantized (lon_e7, lat_e7) vertices
    length_cm:  u32,
    frc:        u8,
    fow:        u8,
    direction:  u8,
}

// ── Feature parsing ───────────────────────────────────────────────────────────

fn map_flowdir(flowdir: i64) -> u8 {
    match flowdir {
        2 => 2, // Backward
        3 => 3, // Forward
        _ => 1, // Both (1 = bidirectional; anything unknown → Both)
    }
}

/// Parse one GeoJSONL line and quantize its geometry immediately.
/// Returns None for blank lines or degenerate geometry (< 2 vertices).
fn parse_line(
    line: &str,
    seg_to_to_int: &mut std::collections::HashMap<i64, i64>,
) -> Result<Option<EdgeBatch>> {
    let v: Value = serde_json::from_str(line).context("JSON parse")?;

    let (props, geom_val) = if let Some(p) = v.get("properties") {
        let g = v.get("geometry").context("missing geometry")?;
        (p, g)
    } else {
        let g = v.get("geometry").context("missing geometry")?;
        (&v, g)
    };

    let id       = props.get("id")      .and_then(Value::as_i64).context("missing id")?;
    let frc_raw  = props.get("frc")     .and_then(Value::as_i64).context("missing frc")?;
    let fow_raw  = props.get("fow")     .and_then(Value::as_i64).context("missing fow")?;
    let flowdir  = props.get("flowdir") .and_then(Value::as_i64).context("missing flowdir")?;
    let from_int = props.get("from_int").and_then(Value::as_i64).context("missing from_int")?;
    let to_int   = props.get("to_int")  .and_then(Value::as_i64).context("missing to_int")?;

    let coords = geom_val
        .get("coordinates")
        .and_then(Value::as_array)
        .context("missing coordinates")?;

    if coords.len() < 2 {
        return Ok(None);
    }

    let mut float_geom: Vec<(f64, f64)> = Vec::with_capacity(coords.len());
    for (i, c) in coords.iter().enumerate() {
        let arr = c.as_array()
            .with_context(|| format!("coordinate[{i}] not array"))?;
        let lon = arr.first().and_then(Value::as_f64)
            .with_context(|| format!("coordinate[{i}] missing lon"))?;
        let lat = arr.get(1).and_then(Value::as_f64)
            .with_context(|| format!("coordinate[{i}] missing lat"))?;
        float_geom.push((lon, lat));
    }

    let length_m: f64 = float_geom
        .windows(2)
        .map(|w| haversine_m(w[0].0, w[0].1, w[1].0, w[1].1))
        .sum();
    let length_cm = (length_m * 100.0).round() as u32;

    // Quantize to 1e-7 degree integers (sub-meter, Invariant 4).
    let q_geom_raw: Vec<(i32, i32)> = float_geom.iter()
        .map(|&(lon, lat)| (
            (lon * 1e7).round() as i32,
            (lat * 1e7).round() as i32,
        ))
        .collect();
    let geom = remove_collinear_lm(q_geom_raw);
    if geom.len() < 2 {
        return Ok(None);
    }

    seg_to_to_int.insert(id, to_int);

    Ok(Some(EdgeBatch {
        start_gers:  node_gers(from_int),
        end_gers:    node_gers(to_int),
        parent_gers: segment_gers(id),
        geom,
        length_cm,
        frc:       frc_raw.clamp(0, 7) as u8,
        fow:       fow_raw.clamp(0, 7) as u8,
        direction: map_flowdir(flowdir),
    }))
}

// ── Phase 1: Extract + quantize from GeoJSONL ─────────────────────────────────

fn process_geojsonl_file(
    path: &Path,
    tile_zoom: u8,
    seg_to_to_int: &mut std::collections::HashMap<i64, i64>,
    seen_nodes: &mut HashSet<[u8; 16]>,
    edge_idx: &mut u32,
    edge_app: &mut duckdb::Appender<'_>,
    node_app: &mut duckdb::Appender<'_>,
    pb: &indicatif::ProgressBar,
) -> Result<()> {
    let file = File::open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let path_str = path.to_string_lossy().to_lowercase();
    let reader: Box<dyn BufRead> = if path_str.ends_with(".gz") {
        Box::new(BufReader::new(GzDecoder::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };

    let mut n_skip = 0usize;
    for (line_no, line_result) in reader.lines().enumerate() {
        let line = line_result
            .with_context(|| format!("read line {} of {}", line_no + 1, path.display()))?;
        let line = line.trim();
        if line.is_empty() { continue; }

        match parse_line(line, seg_to_to_int) {
            Ok(Some(e)) => {
                let (slon_e7, slat_e7) = e.geom[0];
                let (elon_e7, elat_e7) = *e.geom.last().unwrap();

                if seen_nodes.insert(e.start_gers) {
                    let (tx, ty) = lon_lat_to_tile_xy(
                        slon_e7 as f64 / 1e7, slat_e7 as f64 / 1e7, tile_zoom,
                    );
                    node_app.append_row(params![
                        &e.start_gers[..], slon_e7, slat_e7, tx as i64, ty as i64
                    ]).context("append q_node")?;
                }
                if seen_nodes.insert(e.end_gers) {
                    let (tx, ty) = lon_lat_to_tile_xy(
                        elon_e7 as f64 / 1e7, elat_e7 as f64 / 1e7, tile_zoom,
                    );
                    node_app.append_row(params![
                        &e.end_gers[..], elon_e7, elat_e7, tx as i64, ty as i64
                    ]).context("append q_node")?;
                }

                let (stx, sty) = lon_lat_to_tile_xy(
                    slon_e7 as f64 / 1e7, slat_e7 as f64 / 1e7, tile_zoom,
                );
                let (etx, ety) = lon_lat_to_tile_xy(
                    elon_e7 as f64 / 1e7, elat_e7 as f64 / 1e7, tile_zoom,
                );
                let geom_blob = geom_to_blob(&e.geom);
                edge_app.append_row(params![
                    *edge_idx as i64,
                    &e.start_gers[..], &e.end_gers[..], &e.parent_gers[..],
                    geom_blob.as_slice(), e.length_cm as i64,
                    e.frc as i64, e.fow as i64, e.direction as i64,
                    stx as i64, sty as i64,
                    crate::tile::xyz_to_tile_id(tile_zoom, stx, sty) as i64,
                ]).context("append q_edge start-tile")?;
                if (etx, ety) != (stx, sty) {
                    edge_app.append_row(params![
                        *edge_idx as i64,
                        &e.start_gers[..], &e.end_gers[..], &e.parent_gers[..],
                        geom_blob.as_slice(), e.length_cm as i64,
                        e.frc as i64, e.fow as i64, e.direction as i64,
                        etx as i64, ety as i64,
                        crate::tile::xyz_to_tile_id(tile_zoom, etx, ety) as i64,
                    ]).context("append q_edge end-tile")?;
                }
                *edge_idx += 1;
                pb.inc(1);
            }
            Ok(None) => { n_skip += 1; }
            Err(err) => {
                warn!(path = %path.display(), line = line_no + 1, error = %err, "parse error, skipped");
                n_skip += 1;
            }
        }
    }
    if n_skip > 0 {
        warn!(path = %path.display(), n_skip, "lines skipped");
    }
    Ok(())
}

/// Single-pass extract: reads GeoJSONL lines, quantizes in-place, inserts to
/// `q_edges` and `q_nodes`.  Returns `seg_to_to_int` for restriction loading.
fn extract_quantize(
    roads_path: &Path,
    conn: &Connection,
    tile_zoom: u8,
    show_progress: bool,
) -> Result<std::collections::HashMap<i64, i64>> {
    let mut seg_to_to_int = std::collections::HashMap::new();
    let mut seen_nodes: HashSet<[u8; 16]> = HashSet::new();
    let mut edge_idx: u32 = 0;

    let mut edge_app = conn.appender("q_edges").context("appender q_edges")?;
    let mut node_app = conn.appender("q_nodes").context("appender q_nodes")?;

    let pb = make_spinner(show_progress, "Extracting GeoJSONL     ");

    if roads_path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(roads_path)
            .with_context(|| format!("read dir {}", roads_path.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                let s = p.to_string_lossy().to_lowercase();
                s.ends_with(".geojsonl") || s.ends_with(".geojsonl.gz")
                    || s.ends_with(".geojson") || s.ends_with(".geojson.gz")
            })
            .collect();
        entries.sort();
        let bar = make_bar(show_progress, entries.len() as u64, "GeoJSONL files          ");
        for path in &entries {
            process_geojsonl_file(path, tile_zoom, &mut seg_to_to_int, &mut seen_nodes,
                                  &mut edge_idx, &mut edge_app, &mut node_app, &pb)?;
            bar.inc(1);
        }
        bar.finish_and_clear();
    } else {
        process_geojsonl_file(roads_path, tile_zoom, &mut seg_to_to_int, &mut seen_nodes,
                              &mut edge_idx, &mut edge_app, &mut node_app, &pb)?;
    }

    edge_app.flush().context("flush q_edges")?;
    node_app.flush().context("flush q_nodes")?;
    drop(edge_app);
    drop(node_app);
    pb.finish_and_clear();

    let edge_count: i64 = conn.query_row("SELECT COUNT(*) FROM q_edges", [], |r| r.get(0))?;
    let node_count: i64 = conn.query_row("SELECT COUNT(*) FROM q_nodes", [], |r| r.get(0))?;
    info!(edges = edge_count, nodes = node_count, "extract+quantize complete");

    Ok(seg_to_to_int)
}

// ── Phase 2: Load restrictions CSV ───────────────────────────────────────────

/// Load turn restrictions from an optional CSV into `restriction_triples`.
///
/// CSV columns:
///   2-column: from_segment_id, to_segment_id
///   3-column: from_segment_id, via_node_id, to_segment_id
///
/// For the 2-column form, via_node is derived from `seg_to_to_int[from_id]`.
fn load_restrictions(
    csv_path: &Path,
    conn: &Connection,
    seg_to_to_int: &std::collections::HashMap<i64, i64>,
) -> Result<usize> {
    let file = File::open(csv_path)
        .with_context(|| format!("open restrictions CSV {}", csv_path.display()))?;
    let reader = BufReader::new(file);
    let mut count = 0usize;

    conn.execute_batch("BEGIN").context("BEGIN restrictions")?;
    let result: Result<()> = (|| {
        let mut stmt = conn.prepare(
            "INSERT INTO restriction_triples VALUES (?, ?, ?, 0)",
        ).context("prepare INSERT restriction_triples")?;

        for (line_no, line_result) in reader.lines().enumerate() {
            let line = line_result
                .with_context(|| format!("read restrictions line {}", line_no + 1))?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }

            let cols: Vec<&str> = line.split(',').collect();
            let (from_id, via_node_id, to_id) = match cols.len() {
                2 => {
                    let from_id: i64 = cols[0].trim().parse()
                        .with_context(|| format!("bad from_id on line {}", line_no + 1))?;
                    let to_id: i64 = cols[1].trim().parse()
                        .with_context(|| format!("bad to_id on line {}", line_no + 1))?;
                    let via_node_id = match seg_to_to_int.get(&from_id) {
                        Some(&n) => n,
                        None => {
                            warn!(line = line_no + 1, from_id, "via_node not found, restriction skipped");
                            continue;
                        }
                    };
                    (from_id, via_node_id, to_id)
                }
                3 => {
                    let from_id: i64 = cols[0].trim().parse()
                        .with_context(|| format!("bad from_id on line {}", line_no + 1))?;
                    let via_node_id: i64 = cols[1].trim().parse()
                        .with_context(|| format!("bad via_node_id on line {}", line_no + 1))?;
                    let to_id: i64 = cols[2].trim().parse()
                        .with_context(|| format!("bad to_id on line {}", line_no + 1))?;
                    (from_id, via_node_id, to_id)
                }
                _ => {
                    warn!(line = line_no + 1, "unexpected column count, skipped");
                    continue;
                }
            };

            // Encode: from is a segment GERS, via is a node GERS, to is a segment GERS.
            let from_gers = segment_gers(from_id);
            let via_gers  = node_gers(via_node_id);
            let to_gers   = segment_gers(to_id);

            stmt.execute(params![&from_gers[..], &via_gers[..], &to_gers[..]])
                .context("INSERT restriction")?;
            count += 1;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK");
        return result.map(|_| 0);
    }
    conn.execute_batch("COMMIT").context("COMMIT restrictions")?;
    Ok(count)
}

// ── Public entry point ────────────────────────────────────────────────────────

pub(crate) fn run_pipeline(
    roads_path:        &Path,
    restrictions_path: Option<&Path>,
    output_dir:        &Path,
    extent_slug:       &str,
    tile_zoom:         u8,
    duckdb_memory_mb:  Option<u64>,
    duckdb_temp_dir:   Option<&Path>,
    show_progress:     bool,
) -> Result<()> {
    std::fs::create_dir_all(output_dir)?;
    let default_tmp = output_dir.join(format!(".duckdb_tmp_{}", std::process::id()));
    let temp_dir    = duckdb_temp_dir.unwrap_or(&default_tmp);

    let conn = setup_duckdb(duckdb_memory_mb, temp_dir)?;

    // Phase 1: extract + quantize.
    let seg_to_to_int = extract_quantize(roads_path, &conn, tile_zoom, show_progress)?;

    // Phase 2: restrictions (optional).
    if let Some(csv_path) = restrictions_path {
        let count = load_restrictions(csv_path, &conn, &seg_to_to_int)?;
        info!(count, "restrictions loaded");
    }

    // Phase 3: tile + write PMTiles.
    tile_from_duckdb(&conn, tile_zoom, output_dir, extent_slug, "generic", show_progress)?;

    drop(conn);
    if duckdb_temp_dir.is_none() {
        let _ = std::fs::remove_dir_all(&default_tmp);
    }
    Ok(())
}
