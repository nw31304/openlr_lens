import { chatComplete } from './llmClient.js';
import { SYSTEM_PROMPT } from './llm/systemPrompt.js';

// ── Constants ─────────────────────────────────────────────────────────────────

const FOW = ['undef', 'motorway', 'dual_C/W', 'single_C/W', 'roundabout', 'traffic_sq', 'slip_rd', 'other'];
const FRC = ['FRC0/motorway', 'FRC1/trunk', 'FRC2/secondary', 'FRC3/tertiary',
             'FRC4/unclassified', 'FRC5/residential', 'FRC6/service', 'FRC7/other'];

// ── Helpers ───────────────────────────────────────────────────────────────────

function haversineM(lat1, lon1, lat2, lon2) {
  const R  = 6_371_000;
  const φ1 = lat1 * Math.PI / 180, φ2 = lat2 * Math.PI / 180;
  const Δφ = (lat2 - lat1) * Math.PI / 180;
  const Δλ = (lon2 - lon1) * Math.PI / 180;
  const a  = Math.sin(Δφ / 2) ** 2 + Math.cos(φ1) * Math.cos(φ2) * Math.sin(Δλ / 2) ** 2;
  return R * 2 * Math.atan2(Math.sqrt(a), Math.sqrt(1 - a));
}

// ── Trace event extractor (lean version for LLM formatting) ──────────────────

function extractTrace(events) {
  const candidates = {};
  const routing    = {};
  for (const ev of events ?? []) {
    const [type, data] = Object.entries(ev)[0];
    switch (type) {
      case 'CandidatesRanked':
        candidates[data.lrp_idx] = data;
        break;
      case 'RouteSearchStarted':
        (routing[data.leg] ??= {}).start = data;
        break;
      case 'RouteFound':
        (routing[data.leg] ??= {}).result = { found: true, ...data };
        break;
      case 'RouteFailed':
        (routing[data.leg] ??= {}).result = { found: false, ...data };
        break;
      case 'DnpChecked':
        (routing[data.leg] ??= {}).dnp = data;
        break;
      case 'AStarTerminated':
        (routing[data.leg] ??= {}).astar = data;
        break;
    }
  }
  return { candidates, routing };
}

// ── Prompt builder ────────────────────────────────────────────────────────────

