import maplibregl from 'maplibre-gl';
import { PMTiles }  from 'pmtiles';
import { decodeTile } from './decoder.js';

// ── Constants ──────────────────────────────────────────────────────────────────

const TILE_ZOOM = 12;
const MIN_LOAD_ZOOM = 10;  // don't fetch routing tiles below this zoom — too many requests

// Vivid colours that stand out against both light and dark basemap backgrounds
const FRC_COLOR = ['#e8002d', '#ff7700', '#e8c800', '#00aa44',
                   '#00aaff', '#0055ff', '#aa00ff', '#888888'];
const FRC_LABEL = ['0 · Motorway', '1 · Trunk/Primary', '2 · Secondary', '3 · Tertiary',
                   '4 · Unclassified', '5 · Residential', '6 · Svc/Living St', '7 · Other'];
const FRC_WIDTH = [4, 3, 2.5, 2, 1.5, 1.5, 1.2, 1];  // base widths per FRC

// ── Slippy tile helpers ────────────────────────────────────────────────────────

function lngLatToTile(lng, lat, z) {
  const n = 2 ** z;
  const latRad = lat * Math.PI / 180;
  const x = Math.floor((lng + 180) / 360 * n);
  const y = Math.floor((1 - Math.log(Math.tan(latRad) + 1 / Math.cos(latRad)) / Math.PI) / 2 * n);
  return [Math.max(0, Math.min(n - 1, x)), Math.max(0, Math.min(n - 1, y))];
}

function tilesForBounds(bounds, z) {
  const [x0, y0] = lngLatToTile(bounds.getWest(),  bounds.getNorth(), z);
  const [x1, y1] = lngLatToTile(bounds.getEast(),  bounds.getSouth(), z);
  const tiles = [];
  for (let x = x0; x <= x1; x++)
    for (let y = y0; y <= y1; y++)
      tiles.push({ z, x, y });
  return tiles;
}

// ── App ────────────────────────────────────────────────────────────────────────

