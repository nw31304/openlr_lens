use openlr_codec::interval::LinearInterval;
use openlr_graph::{NodeId, SegmentId};

/// Controls how much detail is recorded during a decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum TraceLevel {
    /// No events collected; decode returns the result only. Fastest.
    Off,
    /// Candidates chosen per LRP, routes found/failed, final outcome.
    #[default]
    Summary,
    /// Every candidate evaluated, every A* node expanded. Full replay data.
    Full,
}

// ── Per-candidate data ────────────────────────────────────────────────────────

/// Data derived by projecting an LRP coordinate onto a segment.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProjectionResult {
    /// Arc-length from segment entry to projected point (after endpoint snapping), meters.
    pub arc_offset_m: f64,
    /// Projected point (lon, lat).
    pub point: (f64, f64),
    /// Distance from the LRP coordinate to the projected point, meters.
    pub distance_m: f64,
    /// Bearing computed over the 20 m window at this arc position (degrees).
    pub bearing_deg: f64,
    /// True when the projection was snapped to the segment's entry endpoint.
    pub is_at_entry: bool,
    /// True when the projection was snapped to the segment's exit endpoint.
    pub is_at_exit: bool,
}

/// Additive, decomposable score for one candidate.  Lower is better; 0.0 = perfect match.
///
/// `total = distance_score + bearing_score + frc_score + fow_score
///        + interior_score + wrong_endpoint_score`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CandidateScore {
    /// `distance_weight × (distance_m / search_radius_m)`.
    pub distance_score: f64,
    /// `bearing_weight × (bucket_delta × bearing_penalty_per_bucket)`.
    pub bearing_score: f64,
    /// `frc_weight × frc_penalty_table[lrp_frc][seg_frc]`.
    pub frc_score: f64,
    /// `fow_weight × fow_penalty_table[lrp_fow][seg_fow]`.
    pub fow_score: f64,
    /// `interior_weight × 1.0` when the LRP snapped to an interior point; 0 at endpoints.
    pub interior_score: f64,
    /// `wrong_endpoint_weight × position_along_segment` (0 at correct end, 1 at wrong end).
    pub wrong_endpoint_score: f64,
    /// Sum of all components (what the ranker sorts on).
    pub total: f64,
}

/// Direction in which a candidate segment is traversed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TraversalDir {
    /// From start_node toward end_node.
    Forward,
    /// From end_node toward start_node.
    Backward,
}

/// A candidate that passed all hard gates, with its score.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScoredCandidate {
    pub segment_id: SegmentId,
    pub traversal: TraversalDir,
    pub projection: ProjectionResult,
    pub score: CandidateScore,
    /// The node A* should depart from for this candidate (exit node of the segment).
    pub exit_node: NodeId,
    /// The node the previous leg must arrive at to reach this candidate (entry node).
    pub entry_node: NodeId,
}

/// Summary of a candidate that failed a hard gate (emitted at Summary level).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RejectedCandidate {
    pub segment_id: SegmentId,
    pub traversal: TraversalDir,
    /// Distance from LRP to projected point (available for all gates except FailDirection).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_m: Option<f64>,
    /// Snap point on the segment (lon, lat). Available whenever projection succeeds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<(f64, f64)>,
    /// Measured bearing at projection point (available after radius gate passes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bearing_deg: Option<f64>,
    pub verdict: GateVerdict,
}

// ── Gate verdicts / skip reasons ─────────────────────────────────────────────

/// Why a candidate failed a hard gate.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum GateVerdict {
    Pass,
    FailRadius { distance_m: f64, radius_m: f64 },
    /// Segment geometry was degenerate (fewer than 2 vertices).
    FailDirection,
    /// Bearing deviation exceeded `max_bearing_deviation_deg`.
    FailBearing { excess_deg: f64, max_deg: f64 },
    /// Total score exceeded `max_candidate_score`.
    FailScore { total: f64, max_score: f64 },
}

/// Why an A* edge was skipped.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum SkipReason {
    FrcBelowLfrcnp { seg_frc: u8, lfrcnp: u8 },
    DirectionBlocked,
    TurnRestricted,
    ExceedsMaxDistance { distance_m: f64, max_m: f64 },
}

