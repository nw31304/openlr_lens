import React, { useState, useRef, useEffect } from 'react';
import { useStore } from '../store.js';

const TRACE_LEVELS = ['Off', 'Summary', 'Full'];

export default function TopBar() {
  const { openlrString, showParams, showTrace, showSegmentLayer, showReplay, decoding, params,
          setOpenlrString, toggleParams, toggleTrace, toggleSegmentLayer, toggleReplay,
          setTraceLevel, resetToDefaults, runDecode, replaySteps } = useStore();

  const [showGear, setShowGear] = useState(false);
  const gearRef = useRef(null);

  const traceLevel = params?.trace_level ?? 'Summary';

  useEffect(() => {
    if (!showGear) return;
    const handler = (e) => {
      if (gearRef.current && !gearRef.current.contains(e.target)) setShowGear(false);
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [showGear]);

  return (
    <div className="top-bar">
      <input
        className="openlr-input"
        type="text"
        placeholder="Paste OpenLR string (v3 base64 or TPEG hex)…"
        value={openlrString}
        onChange={e => setOpenlrString(e.target.value)}
        onKeyDown={e => e.key === 'Enter' && runDecode()}
        spellCheck={false}
      />
      <div className="gear-wrap" ref={gearRef}>
        <button
          className={`params-btn${showGear ? ' active' : ''}`}
          onClick={() => setShowGear(g => !g)}
          title="Options"
        >⚙</button>
        {showGear && (
          <div className="gear-panel">
            <div className="gear-row">
              <span>Road segments</span>
              <button className={`gear-toggle${showSegmentLayer ? ' on' : ''}`} onClick={toggleSegmentLayer}>
                {showSegmentLayer ? 'On' : 'Off'}
              </button>
            </div>
            <div className="gear-row">
              <span>Trace panel</span>
              <button className={`gear-toggle${showTrace ? ' on' : ''}`} onClick={toggleTrace}>
                {showTrace ? 'On' : 'Off'}
              </button>
            </div>
            <div className="gear-row">
              <span>Replay</span>
              <button
                className={`gear-toggle${showReplay ? ' on' : ''}${!replaySteps?.length ? ' disabled' : ''}`}
                onClick={toggleReplay}
                disabled={!replaySteps?.length}
                title={!replaySteps?.length ? 'Decode first to enable replay' : undefined}
              >
                {showReplay ? 'On' : 'Off'}
              </button>
            </div>
            <div className="gear-row">
              <span>Trace level</span>
              <div className="gear-level-group">
                {TRACE_LEVELS.map(lvl => (
                  <button
                    key={lvl}
                    className={`gear-level-btn${traceLevel === lvl ? ' active' : ''}`}
                    onClick={() => setTraceLevel(lvl)}
                  >{lvl}</button>
                ))}
              </div>
            </div>
            <div className="gear-divider" />
            <button className="gear-action" onClick={() => { toggleParams(); setShowGear(false); }}>
              Parameters…
            </button>
            <button className="gear-action gear-reset" onClick={() => { resetToDefaults(); setShowGear(false); }}>
              Reset to defaults
            </button>
          </div>
        )}
      </div>
      {replaySteps?.length > 0 && (
        <button
          className={`replay-btn${showReplay ? ' active' : ''}`}
          onClick={toggleReplay}
          title="Toggle decode replay"
        >▶ Replay</button>
      )}
      <button className="decode-btn" onClick={runDecode} disabled={decoding}>
        {decoding ? '…' : 'Decode'}
      </button>
    </div>
  );
}
