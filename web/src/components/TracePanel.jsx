import React, { useState } from 'react';
import { useStore, getSegGeomCache } from '../store.js';
import { verdictType } from '../replayEngine.js';

const ASTAR_DISPLAY_CAP = 200;

const FRC_NAME = ['FRC0 Motorway', 'FRC1 Trunk', 'FRC2 Secondary', 'FRC3 Tertiary',
                  'FRC4 Unclassified', 'FRC5 Residential', 'FRC6 Service', 'FRC7 Other'];
const FOW_NAME = ['Undefined', 'Motorway', 'Dual C/W', 'Single C/W',
                  'Roundabout', 'Traffic Sq', 'Slip Road', 'Other'];

// ── Event parsing ─────────────────────────────────────────────────────────────

function parseTraceEvents(events) {
  const candidates = {}; // lrp_idx → { searchStart, ranked, evaluated[] }
  const routing = {};    // leg → { start, astarNodes[], astarSkipped[], result, dnp }
  const offsets = [];
  let decodeComplete = null;

  for (const event of events) {
    const type = Object.keys(event)[0];
    const data = event[type];
    switch (type) {
      case 'CandidateSearchStarted':
        candidates[data.lrp_idx] ??= {};
        candidates[data.lrp_idx].searchStart = data;
        break;
      case 'CandidateEvaluated':
        candidates[data.lrp_idx] ??= {};
        (candidates[data.lrp_idx].evaluated ??= []).push(data);
        break;
      case 'CandidatesRanked':
        candidates[data.lrp_idx] ??= {};
        candidates[data.lrp_idx].ranked = data;
        break;
      case 'RouteSearchStarted':
        routing[data.leg] ??= {};
        routing[data.leg].start = data;
        break;
      case 'AStarNodeExpanded':
        routing[data.leg] ??= {};
        (routing[data.leg].astarNodes ??= []).push(data);
        break;
      case 'AStarEdgeSkipped':
        routing[data.leg] ??= {};
        (routing[data.leg].astarSkipped ??= []).push(data);
        break;
      case 'RouteFound':
        routing[data.leg] ??= {};
        routing[data.leg].result = { found: true, ...data };
        break;
      case 'RouteFailed':
        routing[data.leg] ??= {};
        routing[data.leg].result = { found: false, ...data };
        break;
      case 'DnpChecked':
        routing[data.leg] ??= {};
        routing[data.leg].dnp = data;
        break;
      case 'OffsetApplied':
        offsets.push(data);
        break;
      case 'DecodeComplete':
        decodeComplete = data;
        break;
    }
  }
  return { candidates, routing, offsets, decodeComplete };
}

// ── Formatting helpers ────────────────────────────────────────────────────────

function fmtM(v) { return v == null ? '—' : `${v.toFixed(1)} m`; }
function fmtScore(v) { return v == null ? '—' : v.toFixed(4); }

function fmtSkipReason(reason) {
  // Unit variants serialise as plain strings; struct variants as {TypeName: {…}}.
  const [type, data] = typeof reason === 'string'
    ? [reason, {}]
    : Object.entries(reason)[0];
  switch (type) {
    case 'FrcBelowLfrcnp':    return `FRC ${data.seg_frc} > LFRCNP ${data.lfrcnp} (too unimportant)`;
    case 'DirectionBlocked':  return 'One-way — wrong direction';
    case 'TurnRestricted':    return 'Turn restriction';
    case 'ExceedsMaxDistance': return `Exceeds max dist (${data.distance_m?.toFixed(0)}m > ${data.max_m?.toFixed(0)}m)`;
    default: return type;
  }
}

function fmtRouteFailReason(reason) {
  if (typeof reason === 'string') return reason;
  const [type, data] = Object.entries(reason)[0];
  if (type === 'DnpOutOfRange') {
    return `DNP out of range (actual ${data.actual_m?.toFixed(1)}m, window [${data.window?.lb?.toFixed(1)}, ${data.window?.ub?.toFixed(1)}]m)`;
  }
  return type;
}

