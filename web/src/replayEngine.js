/**
 * Replay engine: converts a flat DecodeTrace.events array into display steps
 * and computes the visual state (GeoJSON inputs) for any step index.
 *
 * The trace events use Rust serde's externally-tagged format:
 *   { "CandidateSearchStarted": { lrp_idx, coord, radius_m } }
 *   { "AStarNodeExpanded": { leg, node_id, via_segment, g_m, h_m, lon, lat } }
 *   etc.
 */

// ── Colour utilities ──────────────────────────────────────────────────────────

/** Map t ∈ [0,1] to a CSS hex colour on a blue→cyan→yellow→red ramp. */
function nodeColorAt(t) {
  t = Math.max(0, Math.min(1, t));
  let r, g, b;
  if (t < 0.33) {
    const u = t / 0.33;
    r = 0;  g = Math.round(100 + 155 * u);  b = 255;
  } else if (t < 0.66) {
    const u = (t - 0.33) / 0.33;
    r = Math.round(255 * u);  g = 255;  b = Math.round(255 * (1 - u));
  } else {
    const u = (t - 0.66) / 0.34;
    r = 255;  g = Math.round(255 * (1 - u));  b = 0;
  }
  return '#' + [r, g, b].map(v => v.toString(16).padStart(2, '0')).join('');
}

/** Verdict type string from a serde-tagged GateVerdict. */
function verdictType(verdict) {
  if (!verdict || verdict === 'Pass') return 'pass';
  if (typeof verdict === 'string') return verdict.toLowerCase();
  const key = Object.keys(verdict)[0];
  switch (key) {
    case 'FailBearing':   return 'bearing';
    case 'FailRadius':    return 'radius';
    case 'FailScore':     return 'score';
    case 'FailDirection': return 'direction';
    default:              return 'other';
  }
}

// ── Step builder ─────────────────────────────────────────────────────────────

/** Nodes per display step. 1 = one A* expansion per step (shows the wavefront growing). */
const ASTAR_BATCH = 1;

/**
 * Convert a flat trace events array into a richer, display-oriented step list.
 * Also computes stats (maxG, totalNodes) for normalizing A* colours.
 */
export function buildReplaySteps(events) {
  if (!events?.length) return { steps: [], stats: { maxG: 0, totalNodes: 0, phases: [] } };

  const steps   = [];
  const phases  = [];   // { label, startStep, color }
  let maxG      = 0;
  let totalNodes = 0;

  let i = 0;
  while (i < events.length) {
    const ev  = events[i];
    const key = Object.keys(ev)[0];
    const d   = ev[key];

    switch (key) {
      case 'CandidateSearchStarted':
        phases.push({ label: `LRP ${d.lrp_idx}`, startStep: steps.length, color: '#aa44ff' });
        steps.push({ type: 'search_started', lrp_idx: d.lrp_idx, coord: d.coord, radius_m: d.radius_m });
        i++;
        break;

      case 'CandidateEvaluated':
        // Full-mode per-candidate events — skip (CandidatesRanked has the summary)
        i++;
        break;

      case 'CandidatesRanked':
        steps.push({ type: 'candidates_ranked', lrp_idx: d.lrp_idx, accepted: d.accepted, rejected: d.rejected ?? [] });
        i++;
        break;

      case 'RouteSearchStarted':
        phases.push({ label: `Leg ${d.leg} A*`, startStep: steps.length, color: '#0088ff' });
        steps.push({ type: 'route_search_started', leg: d.leg, from: d.from, to: d.to });
        i++;
        break;

      case 'AStarNodeExpanded': {
        // Batch consecutive A* events into groups of ASTAR_BATCH
        const leg = d.leg;
        let batch = [];
        while (i < events.length && events[i].AStarNodeExpanded) {
          const nd = events[i].AStarNodeExpanded;
          if (nd.g_m > maxG) maxG = nd.g_m;
          totalNodes++;
          batch.push({ lon: nd.lon, lat: nd.lat, g_m: nd.g_m, h_m: nd.h_m, node_id: nd.node_id });
          i++;
          if (batch.length >= ASTAR_BATCH) {
            steps.push({ type: 'astar_batch', leg, nodes: batch });
            batch = [];
          }
        }
        if (batch.length > 0) {
          steps.push({ type: 'astar_batch', leg, nodes: batch });
        }
        break;
      }

      case 'AStarEdgeSkipped':
        i++;
        break;

      case 'RouteFound':
        steps.push({ type: 'route_found', leg: d.leg, path: d.path, length_m: d.length_m });
        i++;
        break;

      case 'RouteFailed':
        steps.push({ type: 'route_failed', leg: d.leg, reason: d.reason });
        i++;
        break;

      case 'DnpChecked':
        steps.push({ type: 'dnp_checked', leg: d.leg, actual_m: d.actual_m, interval: d.interval, passed: d.passed });
        i++;
        break;

      case 'OffsetApplied':
        steps.push({ type: 'offset_applied', is_positive: d.is_positive, trim_m: d.trim_m, interval: d.interval });
        i++;
        break;

      case 'DecodeComplete':
        phases.push({ label: 'Done', startStep: steps.length, color: d.Success ? '#00ff88' : '#ff4444' });
        steps.push({ type: 'decode_complete', outcome: d });
        i++;
        break;

      default:
        i++;
    }
  }

  return { steps, stats: { maxG, totalNodes, phases } };
}

