import React, { useState, useRef, useEffect } from 'react';
import { useStore } from '../store.js';
import { diagnoseFailure, diagnoseSuccess } from '../diagnosis.js';
import { renderLlmText } from '../renderLlmText.jsx';

// ── Reference panel helpers ───────────────────────────────────────────────────

const FRC_LABEL = [
  'FRC0 · Motorway', 'FRC1 · Trunk', 'FRC2 · Secondary', 'FRC3 · Tertiary',
  'FRC4 · Unclassified', 'FRC5 · Residential', 'FRC6 · Service/Link', 'FRC7 · Other/Path',
];
const FOW_LABEL = [
  'Undefined', 'Motorway', 'Dual Carriageway', 'Single Carriageway',
  'Roundabout', 'Traffic Square', 'Slip Road', 'Other',
];

const HELP = {
  frc:     'Functional Road Class (0–7): how important the road is (0 = motorway, 7 = local path). Candidates must match within the configured FRC tolerance.',
  fow:     'Form of Way: the geometric road type (motorway, dual carriageway, roundabout, slip road, etc.).',
  bearing: 'Direction of travel at this LRP, clockwise from North. TomTomV3 uses an 11.25° sector (32 sectors); TPEG-OLR uses a 1.41° sector (256 sectors). Decoded against the interval ± the map tolerance.',
  dnp:     'Distance to Next Point: encoded path length from this LRP to the next (meters). TomTomV3 quantises into ~58.6 m buckets (max ~14,901 m); TPEG-OLR is exact. The found route length must fall within this interval ± tolerance.',
  lfrcnp:  'Lowest FRC to Next Point: the least-important road class the A* path between this LRP and the next may use. Prevents re-routing via minor roads when the encoder used a motorway.',
  offset:  'Trim distance applied after route validation — positive from the path start, negative from the path end.',
};

function Help({ field }) {
  return <span className="ref-help" title={HELP[field]}>?</span>;
}

function fmtBearing(lb, ub) {
  return Math.abs(ub - lb) < 0.1
    ? `${lb.toFixed(1)}°`
    : `${lb.toFixed(1)}°–${ub.toFixed(1)}°`;
}

function fmtInterval(lb, ub) {
  if (lb == null) return null;
  return Math.abs(ub - lb) < 0.1
    ? `${lb.toFixed(0)} m`
    : `${lb.toFixed(0)}–${ub.toFixed(0)} m`;
}

function RefSect({ title, children, defaultOpen = true }) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div className="ref-sect">
      <button className="ref-sect-hdr" onClick={() => setOpen(o => !o)}>
        <span className="ref-sect-arrow">{open ? '▼' : '▶'}</span>
        {title}
      </button>
      {open && <div className="ref-sect-body">{children}</div>}
    </div>
  );
}

function RefRow({ label, value, helpKey }) {
  return (
    <div className="ref-row">
      <span className="ref-label">{label}{helpKey && <Help field={helpKey} />}</span>
      <span className="ref-val">{value}</span>
    </div>
  );
}

function ReferenceSection({ decodeResult, onLrpClick }) {
  const { format, location_type, lrps, pos_offset_lb, pos_offset_ub, neg_offset_lb, neg_offset_ub } = decodeResult;
  const isV3  = format === 'TomTomV3';
  const hasPos = pos_offset_ub > 0;
  const hasNeg = neg_offset_ub > 0;

  return (
    <div className="ref-section">
      <RefSect title="Reference" defaultOpen={true}>
        <RefRow label="Format"
          value={isV3 ? 'TomTomV3 (binary v3)' : 'TPEG-OLR (ISO 21219-22)'} />
        <RefRow label="Type"   value={location_type} />
        <RefRow label="LRPs"   value={lrps.length} />
        {(hasPos || hasNeg) && <>
          {hasPos && <RefRow label="Pos. offset" helpKey="offset"
            value={fmtInterval(pos_offset_lb, pos_offset_ub)} />}
          {hasNeg && <RefRow label="Neg. offset" helpKey="offset"
            value={fmtInterval(neg_offset_lb, neg_offset_ub)} />}
        </>}
      </RefSect>

      <RefSect title="Location Reference Points" defaultOpen={true}>
        {lrps.map((lrp, i) => {
          const isFirst = i === 0;
          const isLast  = i === lrps.length - 1;
          const role    = isFirst ? 'First' : isLast ? 'Last' : 'Intermediate';
          const dotCls  = isFirst ? 'first' : isLast ? 'last' : 'mid';
          const dnpStr  = fmtInterval(lrp.dnp_lb, lrp.dnp_ub);
          const latDir  = lrp.lat >= 0 ? 'N' : 'S';
          const lonDir  = lrp.lon >= 0 ? 'E' : 'W';
          return (
            <div key={i} className="lrp-card">
              <button
                className="lrp-card-hdr"
                title="Zoom to this LRP on the map"
                onClick={() => onLrpClick?.({
                  index: i, lat: lrp.lat, lon: lrp.lon,
                  frc: lrp.frc, fow: lrp.fow,
                  lfrcnp: lrp.lfrcnp ?? null,
                  bearing_lb: lrp.bearing_lb, bearing_ub: lrp.bearing_ub,
                })}
              >
                <span className={`lrp-dot lrp-dot-${dotCls}`} />
                LRP {i + 1} · {role}
              </button>
              <RefRow label="Coord"
                value={`${Math.abs(lrp.lat).toFixed(5)}°${latDir}  ${Math.abs(lrp.lon).toFixed(5)}°${lonDir}`} />
              <RefRow label="FRC"     helpKey="frc"
                value={FRC_LABEL[lrp.frc] ?? `FRC${lrp.frc}`} />
              <RefRow label="FOW"     helpKey="fow"
                value={FOW_LABEL[lrp.fow] != null ? `FOW${lrp.fow} · ${FOW_LABEL[lrp.fow]}` : `FOW${lrp.fow}`} />
              <RefRow label="Bearing" helpKey="bearing"
                value={fmtBearing(lrp.bearing_lb, lrp.bearing_ub)} />
              {!isLast && dnpStr &&
                <RefRow label="DNP" helpKey="dnp" value={dnpStr} />}
              {!isLast && lrp.lfrcnp != null &&
                <RefRow label="LFRCNP" helpKey="lfrcnp"
                  value={FRC_LABEL[lrp.lfrcnp] ?? `FRC${lrp.lfrcnp}`} />}
            </div>
          );
        })}
      </RefSect>
    </div>
  );
}