export function buildDiagnosticPrompt(decodeResult, params) {
  const lines = [];
  const { candidates, routing } = extractTrace(decodeResult.trace?.events);
  const lfrcnpTol = params?.lfrcnp_tolerance ?? 0;

  // Header
  if (decodeResult.openlr_string) lines.push(`OpenLR: ${decodeResult.openlr_string}`);
  if (decodeResult.ok) {
    const segs = decodeResult.segments ?? [];
    lines.push(`Result: SUCCESS — ${segs.length} segment${segs.length !== 1 ? 's' : ''}`);
    if (decodeResult.pos_offset_ub > 0) {
      const lb = decodeResult.pos_offset_lb, ub = decodeResult.pos_offset_ub;
      lines.push(`  positive offset: ${lb === ub ? `${lb.toFixed(1)} m` : `[${lb.toFixed(1)}, ${ub.toFixed(1)}] m`}`);
    }
    if (decodeResult.neg_offset_ub > 0) {
      const lb = decodeResult.neg_offset_lb, ub = decodeResult.neg_offset_ub;
      lines.push(`  negative offset: ${lb === ub ? `${lb.toFixed(1)} m` : `[${lb.toFixed(1)}, ${ub.toFixed(1)}] m`}`);
    }
    if (segs.length > 0) {
      lines.push('Decoded path (ordered segments):');
      for (const s of segs) {
        lines.push(`  seg=${s.source_id ?? s.segment_id}  frc=${s.frc}(${FRC[s.frc] ?? s.frc})  fow=${s.fow}(${FOW[s.fow] ?? s.fow})`);
      }
    }
  } else {
    lines.push(`Result: FAILED — ${decodeResult.error ?? 'unknown error'}`);
  }
  lines.push('');

  // LRPs
  for (let i = 0; i < (decodeResult.lrps?.length ?? 0); i++) {
    const l = decodeResult.lrps[i];
    const isLast = i === decodeResult.lrps.length - 1;

    const bearStr = Math.abs(l.bearing_ub - l.bearing_lb) < 0.5
      ? `${l.bearing_lb.toFixed(1)}°`
      : `${l.bearing_lb.toFixed(1)}–${l.bearing_ub.toFixed(1)}°`;

    const lfrcEff = lfrcnpTol > 0 && l.lfrcnp != null
      ? `${l.lfrcnp}→${Math.min(l.lfrcnp + lfrcnpTol, 7)}`
      : (l.lfrcnp != null ? String(l.lfrcnp) : null);

    const dnpStr = l.dnp_lb != null
      ? (Math.abs((l.dnp_ub ?? l.dnp_lb) - l.dnp_lb) < 1
          ? `${l.dnp_lb.toFixed(0)} m`
          : `${l.dnp_lb.toFixed(0)}–${(l.dnp_ub ?? l.dnp_lb).toFixed(0)} m`)
      : null;

    const attrs = [
      `bear=${bearStr}`,
      `frc=${l.frc}(${FRC[l.frc] ?? l.frc})`,
      `fow=${l.fow}(${FOW[l.fow] ?? l.fow})`,
      lfrcEff != null && !isLast ? `lfrcnp=${lfrcEff}` : null,
      dnpStr && !isLast ? `dnp=${dnpStr}` : null,
    ].filter(Boolean).join('  ');

    lines.push(`LRP ${i}${isLast ? ' [last]' : ''}  ${l.lat.toFixed(5)},${l.lon.toFixed(5)}`);
    lines.push(`  ${attrs}`);

    const cands = candidates[i];
    if (cands) {
      const accepted = cands.accepted ?? [];
      const rejected = cands.rejected ?? [];
      for (const c of accepted.slice(0, 3)) {
        const scoreBreakdown = [
          c.score.distance_score > 0 ? `dist+${c.score.distance_score.toFixed(3)}` : null,
          c.score.bearing_score  > 0 ? `bear+${c.score.bearing_score.toFixed(3)}`  : null,
          c.score.frc_score      > 0 ? `frc+${c.score.frc_score.toFixed(3)}`       : null,
          c.score.fow_score      > 0 ? `fow+${c.score.fow_score.toFixed(3)}`       : null,
        ].filter(Boolean).join(' ');
        lines.push(`  ✓ seg=${c.segment_id} ${c.traversal}  dist=${c.projection.distance_m.toFixed(1)}m  bear=${c.projection.bearing_deg.toFixed(1)}°  score=${c.score.total.toFixed(3)}${scoreBreakdown ? ` (${scoreBreakdown})` : ''}`);
      }
      if (accepted.length > 3) lines.push(`  … ${accepted.length - 3} more accepted`);

      // Show up to 3 rejected with reasons
      for (const r of rejected.slice(0, 3)) {
        const reason = r.verdict ? fmtVerdict(r.verdict) : '?';
        lines.push(`  ✗ seg=${r.segment_id}  dist=${r.projection?.distance_m?.toFixed(1) ?? '?'}m  ${reason}`);
      }
      if (rejected.length > 3) lines.push(`  … ${rejected.length - 3} more rejected`);
      if (accepted.length === 0 && rejected.length === 0) lines.push(`  (no candidates generated — outside search radius or no segments loaded)`);
    } else {
      lines.push(`  (no candidate data in trace)`);
    }

    // Leg routing (between this LRP and next)
    if (!isLast) {
      const leg = routing[i];
      if (leg) {
        if (leg.result?.found) {
          const path = leg.result.path;
          const pathIds = Array.isArray(path) && path.length > 0
            ? path.slice(0, 20).join(', ') + (path.length > 20 ? ` … +${path.length - 20}` : '')
            : null;
          lines.push(`  → route: ${path?.length ?? '?'} segs, ${leg.result.length_m?.toFixed(0) ?? '?'} m${pathIds ? `  [${pathIds}]` : ''}`);
        } else if (leg.result && !leg.result.found) {
          lines.push(`  → route: FAILED — ${fmtRouteFail(leg.result.reason)}`);
        }
        if (leg.dnp) {
          const d = leg.dnp;
          const lb = Math.max(0, d.interval?.lb ?? 0).toFixed(0);
          const ub = (d.interval?.ub ?? 0).toFixed(0);
          lines.push(`  DNP ${d.actual_m?.toFixed(0) ?? '?'} m ${d.passed ? '∈' : '∉'} [${lb}, ${ub}] m ${d.passed ? '✓' : '✗'}`);
        }
        if (leg.astar) {
          const t = leg.astar;
          lines.push(`  A*: ${t.nodes_expanded} nodes expanded  skipped: frc=${t.edges_skipped_frc} dir=${t.edges_skipped_direction} turn=${t.edges_skipped_turn} dist=${t.edges_skipped_distance}`);
        }
      }
    }
    lines.push('');
  }

  // Params summary — use descriptive labels so the model's suggestions are human-readable
  lines.push('Active parameters:');
  lines.push([
    `  search radius: ${params?.candidate_search_radius_m ?? '?'} m`,
    `  bearing tolerance: ${params?.max_bearing_deviation_deg ?? '?'}°`,
    `  DNP tolerance: ${((params?.dnp_tolerance_pct ?? 0.1) * 100).toFixed(0)}%`,
    `  LFRCNP tolerance: ${lfrcnpTol} FRC steps (0 = strict, 7 = fully permissive)`,
    `  max candidate score: ${params?.max_candidate_score ?? '?'}`,
  ].join('\n'));

  // Primary signal summary — surface the most diagnostically significant facts
  // so the model doesn't have to infer them from scattered numbers.
  const signals = [];
  for (const [leg, info] of Object.entries(routing)) {
    if (info.astar) {
      const t = info.astar;
      const totalSkipped = (t.edges_skipped_frc ?? 0) + (t.edges_skipped_direction ?? 0) +
                           (t.edges_skipped_turn ?? 0) + (t.edges_skipped_distance ?? 0);
      if (t.edges_skipped_frc > 0 && t.edges_skipped_frc >= (t.nodes_expanded ?? 0)) {
        signals.push(`Leg ${leg}: FRC skips (${t.edges_skipped_frc}) >= nodes expanded (${t.nodes_expanded ?? 0}) — LFRCNP floor is blocking the search`);
      } else if (t.edges_skipped_frc > 0) {
        signals.push(`Leg ${leg}: ${t.edges_skipped_frc} of ${totalSkipped} total skipped edges were due to LFRCNP floor`);
      }
      if ((t.nodes_expanded ?? 0) < 10 && !info.result?.found) {
        signals.push(`Leg ${leg}: only ${t.nodes_expanded ?? 0} nodes expanded before failure — graph is nearly disconnected at current LFRCNP`);
      }
    }
    if (info.dnp && !info.dnp.passed) {
      signals.push(`Leg ${leg}: DNP check FAILED — actual ${info.dnp.actual_m?.toFixed(0)}m outside window [${Math.max(0, info.dnp.interval?.lb ?? 0).toFixed(0)}, ${(info.dnp.interval?.ub ?? 0).toFixed(0)}]m`);
    }
    if (info.dnp?.passed && info.dnp.actual_m != null && info.dnp.actual_m < 10) {
      signals.push(`Leg ${leg}: routed path is only ${info.dnp.actual_m.toFixed(1)}m — both LRP anchors likely snapped to the same map location (missing connector segment in decoding map)`);
    }
  }

  // Offset overflow: combined offsets must not reach or exceed the decoded path length.
  // This can only be checked in JS because the Rust engine now rejects such references
  // before returning ok:true, but older results or edge cases may slip through.
  if (decodeResult.ok) {
    const pathTotalM = (decodeResult.segments ?? []).reduce((sum, s) => sum + (s.length_m ?? 0), 0);
    const posLb = decodeResult.pos_offset_lb ?? 0;
    const negLb = decodeResult.neg_offset_lb ?? 0;
    const posUb = decodeResult.pos_offset_ub ?? 0;
    const negUb = decodeResult.neg_offset_ub ?? 0;
    if ((posLb > 0 || negLb > 0) && posLb + negLb >= pathTotalM) {
      signals.push(
        `OFFSET OVERFLOW: combined offsets (${posLb.toFixed(0)}+${negLb.toFixed(0)} = ${(posLb + negLb).toFixed(0)} m) ` +
        `≥ path length (${pathTotalM.toFixed(0)} m) — trimmed location has zero or negative length; reference is malformed`
      );
    } else if ((posUb > 0 || negUb > 0) && posUb + negUb >= pathTotalM) {
      signals.push(
        `OFFSET WARNING: combined offset upper bounds (${posUb.toFixed(0)}+${negUb.toFixed(0)} = ${(posUb + negUb).toFixed(0)} m) ` +
        `≥ path length (${pathTotalM.toFixed(0)} m) — trimmed location may be zero-length`
      );
    }
  }

  // Adjacent LRP proximity (encoded coordinates)
  for (let i = 0; i < (decodeResult.lrps?.length ?? 0) - 1; i++) {
    const a = decodeResult.lrps[i], b = decodeResult.lrps[i + 1];
    if (!a || !b) continue;
    const dist = haversineM(a.lat, a.lon, b.lat, b.lon);
    if (dist < 25) {
      signals.push(`LRP ${i} and LRP ${i + 1} are only ${dist.toFixed(1)}m apart in encoded coordinates — risk of same-point snapping`);
    }
  }

  if (signals.length > 0) {
    lines.push('');
    lines.push('Key signals:');
    for (const s of signals) lines.push(`  !! ${s}`);
  }

  return lines.join('\n');
}