// ── Visual state ─────────────────────────────────────────────────────────────

function emptyState() {
  return {
    searchRadius:  null,    // { lon, lat, radiusM, lrpIdx }
    candidates:    [],      // { lon, lat, ctype, score, lrpIdx, segmentId, winner }
    astarNodes:    [],      // { lon, lat, gM, hM, color }
    frontier:      [],      // last FRONTIER_SIZE nodes
    currentLeg:    null,    // { leg, fromPt, toPt, fromSegId, toSegId }
    routeSegIds:   [],      // segment IDs of current best route
    maxG:          0,
    statusText:    'Ready to replay',
    phase:         'idle',
    stepType:      '',
    stepIdx:       -1,
  };
}

const FRONTIER_SIZE = 25;

/** Apply one display step onto a mutable visual state. */
function applyStep(s, step, maxGTotal) {
  s.stepType = step.type;

  switch (step.type) {
    case 'search_started':
      s.phase = 'candidates';
      s.searchRadius = { lon: step.coord[0], lat: step.coord[1], radiusM: step.radius_m, lrpIdx: step.lrp_idx };
      s.statusText   = `LRP ${step.lrp_idx} — searching within ${step.radius_m.toFixed(0)} m`;
      // Reset A* state from previous legs
      s.astarNodes   = [];
      s.frontier     = [];
      s.routeSegIds  = [];
      s.currentLeg   = null;
      break;

    case 'candidates_ranked': {
      s.searchRadius = null;
      const acc = step.accepted ?? [];
      const rej = step.rejected ?? [];
      for (const c of acc) {
        s.candidates.push({
          lon: c.projection.point[0], lat: c.projection.point[1],
          ctype: 'accepted',
          lrpIdx: step.lrp_idx,
          segmentId: c.segment_id,
          winner: false,
          // Full details for the popup
          traversal:     c.traversal ?? null,
          distance_m:    c.projection.distance_m,
          arc_offset_m:  c.projection.arc_offset_m,
          bearing_deg:   c.projection.bearing_deg,
          score_total:        c.score.total,
          score_distance:     c.score.distance_score,
          score_bearing:      c.score.bearing_score,
          score_frc:          c.score.frc_score,
          score_fow:          c.score.fow_score,
          score_wrong_ep:     c.score.wrong_endpoint_score,
          score_interior:     c.score.interior_score,
        });
      }
      for (const r of rej) {
        if (r.point) {
          s.candidates.push({
            lon: r.point[0], lat: r.point[1],
            ctype: verdictType(r.verdict),
            lrpIdx: step.lrp_idx,
            segmentId: r.segment_id ?? null,
            winner: false,
            // Rejection details
            verdict_json:  JSON.stringify(r.verdict ?? null),
            bearing_deg:   r.bearing_deg ?? null,
            distance_m:    r.distance_m  ?? null,
          });
        }
      }
      s.statusText = `LRP ${step.lrp_idx} — ${acc.length} accepted, ${rej.length} rejected`;
      break;
    }

    case 'route_search_started': {
      s.phase      = 'routing';
      s.currentLeg = {
        leg:       step.leg,
        fromPt:    step.from.projection.point,
        toPt:      step.to.projection.point,
        fromSegId: step.from.segment_id,
        toSegId:   step.to.segment_id,
      };
      s.astarNodes = [];
      s.frontier   = [];
      s.maxG       = 0;
      s.statusText = `Leg ${step.leg} — A* search started`;
      // Mark the chosen from/to candidates as winners so they render distinctly.
      const winIds = new Set([step.from.segment_id, step.to.segment_id]);
      for (const c of s.candidates) {
        if (winIds.has(c.segmentId)) c.winner = true;
      }
      break;
    }

    case 'astar_batch': {
      for (const n of step.nodes) {
        const color = nodeColorAt(maxGTotal > 0 ? n.g_m / maxGTotal : 0);
        s.astarNodes.push({ lon: n.lon, lat: n.lat, gM: n.g_m, hM: n.h_m, color });
        if (n.g_m > s.maxG) s.maxG = n.g_m;
      }
      s.frontier = s.astarNodes.slice(-FRONTIER_SIZE);
      const last = step.nodes[step.nodes.length - 1];
      s.statusText = `Leg ${step.leg} — A* · ${s.astarNodes.length} nodes · g=${last.g_m.toFixed(0)}m h=${last.h_m.toFixed(0)}m`;
      break;
    }

    case 'route_found':
      s.routeSegIds = step.path;
      s.frontier    = [];
      s.statusText  = `Leg ${step.leg} — route found · ${step.length_m.toFixed(0)} m · ${step.path.length} seg${step.path.length !== 1 ? 's' : ''}`;
      break;

    case 'route_failed':
      s.frontier   = [];
      s.statusText = `Leg ${step.leg} — route FAILED`;
      break;

    case 'dnp_checked': {
      const lb = step.interval?.lb ?? 0, ub = step.interval?.ub ?? 0;
      s.statusText = `Leg ${step.leg} — DNP ${step.actual_m.toFixed(0)} m ∈ [${lb.toFixed(0)}, ${ub.toFixed(0)}] ${step.passed ? '✓' : '✗'}`;
      break;
    }

    case 'offset_applied':
      s.phase      = 'trimming';
      s.statusText = `${step.is_positive ? 'Positive' : 'Negative'} offset — trim ${step.trim_m.toFixed(0)} m`;
      break;

    case 'decode_complete': {
      s.phase     = 'complete';
      const o     = step.outcome;
      if (o.Success)        s.statusText = `✓ Complete — ${o.Success.path.length} segments`;
      else if (o.NoCandidates) s.statusText = `✗ No candidates for LRP ${o.NoCandidates.lrp_idx}`;
      else if (o.NoRoute)   s.statusText = `✗ No route for leg ${o.NoRoute.leg}`;
      break;
    }
  }
}