async function init() {
  // Allow ?tiles=nz-osm (or any subdir under out/) to pick the tile set.
  const tilesParam = new URLSearchParams(window.location.search).get('tiles') ?? '';
  const tilesBase  = tilesParam ? `/tiles/${tilesParam}` : '/tiles';

  const manifest = await fetch(`${tilesBase}/manifest.json`).then(r => r.json());
  console.log('Manifest:', manifest);

  const pmtiles = new PMTiles(`${tilesBase}/${manifest.archive}`);

  const tileCache = new Map();   // tileKey → GeoJSON Feature[]
  let   pendingCount = 0;

  const statusEl = document.getElementById('status');
  function setStatus(msg) {
    if (msg) { statusEl.textContent = msg; statusEl.classList.remove('hidden'); }
    else      { statusEl.classList.add('hidden'); }
  }

  // ── MapLibre ──────────────────────────────────────────────────────────────

  const map = new maplibregl.Map({
    container: 'map',
    style:     'https://tiles.openfreemap.org/styles/liberty',
    center:    [172.6, -41.3],
    zoom:      6,
    hash:      true,
  });

  map.addControl(new maplibregl.NavigationControl(), 'top-right');

  map.on('load', () => {
    console.log('Map loaded');
    buildLegend();

    map.addSource('olr-segments', {
      type: 'geojson',
      data: { type: 'FeatureCollection', features: [] },
    });

    // One layer per FRC so widths and colours are independent and predictable
    for (let frc = 0; frc < 8; frc++) {
      map.addLayer({
        id:     `olr-frc${frc}`,
        type:   'line',
        source: 'olr-segments',
        filter: ['==', ['get', 'frc'], frc],
        paint: {
          'line-color': FRC_COLOR[frc],
          'line-width': ['interpolate', ['linear'], ['zoom'], 10, FRC_WIDTH[frc] * 0.6, 16, FRC_WIDTH[frc] * 2],
          'line-opacity': 0.9,
        },
      });
    }

    // Highlight layer — activated on click
    map.addLayer({
      id:     'olr-highlight',
      type:   'line',
      source: 'olr-segments',
      filter: ['boolean', false],   // nothing highlighted initially
      paint: {
        'line-color':   '#00aaff',
        'line-width':   8,
        'line-opacity': 1,
      },
    });

    // Click handlers on all FRC layers
    for (let frc = 0; frc < 8; frc++) {
      map.on('click', `olr-frc${frc}`, onSegmentClick);
      map.on('mouseenter', `olr-frc${frc}`, () => map.getCanvas().style.cursor = 'pointer');
      map.on('mouseleave', `olr-frc${frc}`, () => map.getCanvas().style.cursor = '');
    }

    map.on('click', onMapClick);

    loadVisibleTiles();
  });

  map.on('moveend', loadVisibleTiles);
  map.on('zoomend', loadVisibleTiles);

  // ── Tile loading ───────────────────────────────────────────────────────────

  async function loadVisibleTiles() {
    const zoom = map.getZoom();
    if (zoom < MIN_LOAD_ZOOM) {
      setStatus(`Zoom in past ${MIN_LOAD_ZOOM} to load road segments`);
      return;
    }
    setStatus(null);

    const tiles   = tilesForBounds(map.getBounds(), TILE_ZOOM);
    const missing = tiles.filter(({ z, x, y }) => !tileCache.has(`${z}/${x}/${y}`));
    console.log(`Viewport: ${tiles.length} tiles, ${missing.length} to fetch`);
    if (missing.length === 0) { rebuildSource(tiles); return; }

    pendingCount += missing.length;
    setStatus(`Loading ${pendingCount} tile${pendingCount > 1 ? 's' : ''}…`);

    await Promise.all(missing.map(async ({ z, x, y }) => {
      const key = `${z}/${x}/${y}`;
      try {
        console.log(`Fetching tile ${key}…`);
        const result = await pmtiles.getZxy(z, x, y);
        console.log(`Tile ${key} result:`, result ? `data ${result.data?.byteLength}B` : 'null');
        if (result?.data) {
          const fc = decodeTile(result.data, z, x, y);
          tileCache.set(key, fc.features);
          console.log(`Tile ${key}: ${fc.features.length} segments`);
        } else {
          tileCache.set(key, []);  // tile not in archive (outside extent)
          console.warn(`Tile ${key}: not in archive (result=${JSON.stringify(result)})`);
        }
      } catch (e) {
        console.error(`Tile ${key} failed:`, e);
        tileCache.set(key, []);
      } finally {
        pendingCount = Math.max(0, pendingCount - 1);
        if (pendingCount === 0) setStatus(null);
      }
    }));

    rebuildSource(tiles);
  }

  function rebuildSource(visibleTiles) {
    const visibleKeys = new Set(visibleTiles.map(({ z, x, y }) => `${z}/${x}/${y}`));
    const features = [];
    for (const [key, feats] of tileCache) {
      if (visibleKeys.has(key)) features.push(...feats);
    }
    console.log(`Rebuilding source: ${features.length} total segments`);
    map.getSource('olr-segments').setData({ type: 'FeatureCollection', features });
  }

  // ── Click interaction ──────────────────────────────────────────────────────

  function onSegmentClick(e) {
    if (!e.features?.length) return;
    const props = e.features[0].properties;

    map.setFilter('olr-highlight', ['all',
      ['==', ['get', 'tile'],        props.tile],
      ['==', ['get', 'local_index'], props.local_index],
    ]);

    showInfo(props);
    e.originalEvent.stopPropagation();
  }

  function onMapClick(e) {
    // MapLibre fires this after layer-specific handlers even with stopPropagation,
    // so check explicitly whether the click landed on any segment layer.
    const layerIds = Array.from({ length: 8 }, (_, i) => `olr-frc${i}`);
    const hits = map.queryRenderedFeatures(e.point, { layers: layerIds });
    if (hits.length > 0) return; // segment handler already handled it
    map.setFilter('olr-highlight', ['boolean', false]);
    hideInfo();
  }

  // ── Info panel ─────────────────────────────────────────────────────────────

  const panel   = document.getElementById('info-panel');
  const infoBody = document.getElementById('info-body');
  document.getElementById('close-info').addEventListener('click', () => {
    map.setFilter('olr-highlight', ['boolean', false]);
    hideInfo();
  });

  function showInfo(props) {
    const rows = [
      ['FRC',       `${props.frc} — ${props.frc_name}`],
      ['FOW',       `${props.fow} — ${props.fow_name}`],
      ['Direction', props.direction],
      ['Length',    `${props.length_m} m`],
      ['Tile',      props.tile],
      ['Index',     props.local_index],
    ];
    infoBody.innerHTML = `<table>${
      rows.map(([k, v]) => `<tr><td>${k}</td><td><b>${v}</b></td></tr>`).join('')
    }</table>`;
    panel.classList.add('visible');
  }

  function hideInfo() { panel.classList.remove('visible'); }

  // ── Legend ─────────────────────────────────────────────────────────────────

  function buildLegend() {
    const legend = document.getElementById('legend');
    legend.innerHTML = '<h4>FRC</h4>' + FRC_LABEL.map((label, i) => `
      <div class="legend-row">
        <div class="legend-swatch" style="background:${FRC_COLOR[i]};border:1px solid #aaa"></div>
        <span>${label}</span>
      </div>`).join('');
  }
}

init().catch(err => {
  console.error('Init failed:', err);
  document.getElementById('status').textContent = `Error: ${err.message}`;
  document.getElementById('status').classList.remove('hidden');
});
