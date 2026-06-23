use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{info, info_span, warn};

use crate::{
    extent::Bbox,
    http::Client,
    osm_schema::OsmSchemaMapping,
    partition,
    schema::SchemaMapping,
};

// ── OSM PBF build path ────────────────────────────────────────────────────────

/// Build a PMTiles archive from a local OSM PBF file.
///
/// Skips the Overture-specific adapt/split/restrictions steps; instead calls
/// `osm_extract::extract` + `osm_adapt::adapt` which produce split edges,
/// nodes, and restrictions directly from OSM tags.
pub async fn run_osm(
    pbf_path:         &Path,
    extent_spec:      &str,
    bbox:             Option<Bbox>,
    osm_schema:       &OsmSchemaMapping,
    output:           &Path,
    tile_zoom:        u8,
    low_memory:       bool,
    duckdb_memory_mb: Option<u64>,
    show_progress:    bool,
) -> Result<()> {
    std::fs::create_dir_all(output)?;
    let t0 = Instant::now();

    let extent_slug  = crate::extent::extent_slug(extent_spec);
    let pbf_stem     = pbf_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("osm");
    // Strip compression suffix if present (e.g. "new-zealand-latest.osm.pbf" → "new-zealand-latest")
    let release_label = pbf_stem.trim_end_matches(".osm");

    info!(
        pbf   = %pbf_path.display(),
        extent = %extent_slug,
        output = %output.display(),
        "OSM build started"
    );

    // Low-memory path: hand off entirely to the DuckDB-backed pipeline.
    if low_memory {
        let pbf_path    = pbf_path.to_path_buf();
        let schema_lm   = osm_schema.clone();
        let output_dir  = output.to_path_buf();
        let extent_slug = extent_slug.clone();
        let release_lm  = release_label.to_string();
        return tokio::task::spawn_blocking(move || {
            crate::osm_low_memory::run_pipeline(
                &pbf_path,
                bbox,
                &schema_lm,
                &output_dir,
                &extent_slug,
                &release_lm,
                tile_zoom,
                duckdb_memory_mb,
                show_progress,
            )
        })
        .await
        .context("osm_low_memory panicked")?;
    }

    // Step 1: extract ─────────────────────────────────────────────────────────
    let osm_data = {
        let _s = info_span!("osm_extract").entered();
        let pbf_path = pbf_path.to_path_buf();
        let schema_for_extract = osm_schema.clone();
        let data = tokio::task::spawn_blocking(move || {
            crate::osm_extract::extract(&pbf_path, bbox, &schema_for_extract)
        })
        .await
        .context("osm_extract panicked")??;
        info!(
            ways         = data.ways.len(),
            nodes        = data.nodes.len(),
            restrictions = data.restrictions.len(),
            elapsed_s    = t0.elapsed().as_secs_f32(),
            "OSM extract complete"
        );
        data
    };

    // Step 2: adapt + split ───────────────────────────────────────────────────
    let (edges, nodes, restrictions) = {
        let _s = info_span!("osm_adapt").entered();
        let (edges, nodes, restrictions) = tokio::task::spawn_blocking(move || {
            crate::osm_adapt::adapt(osm_data)
        })
        .await
        .context("osm_adapt panicked")?;

        let dir_fwd  = edges.iter().filter(|e| matches!(e.direction, openlr_graph::Direction::Forward)).count();
        let dir_bwd  = edges.iter().filter(|e| matches!(e.direction, openlr_graph::Direction::Backward)).count();
        let dir_both = edges.iter().filter(|e| matches!(e.direction, openlr_graph::Direction::Both)).count();
        info!(
            edges        = edges.len(),
            nodes        = nodes.len(),
            restrictions = restrictions.len(),
            dir_forward  = dir_fwd,
            dir_backward = dir_bwd,
            dir_both,
            elapsed_s    = t0.elapsed().as_secs_f32(),
            "OSM adapt complete"
        );
        (edges, nodes, restrictions)
    };

    // Step 3: quantize ────────────────────────────────────────────────────────
    let (q_edges, q_nodes) = {
        let _s = info_span!("quantize").entered();
        let (qe, qn) = tokio::task::spawn_blocking(move || {
            crate::quantize::quantize(edges, nodes)
        })
        .await
        .context("quantize panicked")?;
        info!(
            edges     = qe.len(),
            nodes     = qn.len(),
            elapsed_s = t0.elapsed().as_secs_f32(),
            "quantize complete"
        );
        (qe, qn)
    };

    // Step 4: tile and write PMTiles ──────────────────────────────────────────
    {
        let _s = info_span!("tile").entered();
        info!(tile_zoom, edges = q_edges.len(), restrictions = restrictions.len(), "tiling");
        let output_dir    = output.to_path_buf();
        let release_label = release_label.to_string();
        let extent_slug   = extent_slug.clone();
        tokio::task::spawn_blocking(move || {
            crate::tile::write_tiles(
                q_edges, q_nodes, restrictions,
                tile_zoom, &output_dir, &release_label, &extent_slug,
                low_memory,
            )
        })
        .await
        .context("tile panicked")??;
    }

    info!(
        elapsed_s = t0.elapsed().as_secs_f32(),
        output    = %output.display(),
        "OSM build complete"
    );
    Ok(())
}

