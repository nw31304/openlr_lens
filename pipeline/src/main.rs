mod adapt;
mod build;
mod cli;
mod extent;
mod extract;
mod generic_extract;
mod http;
mod merge;
mod osm_adapt;
mod osm_extract;
mod generic_low_memory;
mod osm_low_memory;
mod osm_schema;
mod parquet_meta;
mod partition;
mod quantize;
mod releases;
mod restrictions;
mod schema;
mod split;
mod tile;

use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Command, MergeArgs};
use http::RetryConfig;
use tracing::{debug, info};
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let default_level = match cli.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_level));
    fmt().with_env_filter(filter).with_target(false).init();

    debug!("openlrlens-build starting");

    let retry = RetryConfig::new(
        cli.retry_max,
        cli.retry_base_ms,
        cli.retry_max_ms,
        cli.retry_factor,
    );

    match cli.command {
        Command::ListReleases => releases::list_and_print(&http::Client::new(retry)).await?,
        Command::Merge(args)  => run_merge(args)?,
        Command::Build(args) => {
            if let Some(n) = args.jobs {
                rayon::ThreadPoolBuilder::new()
                    .num_threads(n)
                    .build_global()
                    .expect("failed to configure rayon thread pool");
                info!(threads = n, "rayon thread pool configured");
            }

            if let Some(roads_path) = args.roads {
                // ── Generic GeoJSONL path ─────────────────────────────────────
                // extent_slug is derived directly from the --extent label; no
                // spatial filtering is needed — the data is already the right region.
                build::run_generic(
                    &roads_path,
                    args.restrictions.as_deref(),
                    &args.label,
                    &args.extent,
                    &args.output,
                    args.tile_zoom,
                    args.low_memory,
                    args.duckdb_memory_mb,
                    args.progress,
                )
                .await?;
            } else {
                // OSM and Overture paths both need extent resolved to a bbox.
                let bbox = extent::resolve(&args.extent)?;

                match args.pbf {
                    // ── OSM PBF path ──────────────────────────────────────────
                    Some(pbf_str) => {
                        let pbf_path: PathBuf = if pbf_str.starts_with("http://")
                            || pbf_str.starts_with("https://")
                        {
                            let filename = pbf_str
                                .rsplit('/')
                                .next()
                                .filter(|s| !s.is_empty())
                                .unwrap_or("download.osm.pbf");
                            let dest = PathBuf::from(filename);
                            info!(url = %pbf_str, dest = %dest.display(), "fetching PBF from URL");
                            http::Client::new(retry).download_to_file(&pbf_str, &dest).await?;
                            dest
                        } else {
                            PathBuf::from(&pbf_str)
                        };

                        let osm_schema = osm_schema::load(&args.osm_schema)?;
                        build::run_osm(
                            &pbf_path,
                            &args.extent,
                            bbox,
                            &osm_schema,
                            &args.output,
                            args.tile_zoom,
                            args.low_memory,
                            args.duckdb_memory_mb,
                            args.progress,
                        )
                        .await?;
                    }

                    // ── Overture path ─────────────────────────────────────────
                    None => {
                        let release = args.release.ok_or_else(|| {
                            anyhow::anyhow!(
                                "one of --roads, --pbf, or --release must be provided"
                            )
                        })?;
                        let client    = http::Client::new(retry);
                        let available = releases::fetch(&client).await?;
                        if !available.contains(&release) {
                            anyhow::bail!(
                                "release '{}' not found. Run `list-releases` to see available releases.",
                                release
                            );
                        }
                        info!(release = %release, extent = %args.extent, "release validated");

                        let schema = schema::load(&args.schema)?;

                        build::run(
                            &release,
                            &args.extent,
                            bbox,
                            &schema,
                            &args.output,
                            &client,
                            args.fetch_concurrency,
                            args.tile_zoom,
                            args.ram_gb,
                            args.bytes_per_segment,
                            args.low_memory,
                        )
                        .await?;
                    }
                }
            }
        }
    }
    Ok(())
}

// ── Merge subcommand ──────────────────────────────────────────────────────────

fn run_merge(args: MergeArgs) -> Result<()> {
    // Resolve each input argument to a concrete .pmtiles path.
    let mut pmtiles_paths: Vec<PathBuf> = Vec::new();
    for input in &args.inputs {
        if input.extension().and_then(|s| s.to_str()) == Some("pmtiles") {
            anyhow::ensure!(input.exists(), "input not found: {}", input.display());
            pmtiles_paths.push(input.clone());
        } else {
            let found = find_pmtiles_in_dir(input)
                .with_context(|| format!("no .pmtiles file found in {}", input.display()))?;
            pmtiles_paths.push(found);
        }
    }

    // Read tile_zoom from sibling manifest.json files; verify all inputs agree.
    let mut tile_zoom: Option<u8> = None;
    for p in &pmtiles_paths {
        let manifest_path = p.with_file_name("manifest.json");
        if let Ok(text) = std::fs::read_to_string(&manifest_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(z) = v["tile_zoom"].as_u64().map(|z| z as u8) {
                    match tile_zoom {
                        None => tile_zoom = Some(z),
                        Some(prev) => anyhow::ensure!(
                            prev == z,
                            "tile_zoom mismatch between inputs ({} vs {}); cannot merge archives built at different zoom levels",
                            prev, z
                        ),
                    }
                }
            }
        }
    }
    let tile_zoom = tile_zoom.unwrap_or(12);

    info!(
        archives  = pmtiles_paths.len(),
        tile_zoom,
        output    = %args.output.display(),
        "merging archives"
    );
    for p in &pmtiles_paths {
        info!(input = %p.display(), "  input archive");
    }

    if let Some(parent) = args.output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create output directory {}", parent.display()))?;
    }

    merge::merge_pmtiles(&pmtiles_paths, &args.output, tile_zoom)?;

    // Write manifest.json alongside the output archive.
    let archive_name = args.output
        .file_name()
        .and_then(|s| s.to_str())
        .context("output path has no filename")?;
    let output_dir = args.output.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    build::write_top_manifest(output_dir, archive_name, "", &args.extent, tile_zoom)?;
    info!(manifest = %output_dir.join("manifest.json").display(), "manifest written");

    Ok(())
}

/// Return the first `.pmtiles` file found directly inside `dir`.
fn find_pmtiles_in_dir(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.find_map(|e| {
        let p = e.ok()?.path();
        if p.extension().and_then(|s| s.to_str()) == Some("pmtiles") { Some(p) } else { None }
    })
}