// ── Decode result section ─────────────────────────────────────────────────────

const FOW_NAMES = ['Undef', 'Motorway', 'Dual C/W', 'Single C/W', 'Roundabout', 'Traffic Sq', 'Slip Rd', 'Other'];
const FRC_NAMES = ['FRC0', 'FRC1', 'FRC2', 'FRC3', 'FRC4', 'FRC5', 'FRC6', 'FRC7'];

export default function ResultPanel() {
  const { decodeResult, highlightedSegment, setHighlightedSegment,
          requestInfoSegment, showTrace, toggleTrace, debugDecode, params,
          llmConfig, llmChatOpen, toggleLlmChat, toggleLlmSettings,
          setTraceLrpFocus } = useStore();

  const [refHeight, setRefHeight] = useState(280);
  const dragging  = useRef(false);
  const dragY0    = useRef(0);
  const dragH0    = useRef(0);

  useEffect(() => {
    function onMove(e) {
      if (!dragging.current) return;
      setRefHeight(Math.max(80, Math.min(700, dragH0.current + (e.clientY - dragY0.current))));
    }
    function onUp() { dragging.current = false; }
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
    return () => {
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
    };
  }, []);

  if (!decodeResult) return (
    <div className="result-panel-empty">Decode a reference to see results.</div>
  );

  const hasRef = (decodeResult.lrps?.length ?? 0) > 0;

  const diagnosis      = decodeResult.ok ? null : diagnoseFailure(decodeResult);
  const successWarning = decodeResult.ok ? diagnoseSuccess(decodeResult) : null;

  const hasTrace  = !!decodeResult.trace;
  const isFull    = params.trace_level === 'Full';
  const debugLabel = !hasTrace && isFull  ? 'Re-decode'
                   : !hasTrace           ? 'Re-decode with tracing'
                   : !isFull             ? 'Re-decode with full trace'
                   : !showTrace ? 'Open trace panel'
                   : null;
  const debugAction = (!hasTrace || !isFull) ? debugDecode : toggleTrace;

  return (
    <div className="result-panel">

      {/* ── Reference section (top, draggable height) ── */}
      {hasRef && (
        <>
          <div className="ref-area" style={{ height: refHeight }}>
            <ReferenceSection decodeResult={decodeResult} onLrpClick={setTraceLrpFocus} />
          </div>
          <div
            className="panel-split-handle"
            onMouseDown={e => {
              dragging.current = true;
              dragY0.current   = e.clientY;
              dragH0.current   = refHeight;
              e.preventDefault();
            }}
          />
        </>
      )}

      {/* ── Decode result section (fills remaining height) ── */}
      <div className="result-decode-area">
        <div className={`result-header ${decodeResult.ok ? 'ok' : 'err'}`}>
          <span>{decodeResult.ok
            ? (decodeResult.location_type === 'PointAlongLine' ? '✓ Decoded (Point)' : '✓ Decoded')
            : '✗ Failed'}</span>
        </div>
        <div className="result-body">
          {decodeResult.ok ? (
            <>
              <div className="result-meta">
                {decodeResult.location_type === 'PointAlongLine'
                  ? 'PointAlongLine'
                  : `${decodeResult.segments.length} segment${decodeResult.segments.length !== 1 ? 's' : ''}`}
                {decodeResult.pos_offset_ub > 0 && ` · +[${decodeResult.pos_offset_lb.toFixed(1)}, ${decodeResult.pos_offset_ub.toFixed(1)}] m`}
                {decodeResult.neg_offset_ub > 0 && ` · −[${decodeResult.neg_offset_lb.toFixed(1)}, ${decodeResult.neg_offset_ub.toFixed(1)}] m`}
                {decodeResult.trace && !showTrace && (
                  <button className="result-trace-link" onClick={toggleTrace} title="Open decode trace panel">
                    ⚡ Trace
                  </button>
                )}
              </div>
              <div className="seg-table-wrap">
                <table className="seg-table">
                  <thead>
                    <tr>
                      <th>Segment Key</th>
                      <th>FRC</th>
                      <th>FOW</th>
                      <th>Dir</th>
                      <th>Length</th>
                    </tr>
                  </thead>
                  <tbody>
                    {decodeResult.segments.map((s, i) => {
                      const isActive = highlightedSegment?.tile === s.tile &&
                                       highlightedSegment?.local_index === s.local_index;
                      return (
                        <tr key={i} className={isActive ? 'seg-row-active' : ''}>
                          <td>
                            <button
                              className="seg-row-btn"
                              title={`Tile ${s.tile} · tile index ${s.local_index} · internal ID ${s.segment_id}`}
                              onClick={() => {
                                const nowActive = !isActive;
                                setHighlightedSegment(nowActive ? { tile: s.tile, local_index: s.local_index } : null);
                                if (nowActive) requestInfoSegment(s.tile, s.local_index);
                              }}
                            >{s.source_id ?? s.segment_id ?? i + 1}</button>
                          </td>
                          <td>{FRC_NAMES[s.frc] ?? s.frc}</td>
                          <td>{FOW_NAMES[s.fow] ?? s.fow}</td>
                          <td title={s.direction}>{s.direction === 'Both' ? 'S↔E' : s.direction === 'Forward' ? 'S→E' : 'S←E'}</td>
                          <td>{s.length_m != null ? `${s.length_m} m` : '—'}</td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
              {decodeResult.location_type === 'PointAlongLine' && decodeResult.point_lon != null && (
                <div className="pal-point-info">
                  <div className="pal-point-row">
                    <span className="pal-label">Point</span>
                    <span className="pal-value">{decodeResult.point_lat?.toFixed(6)}, {decodeResult.point_lon?.toFixed(6)}</span>
                  </div>
                  {decodeResult.orientation && decodeResult.orientation !== 'NoOrientation' && (
                    <div className="pal-point-row">
                      <span className="pal-label">Orientation</span>
                      <span className="pal-value">{decodeResult.orientation.replace(/([A-Z])/g, ' $1').trim()}</span>
                    </div>
                  )}
                  {decodeResult.side_of_road && decodeResult.side_of_road !== 'DirectlyOnOrNA' && (
                    <div className="pal-point-row">
                      <span className="pal-label">Side of road</span>
                      <span className="pal-value">{decodeResult.side_of_road}</span>
                    </div>
                  )}
                </div>
              )}
              {successWarning && (
                <div className="diag-body diag-body-warn">
                  <div className="diag-headline diag-headline-warn">⚠ {successWarning.headline}</div>
                  <ul className="diag-bullets">
                    {successWarning.bullets.map((b, i) => <li key={i}>{b}</li>)}
                  </ul>
                  {successWarning.suggestions.length > 0 && (
                    <div className="diag-suggestions">
                      <span className="diag-try-label">Note:</span>
                      <ul className="diag-bullets">
                        {successWarning.suggestions.map((s, i) => <li key={i}>{s}</li>)}
                      </ul>
                    </div>
                  )}
                </div>
              )}
              <button
                className="diag-debug-btn llm-ask-btn"
                onClick={llmConfig ? toggleLlmChat : toggleLlmSettings}
                title={llmConfig ? undefined : 'Configure an AI model to use this feature'}
              >
                {llmConfig ? (llmChatOpen ? '✦ Close AI Chat' : '✦ AI Chat') : '✦ AI Chat — configure…'}
              </button>
            </>
          ) : (
            <div className="result-failure">
              <div className="result-error">{decodeResult.error}</div>
              {diagnosis && (
                <div className="diag-body">
                  <div className="diag-headline">{diagnosis.headline}</div>
                  {diagnosis.bullets.length > 0 && (
                    <ul className="diag-bullets">
                      {diagnosis.bullets.map((b, i) => <li key={i}>{b}</li>)}
                    </ul>
                  )}
                  {diagnosis.suggestions.length > 0 && (
                    <div className="diag-suggestions">
                      <span className="diag-try-label">Try:</span>
                      <ul className="diag-bullets">
                        {diagnosis.suggestions.map((s, i) => <li key={i}>{s}</li>)}
                      </ul>
                    </div>
                  )}
                </div>
              )}
              {debugLabel && (
                <button className="diag-debug-btn" onClick={debugAction}>
                  {debugLabel}
                </button>
              )}
              <button
                className="diag-debug-btn llm-ask-btn"
                onClick={llmConfig ? toggleLlmChat : toggleLlmSettings}
                title={llmConfig ? undefined : 'Configure an AI model to use this feature'}
              >
                {llmConfig ? (llmChatOpen ? '✦ Close AI Chat' : '✦ AI Chat') : '✦ AI Chat — configure…'}
              </button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
