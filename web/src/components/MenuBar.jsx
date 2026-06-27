import React, { useState, useRef, useEffect } from 'react';
import { useStore, PRESETS } from '../store.js';

const TRACE_LEVELS = ['Off', 'Summary', 'Full'];

export default function MenuBar() {
  const {
    showSegmentLayer, toggleSegmentLayer,
    showTrace, toggleTrace,
    showReplay, toggleReplay,
    showResult, toggleResult, decodeResult,
    toggleParams, toggleLlmSettings,
    llmConfig, llmChatOpen, toggleLlmChat,
    params, setTraceLevel, resetToDefaults,
    tileUrl, setTileUrl,
    decoding,
  } = useStore();

  const [showTileMenu,  setShowTileMenu]  = useState(false);
  const [showTraceMenu, setShowTraceMenu] = useState(false);
  const [urlDraft, setUrlDraft]           = useState('');
  const tileMenuRef  = useRef(null);
  const traceMenuRef = useRef(null);
  const traceLevel   = params?.trace_level ?? 'Summary';

  // Sync urlDraft with the active tile URL whenever the menu opens.
  useEffect(() => {
    if (showTileMenu) setUrlDraft(tileUrl || 'http://localhost:5176');
  }, [showTileMenu, tileUrl]);

  // Close tile menu on outside click.
  useEffect(() => {
    if (!showTileMenu) return;
    const handler = (e) => {
      if (tileMenuRef.current && !tileMenuRef.current.contains(e.target))
        setShowTileMenu(false);
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [showTileMenu]);

  // Close trace menu on outside click.
  useEffect(() => {
    if (!showTraceMenu) return;
    const handler = (e) => {
      if (traceMenuRef.current && !traceMenuRef.current.contains(e.target))
        setShowTraceMenu(false);
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [showTraceMenu]);

  function applyTileUrl() {
    const trimmed = urlDraft.trim();
    if (!trimmed) return;
    setTileUrl(trimmed);
    window.location.assign(window.location.pathname);
  }

  return (
    <div className="menu-bar">
      <span className="menu-title">OpenLRLens</span>

      <div className="menu-divider" />

      <button
        className={`menu-btn${showSegmentLayer ? ' active' : ''}`}
        onClick={toggleSegmentLayer}
        title="Toggle road segment layer"
      >Segments</button>

      <button
        className={`menu-btn${showTrace ? ' active' : ''}`}
        onClick={toggleTrace}
        title="Toggle decode trace panel"
      >Trace</button>

      <button
        className={`menu-btn${showReplay ? ' active' : ''}`}
        onClick={toggleReplay}
        title="Toggle step replay bar"
      >Replay</button>

      {decodeResult && (
        <button
          className={`menu-btn${showResult ? ' active' : ''}`}
          onClick={toggleResult}
          title="Toggle results panel"
        >Results</button>
      )}

      <div className="menu-spacer" />

      <button className="menu-btn" onClick={toggleParams} title="Decode parameters">
        Parameters
      </button>

      {/* Trace level dropdown */}
      <div className="menu-tile-wrap" ref={traceMenuRef}>
        <button
          className={`menu-btn${showTraceMenu ? ' active' : ''}`}
          onClick={() => setShowTraceMenu(v => !v)}
          title="Trace detail level"
        >Trace Level</button>

        {showTraceMenu && (
          <div className="menu-tile-dropdown menu-trace-dropdown">
            <div className="menu-tile-label">Trace detail level</div>
            {TRACE_LEVELS.map(lvl => (
              <button
                key={lvl}
                className={`menu-trace-opt${traceLevel === lvl ? ' active' : ''}`}
                onClick={() => { setTraceLevel(lvl); setShowTraceMenu(false); }}
              >{lvl}</button>
            ))}
          </div>
        )}
      </div>

      {llmConfig && (
        <button
          className={`menu-btn${llmChatOpen ? ' active' : ''}`}
          onClick={toggleLlmChat}
          title="AI chat"
        >AI Chat</button>
      )}

      <button
        className={`menu-btn${llmConfig ? ' configured' : ''}`}
        onClick={toggleLlmSettings}
        title="AI / LLM settings"
      >AI{llmConfig ? ' ●' : ''}</button>

      {/* Tile source dropdown */}
      <div className="menu-tile-wrap" ref={tileMenuRef}>
        <button
          className={`menu-btn${showTileMenu ? ' active' : ''}`}
          onClick={() => setShowTileMenu(v => !v)}
          title="Tile source"
        >Tile source</button>

        {showTileMenu && (
          <div className="menu-tile-dropdown">
            <div className="menu-tile-label">Tile server URL</div>
            <div className="menu-tile-row">
              <input
                className="menu-tile-input"
                type="url"
                value={urlDraft}
                onChange={e => setUrlDraft(e.target.value)}
                onKeyDown={e => e.key === 'Enter' && applyTileUrl()}
                spellCheck={false}
                placeholder="http://localhost:5176"
              />
            </div>
            <button
              className="menu-tile-apply"
              onClick={applyTileUrl}
              disabled={!urlDraft.trim()}
            >Apply &amp; reload</button>
            <div className="menu-tile-divider" />
            <button className="menu-tile-action" onClick={() => { resetToDefaults(); setShowTileMenu(false); }}>
              Reset decode params to defaults
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
