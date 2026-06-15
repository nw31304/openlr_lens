use crate::trace::TraceLevel;

/// Pre-tuned parameter presets. Individual fields can be overridden after construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Preset {
    /// Wide tolerances; use when references are old or from a different map version.
    Permissive,
    /// Balanced defaults; good starting point.
    Default,
    /// Tight tolerances; use when references are fresh and you trust the map.
    Strict,
}

/// Decode-time configuration.  Exposed to the UI; all fields are independently tunable.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DecodeParams {
    // ── Spatial ──────────────────────────────────────────────────────────────
    /// Candidate search radius around each LRP, meters.
    pub candidate_search_radius_m: f64,

    // ── Bearing ──────────────────────────────────────────────────────────────
    /// Map-divergence bearing tolerance τ (degrees).
    /// Combined with the encoding interval: hard window = `[LB−τ, UB+τ]`.
    pub bearing_tolerance_deg: f64,

    // ── Distance / DNP ───────────────────────────────────────────────────────
    /// DNP tolerance δ as a fraction of expected path length (e.g. 0.25 = 25 %).
    /// Combined with the v3 bucket half-width for the hard window.
    pub dnp_tolerance_pct: f64,

    // ── Soft ranking weights ─────────────────────────────────────────────────
    /// Penalty per FRC step of mismatch (added to candidate score).
    pub frc_penalty_per_step: f64,
    /// Penalty for any FOW mismatch (added to candidate score).
    pub fow_penalty: f64,

    // ── Candidate set ────────────────────────────────────────────────────────
    /// Maximum candidates to retain per LRP after scoring (best-first).
    /// Limits RouteGenerator search space from O(N^L) to O(K^L).
    /// 0 = unlimited (may be very slow on dense maps).
    pub max_candidates_per_lrp: usize,

    // ── A* ───────────────────────────────────────────────────────────────────
    /// A* expansion cap: maximum ratio of expanded distance to expected DNP.
    pub max_path_search_factor: f64,
    /// Hard cap on A* node expansions per leg.  Prevents runaway search on
    /// large graphs when the route is genuinely missing from the loaded tiles.
    /// 0 means unlimited (use only max_path_search_factor).
    pub max_astar_expansions: usize,
    /// Extra FRC steps added to the encoded LFRCNP floor before passing to A*.
    /// Compensates for FRC mapping differences between encoder and decoder maps.
    /// 0 = strict; 1 = allow one step worse (recommended for cross-map decoding).
    pub lfrcnp_tolerance: u8,

    // ── Trace ────────────────────────────────────────────────────────────────
    /// How much detail to record in the decode trace.
    pub trace_level: TraceLevel,
}

impl DecodeParams {
    pub fn preset(p: Preset) -> Self {
        match p {
            Preset::Permissive => Self {
                candidate_search_radius_m: 200.0,
                bearing_tolerance_deg:      45.0,
                dnp_tolerance_pct:           0.40,
                frc_penalty_per_step:        15.0,
                fow_penalty:                 15.0,
                max_candidates_per_lrp:       10,
                max_path_search_factor:       4.0,
                max_astar_expansions:     50_000,
                // Overture FRC mapping can be 2 steps coarser than TomTom (e.g.
                // TomTom FRC=4 = our frc=6 for "service" roads in mountain areas).
                lfrcnp_tolerance:              2,
                trace_level: TraceLevel::Summary,
            },
            Preset::Default => Self::default(),
            Preset::Strict => Self {
                candidate_search_radius_m:  50.0,
                bearing_tolerance_deg:      15.0,
                dnp_tolerance_pct:           0.10,
                frc_penalty_per_step:        30.0,
                fow_penalty:                 30.0,
                max_candidates_per_lrp:        5,
                max_path_search_factor:       3.0,
                max_astar_expansions:          0,
                lfrcnp_tolerance:              0,
                trace_level: TraceLevel::Summary,
            },
        }
    }
}

impl Default for DecodeParams {
    fn default() -> Self {
        Self {
            candidate_search_radius_m: 100.0,
            bearing_tolerance_deg:      30.0,
            dnp_tolerance_pct:           0.25,
            frc_penalty_per_step:        25.0,
            fow_penalty:                 25.0,
            max_candidates_per_lrp:        8,
            max_path_search_factor:       5.0,
            max_astar_expansions:     100_000,
            lfrcnp_tolerance:              0,
            trace_level: TraceLevel::Summary,
        }
    }
}