function fmtVerdict(verdict) {
  if (!verdict || verdict === 'Pass') return 'pass';
  if (typeof verdict === 'string') return verdict;
  const key = Object.keys(verdict)[0];
  const val = verdict[key];
  switch (key) {
    case 'FailBearing': return `bearing exceeded by ${val?.excess_deg?.toFixed(1) ?? '?'}°`;
    case 'FailRadius':  return 'outside search radius';
    case 'FailScore':   return `score ${val?.score?.toFixed(3) ?? '?'} > max`;
    case 'FailDirection': return 'wrong direction (one-way)';
    default: return key;
  }
}

function fmtRouteFail(reason) {
  if (!reason) return 'unknown';
  if (typeof reason === 'string') return reason;
  const key = Object.keys(reason)[0];
  return key;
}

// ── System context builder (for multi-turn chat) ─────────────────────────────

export function buildSystemContext(decodeResult, params) {
  return `${SYSTEM_PROMPT}\n\nCurrent decode data:\n${buildDiagnosticPrompt(decodeResult, params)}`;
}

// ── LLM call ──────────────────────────────────────────────────────────────────

export async function diagnoseWithLlm(decodeResult, params, llmConfig) {
  const userContent = buildDiagnosticPrompt(decodeResult, params);
  return chatComplete(llmConfig, [
    { role: 'system', content: SYSTEM_PROMPT },
    { role: 'user',   content: userContent },
  ]);
}
