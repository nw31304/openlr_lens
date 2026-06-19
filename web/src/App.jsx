import React, { useEffect, useState } from 'react';
import { PMTiles } from 'pmtiles';
import MapView from './components/Map.jsx';
import TopBar from './components/TopBar.jsx';
import ResultPanel from './components/ResultPanel.jsx';
import ParamsPanel from './components/ParamsPanel.jsx';
import TracePanel from './components/TracePanel.jsx';
import ReplayPanel from './components/ReplayPanel.jsx';
import { setPmtiles, setDecoder, setZoom, useStore } from './store.js';
import { initWasm } from './wasm.js';

export default function App() {
  const [ready, setReady] = useState(false);
  const [error, setError] = useState(null);
  const [tilesBase, setTilesBase] = useState('/tiles');

  useEffect(() => {
    async function setup() {
      try {
        const tilesParam = new URLSearchParams(window.location.search).get('tiles') ?? '';
        const isAbsolute = tilesParam.startsWith('http://') || tilesParam.startsWith('https://');
        // In dev, tiles are served by a dedicated HTTP server on :5176 (bypasses Vite).
        // In production (or when an absolute URL is given), use as-is.
        const devTileBase = `http://localhost:5176`;
        const base = isAbsolute ? tilesParam
                   : tilesParam  ? `${devTileBase}/${tilesParam}`
                   : devTileBase;
        setTilesBase(base);

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
      }
    }
    setup();
  }, []);

  if (error) return (
    <div style={{position:'fixed',inset:0,display:'flex',alignItems:'center',justifyContent:'center',background:'#0a0a14',color:'#ff5566',fontFamily:'monospace',fontSize:14,padding:24,textAlign:'center'}}>
      ⚠ Failed to initialize:<br/>{error}
    </div>
  );

  const showReplay = useStore(s => s.showReplay);
  return (
    <div className="app">
      <MapView tilesBase={tilesBase} ready={ready} />
      <TopBar />
      <ParamsPanel />
      <ResultPanel />
      <TracePanel />
      {showReplay && <ReplayPanel />}
    </div>
  );
}