// ── Generic GeoJSONL build path ───────────────────────────────────────────────

/// Build a PMTiles archive from a GeoJSONL(.gz) road network file or directory.
///
/// Bypasses adapt and split (data arrives pre-attributed and pre-split); goes
/// straight from extract → quantize → tile.
pub async fn run_generic(
    roads_path:        &Path,
    restrictions_path: Option<&Path>,
    label:             &str,
    extent_spec:       &str,
    output:            &Path,
    tile_zoom:         u8,
    low_memory:        bool,
    duckdb_memory_mb:  Option<u64>,
    show_progress:     bool,
) -> Result<()> {
    std::fs::create_dir_all(output)?;
    let t0 = Instant::now();

    let extent_slug = crate::extent::extent_slug(extent_spec);

    info!(
        roads       = %roads_path.display(),
        extent      = %extent_slug,
        output      = %output.display(),
        label,
        "generic build started"
    );

    // Low-memory path: hand off entirely to the DuckDB-backed pipeline.
    if low_memory {
        let roads       = roads_path.to_path_buf();
        let restr       = restrictions_path.map(|p| p.to_path_buf());
        let out_dir     = output.to_path_buf();
        let ext_slug    = extent_slug.clone();
        let label_owned = label.to_string();
        tokio::task::spawn_blocking(move || {
            crate::generic_low_memory::run_pipeline(
                &roads,
                restr.as_deref(),
                &out_dir,
                &ext_slug,
                tile_zoom,
                duckdb_memory_mb,
                show_progress,
            )
        })
        .await
        .context("generic_low_memory panicked")??;

        // Patch --label into manifest (same as in-memory path below).
        if !label_owned.is_empty() {
            let manifest_path = output.join("manifest.json");
            if let Ok(text) = std::fs::read_to_string(&manifest_path) {
                if let Ok(mut map) =
                    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text)
                {
                    map.insert(
                        "external_id_label".to_string(),
                        serde_json::Value::String(label_owned.clone()),
                    );
                    if let Ok(updated) =
                        serde_json::to_string_pretty(&serde_json::Value::Object(map))
                    {
                        let _ = std::fs::write(&manifest_path, updated);
                    }
                }
            }
        }

        info!(
            elapsed_s = t0.elapsed().as_secs_f32(),
            output    = %output.display(),
            "generic build complete"
        );
        return Ok(());
    }

    // Step 1: extract ─────────────────────────────────────────────────────────
    let (edges, nodes, seg_to_to_int) = {
        let _s = info_span!("generic_extract").entered();
        let roads_path = roads_path.to_path_buf();
        let (edges, nodes, seg_to_to_int) = tokio::task::spawn_blocking(move || {
            crate::generic_extract::extract(&roads_path)
        })
        .await
        .context("generic_extract panicked")??;
        info!(
            edges     = edges.len(),
            nodes     = nodes.len(),
            elapsed_s = t0.elapsed().as_secs_f32(),
            "generic extract complete"
        );
        (edges, nodes, seg_to_to_int)
    };

    // Step 2: restrictions (optional) ─────────────────────────────────────────
    let restrictions = if let Some(csv_path) = restrictions_path {
        let _s = info_span!("restrictions").entered();
        let csv_path = csv_path.to_path_buf();
        let r = tokio::task::spawn_blocking(move || {
            crate::generic_extract::read_restrictions_csv(&csv_path, &seg_to_to_int)
        })
        .await
        .context("restrictions panicked")??;
        info!(count = r.len(), elapsed_s = t0.elapsed().as_secs_f32(), "restrictions complete");
        r
    } else {
        vec![]
    };

    // Step 3: quantize ────────────────────────────────────────────────────────
    let (q_edges, q_nodes) = {
        let _s = info_span!("quantize").entered();
        let (qe, qn) = tokio::task::spawn_blocking(move || {
            crate::quantize::quantize(edges, nodes)
        })
        .await
        .context("quantize panicked")?;
        info!(
            edges     = qe.len(),
            nodes     = qn.len(),
            elapsed_s = t0.elapsed().as_secs_f32(),
            "quantize complete"
        );
        (qe, qn)
    };

    // Step 4: tile and write PMTiles ──────────────────────────────────────────
    {
        let _s = info_span!("tile").entered();
        info!(tile_zoom, edges = q_edges.len(), restrictions = restrictions.len(), "tiling");
        let output_dir   = output.to_path_buf();
        let extent_slug2 = extent_slug.clone();
        tokio::task::spawn_blocking(move || {
            crate::tile::write_tiles(
                q_edges, q_nodes, restrictions,
                tile_zoom, &output_dir, "generic", &extent_slug2,
                low_memory,
            )
        })
        .await
        .context("tile panicked")??;
    }

    // Patch label into manifest if provided.
    if !label.is_empty() {
        let manifest_path = output.join("manifest.json");
        if let Ok(text) = std::fs::read_to_string(&manifest_path) {
            if let Ok(mut map) =
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text)
            {
                map.insert(
                    "external_id_label".to_string(),
                    serde_json::Value::String(label.to_string()),
                );
                if let Ok(updated) =
                    serde_json::to_string_pretty(&serde_json::Value::Object(map))
                {
                    let _ = std::fs::write(&manifest_path, updated);
                }
            }
        }
    }

    info!(
        elapsed_s = t0.elapsed().as_secs_f32(),
        output    = %output.display(),
        "generic build complete"
    );
    Ok(())
}