/// Why routing failed for a candidate pair.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum RoutingFailure {
    NoPathFound,
    DnpOutOfRange { actual_m: f64, window: LinearInterval },
}

// ── Event enum ────────────────────────────────────────────────────────────────

/// One decision point emitted by the decoder.
///
/// `#[non_exhaustive]` allows new variants to be added without breaking
/// existing `match` arms in consumer code.
#[non_exhaustive]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum DecodeEvent {
    // ── Candidate selection ───────────────────────────────────────────────────
    CandidateSearchStarted {
        lrp_idx: usize,
        coord: (f64, f64),
        radius_m: f64,
    },
    /// Emitted for every segment evaluated (Full trace level only).
    CandidateEvaluated {
        lrp_idx: usize,
        segment_id: SegmentId,
        traversal: TraversalDir,
        projection: ProjectionResult,
        verdict: GateVerdict,
        /// Some only when verdict is Pass.
        score: Option<CandidateScore>,
    },
    /// Final ranked set after all candidates evaluated (Summary + Full).
    CandidatesRanked {
        lrp_idx: usize,
        accepted: Vec<ScoredCandidate>,
        rejected: Vec<RejectedCandidate>,
    },

    // ── Routing ──────────────────────────────────────────────────────────────
    RouteSearchStarted {
        leg: usize,
        from: ScoredCandidate,
        to: ScoredCandidate,
    },
    /// Emitted for every A* state popped (Full only).
    AStarNodeExpanded {
        leg: usize,
        node_id: NodeId,
        via_segment: SegmentId,
        g_m: f64,
        h_m: f64,
        /// WGS84 coordinates of this node — needed for map visualization in the replay UI.
        lon: f64,
        lat: f64,
    },
    /// Emitted for every edge the A* skips (Full only).
    AStarEdgeSkipped {
        leg: usize,
        from_node: NodeId,
        segment_id: SegmentId,
        reason: SkipReason,
    },
    /// The engine needs a tile that isn't loaded; caller must inject it.
    TileNeeded {
        tile_key: openlr_graph::TileKey,
    },
    RouteFound {
        leg: usize,
        path: Vec<SegmentId>,
        length_m: f64,
    },
    RouteFailed {
        leg: usize,
        reason: RoutingFailure,
    },

    // ── Validation & offsets ─────────────────────────────────────────────────
    DnpChecked {
        leg: usize,
        interval: LinearInterval,
        actual_m: f64,
        passed: bool,
    },
    OffsetApplied {
        is_positive: bool,
        interval: LinearInterval,
        trim_m: f64,
    },

    DecodeComplete(DecodeOutcome),
}

/// Final decode outcome carried in `DecodeComplete`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum DecodeOutcome {
    Success {
        path: Vec<SegmentId>,
        pos_offset_m: Option<f64>,
        neg_offset_m: Option<f64>,
    },
    NoCandidates { lrp_idx: usize },
    NoRoute { leg: usize },
}

// ── Trace accumulator ─────────────────────────────────────────────────────────

/// Records all decode events and the parameter snapshot used.
/// A decode is: `(location_reference, DecodeParams) → DecodedLocation`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DecodeTrace {
    pub events: Vec<DecodeEvent>,
    /// Snapshot of the parameters that produced this trace (reproducibility).
    pub params: crate::params::DecodeParams,
}

impl DecodeTrace {
    pub fn new(params: crate::params::DecodeParams) -> Self {
        Self { events: Vec::new(), params }
    }

    /// Push an event, respecting the trace level.
    /// `summary_event` — emit at Summary and Full.
    /// `full_event`    — emit at Full only.
    pub fn push_summary(&mut self, event: DecodeEvent) {
        if self.params.trace_level != TraceLevel::Off {
            self.events.push(event);
        }
    }

    pub fn push_full(&mut self, event: DecodeEvent) {
        if self.params.trace_level == TraceLevel::Full {
            self.events.push(event);
        }
    }
}