// ── Section wrapper ───────────────────────────────────────────────────────────

function Section({ title, badge, badgeOk, defaultOpen = true, children }) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div className="tp-section">
      <button className="tp-section-hdr" onClick={() => setOpen(o => !o)}>
        <span className="tp-section-arrow">{open ? '▾' : '▸'}</span>
        <span className="tp-section-title">{title}</span>
        {badge != null && (
          <span className={`tp-badge ${badgeOk === false ? 'tp-badge-err' : badgeOk === true ? 'tp-badge-ok' : ''}`}>
            {badge}
          </span>
        )}
      </button>
      {open && <div className="tp-section-body">{children}</div>}
    </div>
  );
}

// ── Segment highlight button ──────────────────────────────────────────────────

function SegBtn({ segId, setTraceHighlight, onSelect }) {
  const sourceKey = getSegGeomCache().get(segId)?.properties?.source_id ?? null;
  const label = sourceKey != null ? sourceKey : segId;
  return (
    <button
      className="tp-seg-btn"
      title={`Segment Key ${sourceKey ?? '?'} · Internal ID ${segId}`}
      onClick={(e) => {
        e.stopPropagation();
        setTraceHighlight([segId]);
        onSelect?.();
      }}
    >
      {label}
    </button>
  );
}

function buildCandPopup(segId, lrpIdx, traversal, ctype, winner, snapPt, projection, score, verdict) {
  const feat = getSegGeomCache().get(segId);
  return {
    lrp_idx:      lrpIdx,
    segment_id:   segId,
    source_id:    feat?.properties?.source_id ?? null,
    traversal,
    ctype,
    winner:       winner ?? false,
    snap_lon:     snapPt?.[0] ?? null,
    snap_lat:     snapPt?.[1] ?? null,
    distance_m:   projection?.distance_m ?? null,
    arc_offset_m: projection?.arc_offset_m ?? null,
    bearing_deg:  projection?.bearing_deg ?? null,
    score_total:      score?.total ?? null,
    score_distance:   score?.distance_score ?? null,
    score_bearing:    score?.bearing_score ?? null,
    score_frc:        score?.frc_score ?? null,
    score_fow:        score?.fow_score ?? null,
    score_wrong_ep:   score?.wrong_endpoint_score ?? null,
    score_interior:   score?.interior_score ?? null,
    verdict_json:     verdict ? JSON.stringify(verdict) : null,
    frc:       feat?.properties?.frc ?? null,
    fow:       feat?.properties?.fow ?? null,
    frc_name:  feat?.properties?.frc_name ?? null,
    fow_name:  feat?.properties?.fow_name ?? null,
    direction: feat?.properties?.direction ?? null,
    length_m:  feat?.properties?.length_m ?? null,
  };
}

// ── Reference summary ─────────────────────────────────────────────────────────

function fmtBearing(lb, ub) {
  return Math.abs(ub - lb) < 0.1 ? `${lb.toFixed(1)}°` : `[${lb.toFixed(1)}°, ${ub.toFixed(1)}°]`;
}

function fmtDnp(lb, ub) {
  if (lb == null) return null;
  return Math.abs(ub - lb) < 0.1 ? `${lb.toFixed(1)} m` : `[${lb.toFixed(1)}, ${ub.toFixed(1)}] m`;
}

function fmtOffset(lb, ub) {
  if (!lb && !ub) return null;
  return Math.abs(ub - lb) < 0.1 ? `${lb.toFixed(1)} m` : `[${lb.toFixed(1)}, ${ub.toFixed(1)}] m`;
}