// ── Public entry point ────────────────────────────────────────────────────────

pub async fn run(
    release:           &str,
    extent_spec:       &str,
    bbox:              Option<Bbox>,
    schema:            &SchemaMapping,
    output:            &Path,
    client:            &Client,
    fetch_concurrency: usize,
    tile_zoom:         u8,
    ram_gb_override:   Option<f64>,
    bytes_per_segment: u64,
    low_memory:        bool,
) -> Result<()> {
    std::fs::create_dir_all(output)?;

    // Detect RAM and decide how many partitions are needed.
    let available  = partition::available_ram_bytes();
    let budget     = partition::ram_budget_bytes(available, ram_gb_override);
    let partitions = partition::compute_partitions(bbox, budget, bytes_per_segment);

    info!(
        available_ram_gb  = format!("{:.1}", available  as f64 / 1e9),
        budget_gb         = format!("{:.1}", budget     as f64 / 1e9),
        partitions        = partitions.len(),
        "build plan"
    );

    let extent_slug = crate::extent::extent_slug(extent_spec);
    let safe_release    = release.replace('.', "-");
    let archive_name    = format!("openlrlens-{extent_slug}-{safe_release}.pmtiles");
    let final_pmtiles   = output.join(&archive_name);

    if partitions.len() == 1 {
        // ── Single-shot ────────────────────────────────────────────────────────
        run_partition(
            release, bbox, schema, output, client,
            fetch_concurrency, tile_zoom, &extent_slug, low_memory,
        )
        .await
    } else {
        // ── Multi-partition: process each piece then merge ─────────────────────
        let part_dir = output.join("_parts");
        std::fs::create_dir_all(&part_dir)?;

        let mut part_pmtiles: Vec<PathBuf> = Vec::with_capacity(partitions.len());

        for (i, part_bbox) in partitions.iter().enumerate() {
            let part_slug = format!("part-{i:04}");
            let part_out  = part_dir.join(&part_slug);
            std::fs::create_dir_all(&part_out)?;

            info!(
                partition = i + 1,
                total     = partitions.len(),
                west  = part_bbox.west,
                south = part_bbox.south,
                east  = part_bbox.east,
                north = part_bbox.north,
                "processing partition"
            );

            run_partition(
                release, Some(*part_bbox), schema, &part_out, client,
                fetch_concurrency, tile_zoom, &part_slug, low_memory,
            )
            .await?;

            match find_pmtiles(&part_out) {
                Some(p) => part_pmtiles.push(p),
                None    => warn!(dir = %part_out.display(), "no .pmtiles found in partition dir"),
            }
        }

        // Merge all partition archives into the final archive.
        {
            let _s = info_span!("merge").entered();
            info!(
                archives = part_pmtiles.len(),
                output   = %final_pmtiles.display(),
                "merging partition archives"
            );
            crate::merge::merge_pmtiles(&part_pmtiles, &final_pmtiles, tile_zoom)?;
        }

        // Write a single manifest for the merged archive.
        write_top_manifest(output, &archive_name, release, &extent_slug, tile_zoom)?;

        // Clean up partition working directories.
        if let Err(e) = std::fs::remove_dir_all(&part_dir) {
            warn!(error = %e, "could not remove partition working dir");
        }

        info!(output = %final_pmtiles.display(), "multi-partition build complete");
        Ok(())
    }
}