/** Exported so Map.jsx can do incremental updates without re-walking from step 0. */
export { emptyState, applyStep };

/**
 * Compute the full visual state at displayStep N by walking from step 0.
 * Use this only for the initial frame or when jumping backward.
 * For forward steps, call applyStep() directly on the existing state.
 */
export function computeVisualState(steps, stepIdx, stats) {
  const state = emptyState();
  const maxG  = stats?.maxG ?? 0;
  const limit = Math.min(stepIdx, steps.length - 1);
  for (let i = 0; i <= limit; i++) {
    applyStep(state, steps[i], maxG);
    state.stepIdx = i;
  }
  return state;
}

// ── GeoJSON builders ─────────────────────────────────────────────────────────

/** Build a circle polygon approximation (64 points) for the search-radius ring. */
function circlePolygon(lon, lat, radiusM, steps = 64) {
  const R = 6371000;
  const φ = lat * Math.PI / 180;
  const coords = [];
  for (let i = 0; i <= steps; i++) {
    const θ = (i / steps) * 2 * Math.PI;
    const dφ = (radiusM / R) * Math.cos(θ);
    const dλ = (radiusM / R) * Math.sin(θ) / Math.cos(φ);
    coords.push([lon + dλ * 180 / Math.PI, lat + dφ * 180 / Math.PI]);
  }
  return { type: 'Polygon', coordinates: [coords] };
}

