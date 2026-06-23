use std::path::PathBuf;
use clap::{ArgAction, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "openlrlens-build", about = "Build OpenLRLens PMTiles from Overture Maps or an OSM PBF file")]
pub struct Cli {
    /// Increase log verbosity: -v = debug, -vv = trace. Overridden by RUST_LOG.
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Maximum HTTP retry attempts.
    #[arg(long, default_value_t = 5, global = true)]
    pub retry_max: u32,

    /// Initial retry backoff in milliseconds.
    #[arg(long, default_value_t = 200, global = true)]
    pub retry_base_ms: u64,

    /// Maximum retry backoff in milliseconds.
    #[arg(long, default_value_t = 30_000, global = true)]
    pub retry_max_ms: u64,

    /// Retry backoff multiplier.
    #[arg(long, default_value_t = 2.0, global = true)]
    pub retry_factor: f64,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List available Overture releases.
    ListReleases,
    /// Build a PMTiles archive for a given release and extent.
    Build(BuildArgs),
    /// Merge multiple regional PMTiles archives into one.
    Merge(MergeArgs),
}

#[derive(clap::Args)]
pub struct BuildArgs {
    /// Generic GeoJSONL(.gz) road network input (alternative to --pbf / --release).
    /// Accepts a single .geojsonl or .geojsonl.gz file, or a directory of such files.
    /// Properties required per feature: id (integer), frc (0-7), fow (0-7),
    /// flowdir (1=both, 2=backward, 3=forward), from_int, to_int (node integers).
    #[arg(long, conflicts_with_all = ["release", "schema", "pbf", "osm_schema"])]
    pub roads: Option<PathBuf>,

    /// Optional turn-restriction CSV for generic input (requires --roads).
    /// Columns: from_segment_id, [via_node_id,] to_segment_id
    /// If via_node_id is omitted, it is derived from the from-segment's to_int.
    #[arg(long, requires = "roads")]
    pub restrictions: Option<PathBuf>,

    /// Human-readable label for the data source, written to manifest.json as
    /// 'external_id_label'. Used by the WebApp when displaying segment source links.
    #[arg(long, default_value = "")]
    pub label: String,

    /// OSM PBF file or URL to build from (e.g. a Geofabrik regional extract).
    /// If a https:// URL is given, the file is downloaded to the current directory first.
    /// Use this instead of --release to build from OSM instead of Overture.
    #[arg(long, conflicts_with_all = ["release", "schema"])]
    pub pbf: Option<String>,

    /// OSM → OpenLR attribute mapping TOML file (OSM source only).
    #[arg(long, default_value = "pipeline/schema/osm-default.toml", requires = "pbf")]
    pub osm_schema: PathBuf,

    /// Overture release, e.g. 2026-05-20.0.
    /// Use this instead of --pbf to build from Overture parquet.
    #[arg(long)]
    pub release: Option<String>,

    /// Extent: ISO 3166-1 alpha-2 country code (NZ), continent name (oceania),
    /// 'world', explicit bbox 'west,south,east,north', or any label string (generic
    /// input only — used purely as an archive filename slug, no spatial filtering).
    #[arg(long)]
    pub extent: String,

    /// Overture → OpenLR attribute mapping TOML file (Overture source only).
    #[arg(long, default_value = "pipeline/schema/overture-default.toml")]
    pub schema: PathBuf,

    /// Output directory for the PMTiles archive and manifest.
    #[arg(long, default_value = "out")]
    pub output: PathBuf,

    /// Rayon CPU worker threads for parallel processing. Defaults to logical CPU count.
    #[arg(short = 'j', long)]
    pub jobs: Option<usize>,

    /// Maximum concurrent HTTP parquet file downloads (Overture source only).
    #[arg(long, default_value_t = 8)]
    pub fetch_concurrency: usize,

    /// Slippy tile zoom level for the output PMTiles archive.
    /// Determines tile cell size (~10 km at z12). Single level only — not a pyramid.
    /// Tune by measuring fetch sizes against your candidate search radius.
    #[arg(long, default_value_t = 12, value_parser = clap::value_parser!(u8).range(8..=15))]
    pub tile_zoom: u8,

    /// Override automatic RAM detection. Sets the per-partition memory budget.
    /// Example: --ram-gb 20 reserves 20 GiB for data processing.
    /// If omitted, the pipeline detects available system RAM and uses 75 % of it.
    #[arg(long)]
    pub ram_gb: Option<f64>,

    /// Peak RAM estimate per Overture segment (bytes). Increase if the pipeline OOMs;
    /// decrease to allow fewer, larger partitions on machines with ample RAM.
    #[arg(long, default_value_t = crate::partition::DEFAULT_BYTES_PER_SEGMENT)]
    pub bytes_per_segment: u64,

    /// Use disk-backed tile buffering (DuckDB) instead of accumulating all tile
    /// payloads in RAM. Recommended when building large regions (continents, world)
    /// on machines with less than 32 GB RAM. Correct output either way; slower due
    /// to I/O overhead on the tile stage.
    #[arg(long)]
    pub low_memory: bool,
}

#[derive(clap::Args)]
pub struct MergeArgs {
    /// Input PMTiles archives or directories containing exactly one .pmtiles file each.
    /// Mix and match: paths ending in .pmtiles are used directly; directories are searched
    /// for a single .pmtiles file.
    #[arg(required = true, num_args = 1..)]
    pub inputs: Vec<PathBuf>,

    /// Output path for the merged PMTiles archive (e.g. out/world/world.pmtiles).
    #[arg(long)]
    pub output: PathBuf,

    /// Extent label written to the output manifest.json [default: world].
    #[arg(long, default_value = "world")]
    pub extent: String,
}