// ── Single-partition pipeline (the core of the original build::run) ───────────

async fn run_partition(
    release:           &str,
    bbox:              Option<Bbox>,
    schema:            &SchemaMapping,
    output_dir:        &Path,
    client:            &Client,
    fetch_concurrency: usize,
    tile_zoom:         u8,
    extent_slug:       &str,
    low_memory:        bool,
) -> Result<()> {
    let t0 = Instant::now();

    info!(
        release,
        extent   = %extent_slug,
        output   = %output_dir.display(),
        "partition started"
    );

    // Step 1: extract ─────────────────────────────────────────────────────────
    let raw_segments = {
        let _s = info_span!("extract", release, extent = %extent_slug).entered();
        info!("extracting segments from Overture parquet");
        let segs = crate::extract::extract_segments(release, bbox, client, fetch_concurrency)
            .await
            .context("extract")?;
        info!(count = segs.len(), elapsed_s = t0.elapsed().as_secs_f32(), "extract complete");
        segs
    };

    // Step 2: adapt ───────────────────────────────────────────────────────────
    let adapted = {
        let _s = info_span!("adapt").entered();
        info!("adapting class/subclass/road_flags → frc/fow/direction");
        let schema = schema.clone();
        let adapted = tokio::task::spawn_blocking(move || {
            crate::adapt::adapt(raw_segments, &schema)
        })
        .await
        .context("adapt panicked")?;
        let dir_fwd  = adapted.iter().filter(|s| matches!(s.direction, openlr_graph::Direction::Forward)).count();
        let dir_bwd  = adapted.iter().filter(|s| matches!(s.direction, openlr_graph::Direction::Backward)).count();
        let dir_both = adapted.iter().filter(|s| matches!(s.direction, openlr_graph::Direction::Both)).count();
        let excluded = adapted.iter().filter(|s| !s.vehicular).count();
        info!(count = adapted.len(), dir_forward = dir_fwd, dir_backward = dir_bwd, dir_both,
              non_vehicular_excluded = excluded, elapsed_s = t0.elapsed().as_secs_f32(), "adapt complete");
        adapted
    };

    // Filter non-vehicular segments before restrictions and split.
    let adapted: Vec<_> = adapted.into_iter().filter(|s| s.vehicular).collect();

    // Step 5 (pre-split): restrictions ────────────────────────────────────────
    let restrictions = {
        let _s = info_span!("restrictions").entered();
        info!("flattening prohibited_transitions → turn-restriction table");
        let r = crate::restrictions::flatten(&adapted);
        info!(count = r.len(), elapsed_s = t0.elapsed().as_secs_f32(), "restrictions complete");
        r
    };

    // Build the set of connector IDs that are endpoints (at ≈ 0 or 1) of vehicular segments.
    // Interior connectors not in this set connect only to non-vehicular ways and are skipped.
    let vehicular_endpoints: HashSet<String> = adapted.iter()
        .flat_map(|s| s.connectors.iter())
        .filter(|c| c.at <= 1e-9 || c.at >= 1.0 - 1e-9)
        .map(|c| c.connector_id.clone())
        .collect();

    // Steps 3+4: split at interior connectors ─────────────────────────────────
    let (edges, nodes) = {
        let _s = info_span!("split").entered();
        info!("splitting segments at interior connectors");
        let (edges, nodes) = tokio::task::spawn_blocking(move || {
            crate::split::split(adapted, &vehicular_endpoints)
        })
        .await
        .context("split panicked")?;
        info!(
            edges = edges.len(),
            nodes = nodes.len(),
            elapsed_s = t0.elapsed().as_secs_f32(),
            "split complete"
        );
        (edges, nodes)
    };

    // Step 6: quantize ────────────────────────────────────────────────────────
    let (q_edges, q_nodes) = {
        let _s = info_span!("quantize").entered();
        info!("quantizing geometry to 1e-7 degree grid");
        let (qe, qn) = tokio::task::spawn_blocking(move || {
            crate::quantize::quantize(edges, nodes)
        })
        .await
        .context("quantize panicked")?;
        info!(
            edges = qe.len(),
            nodes = qn.len(),
            elapsed_s = t0.elapsed().as_secs_f32(),
            "quantize complete"
        );
        (qe, qn)
    };

    // Step 7: tile and write PMTiles ──────────────────────────────────────────
    {
        let _s = info_span!("tile").entered();
        info!(
            tile_zoom,
            edges        = q_edges.len(),
            restrictions = restrictions.len(),
            "tiling and writing PMTiles archive"
        );
        let output_dir   = output_dir.to_path_buf();
        let release      = release.to_string();
        let extent_slug  = extent_slug.to_string();
        tokio::task::spawn_blocking(move || {
            crate::tile::write_tiles(
                q_edges, q_nodes, restrictions,
                tile_zoom, &output_dir, &release, &extent_slug,
                low_memory,
            )
        })
        .await
        .context("tile panicked")??;
        info!(elapsed_s = t0.elapsed().as_secs_f32(), "partition complete");
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Find the first `.pmtiles` file in `dir`.
fn find_pmtiles(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.find_map(|e| {
        let p = e.ok()?.path();
        if p.extension().and_then(|s| s.to_str()) == Some("pmtiles") {
            Some(p)
        } else {
            None
        }
    })
}

/// Write the manifest for the final merged archive (overwrites any per-partition manifest).
pub(crate) fn write_top_manifest(
    output_dir:    &Path,
    archive_name:  &str,
    release:       &str,
    extent_slug:   &str,
    tile_zoom:     u8,
) -> Result<()> {
    // Reuse tile::write_manifest logic via a small duplicate — avoids making it pub.
    let manifest = serde_json::json!({
        "archive":   archive_name,
        "release":   release,
        "extent":    extent_slug,
        "tile_zoom": tile_zoom,
    });
    let path = output_dir.join("manifest.json");
    std::fs::write(&path, serde_json::to_string_pretty(&manifest)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