function buildRefSummary(openlrString, lrps, decodeResult) {
  const fmtLabel = decodeResult?.format === 'TomTomV3' ? 'TomTom v3'
                 : decodeResult?.format === 'Tpeg'     ? 'TPEG-OLR'
                 : '(unknown)';
  const lines = ['{'];
  lines.push(`  "format": "${fmtLabel}",`);
  lines.push(`  "string": "${openlrString}",`);
  const posOff = fmtOffset(decodeResult?.pos_offset_lb, decodeResult?.pos_offset_ub);
  const negOff = fmtOffset(decodeResult?.neg_offset_lb, decodeResult?.neg_offset_ub);
  if (posOff) lines.push(`  "pos_offset": ${posOff},`);
  if (negOff) lines.push(`  "neg_offset": ${negOff},`);
  lines.push(`  "lrps": [`);
  lrps?.forEach((l, i) => {
    const isLast = i === lrps.length - 1;
    const dnp = fmtDnp(l.dnp_lb, l.dnp_ub);
    const parts = [
      `"coord": [${l.lon.toFixed(6)}, ${l.lat.toFixed(6)}]`,
      `"frc": ${l.frc}`,
      `"fow": ${l.fow}`,
      l.lfrcnp != null ? `"lfrcnp": ${l.lfrcnp}` : null,
      `"bearing": ${fmtBearing(l.bearing_lb, l.bearing_ub)}`,
      dnp ? `"dnp": ${dnp}` : null,
    ].filter(Boolean);
    lines.push(`    { ${parts.join(', ')} }${isLast ? '' : ','}`);
  });
  lines.push('  ]');
  lines.push('}');
  return lines.join('\n');
}

