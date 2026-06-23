import React, { useEffect, useState } from 'react';
import { PMTiles } from 'pmtiles';
import MapView from './components/Map.jsx';
import TopBar from './components/TopBar.jsx';
import ResultPanel from './components/ResultPanel.jsx';
import ParamsPanel from './components/ParamsPanel.jsx';
import TracePanel from './components/TracePanel.jsx';
import LlmSettingsPanel from './components/LlmSettingsPanel.jsx';
import LlmChatPanel from './components/LlmChatPanel.jsx';
import { setPmtiles, setDecoder, setZoom, useStore } from './store.js';
import { initWasm } from './wasm.js';

export default function App() {
  const [ready, setReady] = useState(false);
  const [error, setError] = useState(null);
  const [tilesBase, setTilesBase] = useState('/tiles');
  const [urlDraft, setUrlDraft] = useState('');

  function resolveBase() {
    const tilesParam = new URLSearchParams(window.location.search).get('tiles') ?? '';
    const storedUrl  = useStore.getState().tileUrl || 'http://localhost:5176';
    if (tilesParam) {
      const isAbsolute = tilesParam.startsWith('http://') || tilesParam.startsWith('https://');
      return isAbsolute ? tilesParam : `http://localhost:5176/${tilesParam}`;
    }
    return storedUrl;
  }

  useEffect(() => {
    async function setup() {
      try {
        const base = resolveBase();
        setTilesBase(base);
        setUrlDraft(base);

        console.log('[app] tile base:', base);
        const manifest = await fetch(`${base}/manifest.json`).then(r => r.json());
        const pmtiles = new PMTiles(`${base}/${manifest.archive}`);
        const decoder = await initWasm();

        setPmtiles(pmtiles);
        setDecoder(decoder);
        setZoom(manifest.tile_zoom ?? manifest.zoom ?? 12);
        setReady(true);
      } catch (e) {
        setError(e.message);
        setUrlDraft(resolveBase());
      }
    }
    setup();
  }, []);  // eslint-disable-line react-hooks/exhaustive-deps

  function applyFixedUrl() {
    const trimmed = urlDraft.trim();
    if (!trimmed) return;
    useStore.getState().setTileUrl(trimmed);
    window.location.assign(window.location.pathname);
  }

  if (error) return (
    <div style={{position:'fixed',inset:0,display:'flex',flexDirection:'column',alignItems:'center',justifyContent:'center',gap:16,background:'#0a0a14',color:'#ff5566',fontFamily:'monospace',fontSize:14,padding:24,textAlign:'center'}}>
      <div>⚠ Failed to initialize: {error}</div>
      <div style={{color:'#aaa',fontSize:12}}>Check the tile server URL below and press Apply to retry.</div>
      <div style={{display:'flex',gap:8,alignItems:'center'}}>
        <input
          style={{fontFamily:'monospace',fontSize:13,padding:'6px 10px',borderRadius:4,border:'1px solid #444',background:'#1a1a2e',color:'#eee',width:380}}
          value={urlDraft}
          onChange={e => setUrlDraft(e.target.value)}
          onKeyDown={e => e.key === 'Enter' && applyFixedUrl()}
          placeholder="http://localhost:5176"
        />
        <button
          style={{padding:'6px 14px',fontFamily:'monospace',fontSize:13,borderRadius:4,border:'none',background:'#3355cc',color:'#fff',cursor:'pointer'}}
          onClick={applyFixedUrl}
        >Apply</button>
      </div>
    </div>
  );

  return (
    <div className="app">
      <MapView tilesBase={tilesBase} ready={ready} />
      <TopBar />
      <ParamsPanel />
      <LlmSettingsPanel />
      <LlmChatPanel />
      <ResultPanel />
      <TracePanel />
    </div>
  );
}