/** Convert a visual state to a set of GeoJSON FeatureCollections for all replay sources. */
export function stateToGeoJSON(state) {
  const empty = { type: 'FeatureCollection', features: [] };

  // Search radius
  const radiusFC = state.searchRadius
    ? {
        type: 'FeatureCollection',
        features: [{
          type: 'Feature',
          geometry: circlePolygon(state.searchRadius.lon, state.searchRadius.lat, state.searchRadius.radiusM),
          properties: { lrp_idx: state.searchRadius.lrpIdx },
        }],
      }
    : empty;

  // Candidates — all detail fields flattened so the click popup can read them
  const candFC = {
    type: 'FeatureCollection',
    features: state.candidates.map(c => ({
      type: 'Feature',
      geometry: { type: 'Point', coordinates: [c.lon, c.lat] },
      properties: {
        ctype:         c.ctype,
        lrp_idx:       c.lrpIdx,
        winner:        c.winner ?? false,
        segment_id:    c.segmentId ?? null,
        traversal:     c.traversal ?? null,
        distance_m:    c.distance_m  ?? null,
        arc_offset_m:  c.arc_offset_m ?? null,
        bearing_deg:   c.bearing_deg  ?? null,
        score_total:       c.score_total    ?? null,
        score_distance:    c.score_distance ?? null,
        score_bearing:     c.score_bearing  ?? null,
        score_frc:         c.score_frc      ?? null,
        score_fow:         c.score_fow      ?? null,
        score_wrong_ep:    c.score_wrong_ep ?? null,
        score_interior:    c.score_interior ?? null,
        verdict_json:      c.verdict_json   ?? null,
      },
    })),
  };

  // A* node cloud (older, faded)
  const cloudFC = {
    type: 'FeatureCollection',
    features: state.astarNodes.map((n, i) => ({
      type: 'Feature',
      geometry: { type: 'Point', coordinates: [n.lon, n.lat] },
      properties: { color: n.color, g_m: n.gM, h_m: n.hM, idx: i },
    })),
  };

  // Frontier (latest N nodes, bright)
  const frontierSet = new Set(state.frontier.map(n => `${n.lon},${n.lat}`));
  const frontierFC = {
    type: 'FeatureCollection',
    features: state.frontier.map(n => ({
      type: 'Feature',
      geometry: { type: 'Point', coordinates: [n.lon, n.lat] },
      properties: { g_m: n.gM, h_m: n.hM },
    })),
  };

  // "From" and "to" endpoint markers for current leg
  const legFC = state.currentLeg
    ? {
        type: 'FeatureCollection',
        features: [
          { type: 'Feature', geometry: { type: 'Point', coordinates: state.currentLeg.fromPt }, properties: { role: 'from' } },
          { type: 'Feature', geometry: { type: 'Point', coordinates: state.currentLeg.toPt },   properties: { role: 'to' } },
        ],
      }
    : empty;

  return { radiusFC, candFC, cloudFC, frontierFC, legFC };
}