function ReferenceSummarySection({ openlrString, lrps, decodeResult, setTraceLrpFocus, lfrcnpTolerance = 0 }) {
  const summary = buildRefSummary(openlrString, lrps, decodeResult);
  return (
    <Section title="Reference" defaultOpen={true}>
      <pre className="tp-ref-json">{summary}</pre>
      {lrps?.length > 0 && (
        <table className="tp-table tp-lrp-table">
          <thead>
            <tr>
              <th>#</th><th>Lon</th><th>Lat</th><th>FRC</th><th>FOW</th>
              <th>LFRCNP</th><th>Bearing</th><th>DNP</th>
            </tr>
          </thead>
          <tbody>
            {lrps.map((l, i) => (
              <tr
                key={i}
                className="tp-lrp-row"
                title="Click to pan to this LRP"
                onClick={() => setTraceLrpFocus({ ...l, index: i })}
              >
                <td className="tp-dim">LRP{i + 1}</td>
                <td>{l.lon.toFixed(5)}</td>
                <td>{l.lat.toFixed(5)}</td>
                <td>{l.frc}</td>
                <td>{l.fow}</td>
                <td>{l.lfrcnp != null
                  ? (lfrcnpTolerance > 0
                    ? `${l.lfrcnp} → ${Math.min(l.lfrcnp + lfrcnpTolerance, 7)}`
                    : l.lfrcnp)
                  : '—'}</td>
                <td className="tp-monospace">
                  {Math.abs(l.bearing_ub - l.bearing_lb) < 0.1
                    ? `${l.bearing_lb.toFixed(1)}°`
                    : `${l.bearing_lb.toFixed(1)}°–${l.bearing_ub.toFixed(1)}°`}
                </td>
                <td className="tp-monospace">
                  {l.dnp_lb == null ? '—' : fmtDnp(l.dnp_lb, l.dnp_ub)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Section>
  );
}

// ── Gate verdict formatting ───────────────────────────────────────────────────

function fmtVerdict(verdict) {
  if (typeof verdict === 'string') return { label: verdict, cls: 'tp-gate-other' };
  const [type, data] = Object.entries(verdict)[0];
  switch (type) {
    case 'FailBearing':
      return { label: `Bearing +${data.excess_deg.toFixed(1)}° (max ${data.max_deg}°)`, cls: 'tp-gate-bearing' };
    case 'FailRadius':
      return { label: `Radius ${data.distance_m.toFixed(1)}m > ${data.radius_m}m`, cls: 'tp-gate-radius' };
    case 'FailScore':
      return { label: `Score ${data.total.toFixed(3)} > ${data.max_score}`, cls: 'tp-gate-score' };
    case 'FailDirection':
      return { label: 'Degenerate geometry', cls: 'tp-gate-other' };
    default:
      return { label: type, cls: 'tp-gate-other' };
  }
}

// ── Candidates section ────────────────────────────────────────────────────────

function RejectedTable({ rejected, lrpIdx, setTraceHighlight, setCandidatePopup }) {
  const [open, setOpen] = useState(false);
  if (!rejected?.length) return null;
  return (
    <div className="tp-rejected-wrap">
      <button className="tp-rejected-toggle" onClick={() => setOpen(o => !o)}>
        <span className="tp-section-arrow">{open ? '▾' : '▸'}</span>
        <span className="tp-gate-other tp-gate-pill">{rejected.length} rejected</span>
      </button>
      {open && (
        <table className="tp-table tp-rej-table">
          <thead>
            <tr>
              <th>Seg Key</th>
              <th>Dir</th>
              <th>Dist m</th>
              <th>Bear °</th>
              <th>Gate failure</th>
            </tr>
          </thead>
          <tbody>
            {rejected.map((r, i) => {
              const { label, cls } = fmtVerdict(r.verdict);
              return (
                <tr key={i}>
                  <td><SegBtn segId={r.segment_id} setTraceHighlight={setTraceHighlight}
                    onSelect={() => setCandidatePopup(buildCandPopup(
                      r.segment_id, lrpIdx, r.traversal, verdictType(r.verdict), false,
                      r.point, { distance_m: r.distance_m, bearing_deg: r.bearing_deg }, null, r.verdict
                    ))} /></td>
                  <td className="tp-dim">{r.traversal === 'Forward' ? 'Fwd' : 'Bwd'}</td>
                  <td>{r.distance_m != null ? r.distance_m.toFixed(1) : '—'}</td>
                  <td>{r.bearing_deg != null ? r.bearing_deg.toFixed(1) : '—'}</td>
                  <td><span className={`tp-gate-pill ${cls}`}>{label}</span></td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
    </div>
  );
}

function CandidatesSection({ lrpIdx, phase, lrpInfo, setTraceHighlight, setCandidatePopup }) {
  const ranked = phase?.ranked;
  if (!ranked) return null;

  const lrp = lrpInfo?.[lrpIdx];
  const subtitle = lrp
    ? `LRP ${lrpIdx + 1} · ${lrp.lon.toFixed(4)},${lrp.lat.toFixed(4)}`
    : `LRP ${lrpIdx + 1}`;
  const accepted = ranked.accepted ?? [];
  const rejected = ranked.rejected ?? [];

  return (
    <Section
      title={`Candidates — ${subtitle}`}
      badge={`${accepted.length} ✓  ${rejected.length} ✗`}
      defaultOpen={true}
    >
      {accepted.length === 0 ? (
        <div className="tp-empty">No candidates accepted</div>
      ) : (
        <table className="tp-table tp-cand-table">
          <thead>
            <tr>
              <th>#</th>
              <th>Seg Key</th>
              <th>Dir</th>
              <th>Dist m</th>
              <th>Bear °</th>
              <th>Arc m</th>
              <th title="total score (lower = better)">Score</th>
              <th title="distance component">Dist</th>
              <th title="bearing component">Bear</th>
              <th title="FRC component">FRC</th>
              <th title="FOW component">FOW</th>
              <th title="wrong endpoint component">WEP</th>
            </tr>
          </thead>
          <tbody>
            {accepted.map((c, i) => (
              <tr key={i} className={i === 0 ? 'tp-best-row' : ''}>
                <td className="tp-dim">{i}</td>
                <td>
                  <SegBtn segId={c.segment_id} setTraceHighlight={setTraceHighlight}
                    onSelect={() => setCandidatePopup(buildCandPopup(
                      c.segment_id, lrpIdx, c.traversal, 'accepted', i === 0,
                      c.projection.point, c.projection, c.score, null
                    ))} />
                </td>
                <td className="tp-dim">{c.traversal === 'Forward' ? 'Fwd' : 'Bwd'}</td>
                <td>{c.projection.distance_m.toFixed(1)}</td>
                <td>{c.projection.bearing_deg.toFixed(1)}</td>
                <td>{c.projection.arc_offset_m.toFixed(1)}</td>
                <td className="tp-score-total">{fmtScore(c.score.total)}</td>
                <td className="tp-dim">{fmtScore(c.score.distance_score)}</td>
                <td className="tp-dim">{fmtScore(c.score.bearing_score)}</td>
                <td className="tp-dim">{fmtScore(c.score.frc_score)}</td>
                <td className="tp-dim">{fmtScore(c.score.fow_score)}</td>
                <td className="tp-dim">{fmtScore(c.score.wrong_endpoint_score)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <RejectedTable rejected={rejected} lrpIdx={lrpIdx}
        setTraceHighlight={setTraceHighlight} setCandidatePopup={setCandidatePopup} />
    </Section>
  );
}

// ── Routing section ───────────────────────────────────────────────────────────

function RoutingSection({ leg, phase, fromCandidate, toCandidate, setTraceHighlight }) {
  const [showAstar, setShowAstar] = useState(false);
  if (!phase) return null;

  const start = phase.start;
  const result = phase.result;
  const dnp = phase.dnp;
  const nodes = phase.astarNodes ?? [];
  const skipped = phase.astarSkipped ?? [];
  const found = result?.found ?? false;

  const fromSeg = start?.from?.segment_id;
  const toSeg = start?.to?.segment_id;

  // Direct match: both LRPs projected onto the same segment → no routing needed, DNP = 0
  const isDirect = !start && dnp?.actual_m === 0;
  const directSeg = fromCandidate?.segment_id;
  const directSeg2 = (toCandidate?.segment_id != null && toCandidate.segment_id !== directSeg)
    ? toCandidate.segment_id : null;

  return (
    <Section
      title={`Routing — Leg ${leg + 1}`}
      badge={found ? 'found' : 'failed'}
      badgeOk={found}
      defaultOpen={true}
    >
      {isDirect && (
        <div className="tp-routing-pair">
          <span className="tp-dim">Direct match on </span>
          {directSeg != null
            ? <SegBtn segId={directSeg} setTraceHighlight={setTraceHighlight} />
            : <span className="tp-dim">—</span>}
          {directSeg2 != null && (
            <><span className="tp-dim"> / </span><SegBtn segId={directSeg2} setTraceHighlight={setTraceHighlight} /></>
          )}
          <span className="tp-dim"> — same-segment match; no intermediate route needed</span>
        </div>
      )}
      {!start && !result && !isDirect && (
        <div className="tp-dim" style={{ marginBottom: 4 }}>
          No route search data recorded (trace_level may be too low)
        </div>
      )}

      {start && (
        <div className="tp-routing-pair">
          <span className="tp-dim">From </span>
          <SegBtn segId={fromSeg} setTraceHighlight={setTraceHighlight} />
          <span className="tp-dim"> ({start.from?.traversal === 'Forward' ? 'Fwd' : 'Bwd'}, {fmtM(start.from?.projection?.distance_m)})</span>
          <span className="tp-dim"> → To </span>
          <SegBtn segId={toSeg} setTraceHighlight={setTraceHighlight} />
          <span className="tp-dim"> ({start.to?.traversal === 'Forward' ? 'Fwd' : 'Bwd'}, {fmtM(start.to?.projection?.distance_m)})</span>
        </div>
      )}

      {result?.found && result.path?.length > 0 && (
        <div className="tp-route-path">
          <span className="tp-dim">Path </span>
          <button
            className="tp-seg-btn"
            onClick={() => setTraceHighlight(result.path, result.from_snap && result.to_snap ? { from: result.from_snap, to: result.to_snap } : null)}
            title="Highlight all path segments"
          >
            [{result.path.length} segs]
          </button>
          <span className="tp-dim"> · {fmtM(result.length_m)}</span>
        </div>
      )}

      {!result?.found && result && (
        <div className="tp-route-fail">✗ {fmtRouteFailReason(result.reason)}</div>
      )}

      {dnp && (
        <div className={`tp-dnp ${dnp.passed ? 'tp-ok' : 'tp-err'}`}>
          DNP {fmtM(dnp.actual_m)} {dnp.passed ? '∈' : '∉'} [{fmtM(Math.max(0, dnp.interval?.lb ?? 0))}, {fmtM(dnp.interval?.ub)}] {dnp.passed ? '✓' : '✗'}
        </div>
      )}

      {nodes.length > 0 && (
        <div className="tp-astar-summary">
          <button className="tp-expand-btn" onClick={() => setShowAstar(v => !v)}>
            {showAstar ? '▾' : '▸'} A* expanded {nodes.length} node{nodes.length !== 1 ? 's' : ''}
            {skipped.length > 0 && `, ${skipped.length} skipped`}
          </button>
          {showAstar && (
            <div className="tp-astar-list">
              <table className="tp-table">
                <thead>
                  <tr><th>#</th><th>Node</th><th>Via Seg</th><th>G (m)</th><th>H (m)</th><th>F (m)</th></tr>
                </thead>
                <tbody>
                  {nodes.slice(0, ASTAR_DISPLAY_CAP).map((n, i) => (
                    <tr key={i}>
                      <td className="tp-dim">{i + 1}</td>
                      <td>{n.node_id}</td>
                      <td><SegBtn segId={n.via_segment} setTraceHighlight={setTraceHighlight} /></td>
                      <td>{n.g_m.toFixed(1)}</td>
                      <td>{n.h_m.toFixed(1)}</td>
                      <td>{(n.g_m + n.h_m).toFixed(1)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
              {nodes.length > ASTAR_DISPLAY_CAP && (
                <div className="tp-note">
                  Showing first {ASTAR_DISPLAY_CAP} of {nodes.length} nodes · use Copy JSON for full data
                </div>
              )}
              {skipped.length > 0 && (
                <details className="tp-skipped">
                  <summary className="tp-dim">{skipped.length} edges skipped</summary>
                  <table className="tp-table">
                    <thead><tr><th>From Node</th><th>Seg Key</th><th>Reason</th></tr></thead>
                    <tbody>
                      {skipped.slice(0, 100).map((e, i) => (
                        <tr key={i}>
                          <td>{e.from_node}</td>
                          <td><SegBtn segId={e.segment_id} setTraceHighlight={setTraceHighlight} /></td>
                          <td className="tp-dim">{fmtSkipReason(e.reason)}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </details>
              )}
            </div>
          )}
        </div>
      )}
    </Section>
  );
}

// ── Offsets section ───────────────────────────────────────────────────────────

function OffsetsSection({ offsets }) {
  if (!offsets?.length) return null;
  return (
    <Section title="Offsets" defaultOpen={true}>
      {offsets.map((o, i) => (
        <div key={i} className="tp-row">
          <span className="tp-dim">{o.is_positive ? 'Positive' : 'Negative'}</span>
          {o.interval && o.interval.lb === o.interval.ub
            ? ` ${o.interval.lb?.toFixed(1)} m`
            : o.interval && ` [${o.interval.lb?.toFixed(1)}, ${o.interval.ub?.toFixed(1)}] m`}
        </div>
      ))}
    </Section>
  );
}

// ── Result section ────────────────────────────────────────────────────────────

function ResultSection({ decodeResult }) {
  if (!decodeResult) return null;
  return (
    <Section title="Result" defaultOpen={true}>
      <div className={`tp-row ${decodeResult.ok ? 'tp-ok' : 'tp-err'}`}>
        {decodeResult.ok ? '✓ Decoded' : '✗ Failed'}
        {decodeResult.ok && ` · ${decodeResult.segments?.length ?? 0} segment${decodeResult.segments?.length !== 1 ? 's' : ''}`}
        {decodeResult.ok && decodeResult.pos_offset_ub > 0 && ` · +[${decodeResult.pos_offset_lb?.toFixed(1)}, ${decodeResult.pos_offset_ub?.toFixed(1)}] m`}
        {decodeResult.ok && decodeResult.neg_offset_ub > 0 && ` · −[${decodeResult.neg_offset_lb?.toFixed(1)}, ${decodeResult.neg_offset_ub?.toFixed(1)}] m`}
      </div>
      {!decodeResult.ok && decodeResult.error && (
        <div className="tp-err tp-row">{decodeResult.error}</div>
      )}
      {decodeResult.ok && decodeResult.wkt && (
        <div className="tp-wkt-row">
          <div className="tp-wkt tp-monospace tp-dim">{decodeResult.wkt.slice(0, 140)}{decodeResult.wkt.length > 140 ? '…' : ''}</div>
          <button className="tp-copy-btn" title="Copy WKT (trimmed at LB — maximum extent)" onClick={() => navigator.clipboard.writeText(decodeResult.wkt)}>⎘</button>
        </div>
      )}
    </Section>
  );
}

// ── Main panel ────────────────────────────────────────────────────────────────

export default function TracePanel() {
  const { decodeResult, openlrString, params, setParam,
          setTraceHighlight, setTraceLrpFocus, replaySteps, setCandidatePopup } = useStore();

  const traceLevel = params.trace_level ?? 'Summary';
  const trace = decodeResult?.trace;
  const lrps = decodeResult?.lrps ?? [];

  const { candidates, routing, offsets, decodeComplete } =
    trace?.events ? parseTraceEvents(trace.events) : { candidates: {}, routing: {}, offsets: [], decodeComplete: null };

  const lrpCount = lrps.length;
  const legCount = lrpCount > 1 ? lrpCount - 1 : 0;

  const copyJson = () => {
    navigator.clipboard.writeText(JSON.stringify(decodeResult, null, 2)).catch(() => {});
  };

  const toggleLevel = () => {
    setParam('trace_level', traceLevel === 'Full' ? 'Summary' : 'Full');
  };

  return (
    <div className="trace-panel">
      <div className="trace-panel-hdr">
        <span className="trace-panel-title">⚡ Decode Trace</span>
        <div className="trace-panel-actions">
          <button
            className={`tp-level-btn${traceLevel === 'Full' ? ' active' : ''}`}
            onClick={toggleLevel}
            title={traceLevel === 'Full'
              ? 'Full trace active — click to switch to Summary'
              : 'Summary trace — click to enable Full (A* expansion)'}
          >
            {traceLevel === 'Full' ? 'Full ●' : 'Summary'}
          </button>
          <button className="tp-copy-btn" onClick={copyJson} disabled={!decodeResult} title="Copy decode result + trace JSON">
            Copy JSON
          </button>
        </div>
      </div>

      <div className="trace-panel-body">
        {!decodeResult && (
          <div className="tp-empty-state">Decode a reference to see trace data.</div>
        )}
        {decodeResult && (
          <ReferenceSummarySection
            openlrString={openlrString}
            lrps={lrps}
            decodeResult={decodeResult}
            setTraceLrpFocus={setTraceLrpFocus}
            lfrcnpTolerance={params.lfrcnp_tolerance ?? 0}
          />
        )}
        {decodeResult && !trace && (
          <div className="tp-empty-state">
            {traceLevel === 'Off'
              ? 'Trace level is Off — set to Summary or Full and decode again for routing detail.'
              : 'No trace data in last decode — decode again to capture routing detail.'}
          </div>
        )}
        {trace && (
          <>

            {Array.from({ length: lrpCount }, (_, i) => (
              <CandidatesSection
                key={`cand-${i}`}
                lrpIdx={i}
                phase={candidates[i]}
                lrpInfo={lrps}
                setTraceHighlight={setTraceHighlight}
                setCandidatePopup={setCandidatePopup}
              />
            ))}

            {Array.from({ length: legCount }, (_, i) => (
              <RoutingSection
                key={`route-${i}`}
                leg={i}
                phase={routing[i]}
                fromCandidate={candidates[i]?.ranked?.accepted?.[0]}
                toCandidate={candidates[i + 1]?.ranked?.accepted?.[0]}
                setTraceHighlight={setTraceHighlight}
              />
            ))}

            {offsets.length > 0 && <OffsetsSection offsets={offsets} />}

            <ResultSection decodeResult={decodeResult} />
          </>
        )}
      </div>
    </div>
  );
}
