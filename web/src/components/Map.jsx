import React, { useEffect, useRef, useState } from 'react';
import maplibregl from 'maplibre-gl';
import 'maplibre-gl/dist/maplibre-gl.css';
import { PMTiles } from 'pmtiles';
import { useStore, getSegmentId, getSegGeomCache, getSegIdToTile, getTileGeomCache } from '../store.js';
import { useDraggable } from '../hooks.js';
import { emptyState, applyStep, computeVisualState, stateToGeoJSON } from '../replayEngine.js';


function popupStyle(anchor, w = 260, h = 200) {
  if (!anchor) return undefined;
  const margin = 12;
  let left = anchor.x + margin;
  let top  = anchor.y + margin;
  if (left + w > window.innerWidth  - margin) left = anchor.x - w - margin;
  if (top  + h > window.innerHeight - margin) top  = anchor.y - h - margin;
  return { position: 'absolute', left: Math.max(margin, left), top: Math.max(margin, top), right: 'auto', bottom: 'auto' };
}
import { decodeTile } from '../tileDecoder.js';

// ── Constants ──────────────────────────────────────────────────────────────────

const TILE_ZOOM = 12;
const MIN_LOAD_ZOOM = 10;

// ── Basemap definitions ────────────────────────────────────────────────────────

function rasterStyle(tiles, attribution, maxzoom = 19) {
  return {
    version: 8,
    sources: {
      basemap: { type: 'raster', tiles, tileSize: 256, attribution, maxzoom },
    },
    layers: [{ id: 'basemap', type: 'raster', source: 'basemap' }],
  };
}

const BASEMAPS = [
  { id: 'liberty',     label: 'Liberty',      style: 'https://tiles.openfreemap.org/styles/liberty' },
  { id: 'bright',      label: 'Bright',       style: 'https://tiles.openfreemap.org/styles/bright' },
  { id: 'positron',    label: 'Positron',     style: 'https://tiles.openfreemap.org/styles/positron' },
  { id: 'osm',         label: 'OSM',          style: rasterStyle(
    ['https://tile.openstreetmap.org/{z}/{x}/{y}.png'],
    '© <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors') },
  { id: 'carto-light', label: 'Carto Light',  style: rasterStyle(
    ['https://a.basemaps.cartocdn.com/light_all/{z}/{x}/{y}.png',
     'https://b.basemaps.cartocdn.com/light_all/{z}/{x}/{y}.png',
     'https://c.basemaps.cartocdn.com/light_all/{z}/{x}/{y}.png'],
    '© <a href="https://carto.com/attributions">CARTO</a> © <a href="https://www.openstreetmap.org/copyright">OSM</a> contributors') },
  { id: 'carto-dark',  label: 'Carto Dark',   style: rasterStyle(
    ['https://a.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}.png',
     'https://b.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}.png',
     'https://c.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}.png'],
    '© <a href="https://carto.com/attributions">CARTO</a> © <a href="https://www.openstreetmap.org/copyright">OSM</a> contributors') },
  { id: 'satellite',   label: 'Satellite',    style: rasterStyle(
    ['https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}'],
    'Tiles © Esri — Esri, Maxar, Earthstar Geographics') },
];

// Custom sources/layers to preserve across basemap switches via transformStyle.
const CUSTOM_SOURCES = new Set([
  'olr-segments', 'decoded-path', 'lrp-markers',
  'lrp-snap', 'lrp-displacement',
  'offset-uncertainty', 'lrp-bearing', 'highlighted-segment', 'trace-segment',
  'replay-radius', 'replay-route', 'replay-candidates', 'replay-cloud', 'replay-frontier', 'replay-leg', 'replay-flash',
]);
const CUSTOM_LAYER_IDS = new Set([
  'olr-frc0','olr-frc1','olr-frc2','olr-frc3','olr-frc4','olr-frc5','olr-frc6','olr-frc7',
  'olr-highlight', 'decoded-path-line', 'lrp-markers-circle',
  'lrp-displacement-line', 'lrp-displacement-arrow',
  'offset-uncertainty-line',
  'lrp-bearing-fill', 'lrp-bearing-outline',
  'highlighted-segment-halo', 'highlighted-segment-line',
  'trace-segment-halo', 'trace-segment-line',
  'replay-radius-fill', 'replay-radius-line',
  'replay-route-line',
  'replay-candidates-circle',
  'replay-cloud-circle',
  'replay-frontier-circle',
  'replay-leg-from', 'replay-leg-to',
  'replay-flash-ring',
]);

const FRC_COLOR = ['#e8002d', '#ff7700', '#e8c800', '#00aa44',
                   '#00aaff', '#0055ff', '#aa00ff', '#888888'];
const FRC_LABEL = ['0 · Motorway', '1 · Trunk/Primary', '2 · Secondary', '3 · Tertiary',
                   '4 · Unclassified', '5 · Residential', '6 · Svc/Living St', '7 · Other'];
const FRC_WIDTH = [4, 3, 2.5, 2, 1.5, 1.5, 1.2, 1];

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

// ── LRP bearing helper ─────────────────────────────────────────────────────────

function formatBearing(lb, ub) {
  if (Math.abs(ub - lb) < 0.1) return `${lb.toFixed(1)}°`;
  return `${lb.toFixed(1)}° – ${ub.toFixed(1)}°`;
}

function parseWktLinestring(wkt) {
  const m = wkt?.match(/LINESTRING\s*\(([^)]+)\)/i);
  if (!m) return null;
  return m[1].trim().split(',').map(pair => {
    const [lon, lat] = pair.trim().split(/\s+/).map(Number);
    return [lon, lat];
  });
}

function destinationPoint(lon, lat, bearingDeg, distM) {
  const R = 6371000;
  const φ1 = lat * Math.PI / 180;
  const λ1 = lon * Math.PI / 180;
  const θ  = bearingDeg * Math.PI / 180;
  const δ  = distM / R;
  const φ2 = Math.asin(Math.sin(φ1)*Math.cos(δ) + Math.cos(φ1)*Math.sin(δ)*Math.cos(θ));
  const λ2 = λ1 + Math.atan2(Math.sin(θ)*Math.sin(δ)*Math.cos(φ1), Math.cos(δ) - Math.sin(φ1)*Math.sin(φ2));
  return [λ2 * 180 / Math.PI, φ2 * 180 / Math.PI];
}

function bearingConeGeoJSON(lon, lat, lbDeg, ubDeg, radiusM) {
  const center = [lon, lat];
  let start = lbDeg;
  let sweep = ((ubDeg - lbDeg) + 360) % 360;
  if (sweep < 2) { start -= (2 - sweep) / 2; sweep = 2; } // minimum visual width for TPEG
  const STEPS = 48;
  const ring = [center];
  for (let i = 0; i <= STEPS; i++) ring.push(destinationPoint(lon, lat, start + sweep * i / STEPS, radiusM));
  ring.push(center);
  return { type: 'FeatureCollection', features: [{ type: 'Feature', geometry: { type: 'Polygon', coordinates: [ring] }, properties: {} }] };
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function compassBearing(lon1, lat1, lon2, lat2) {
  const dLon = (lon2 - lon1) * Math.PI / 180;
  const φ1 = lat1 * Math.PI / 180, φ2 = lat2 * Math.PI / 180;
  const y = Math.sin(dLon) * Math.cos(φ2);
  const x = Math.cos(φ1) * Math.sin(φ2) - Math.sin(φ1) * Math.cos(φ2) * Math.cos(dLon);
  return (Math.atan2(y, x) * 180 / Math.PI + 360) % 360;
}

// ── Custom marker images ──────────────────────────────────────────────────────

function addMapImages(map) {
  // Displacement arrowhead — points north (↑) by default, tip at top-center.
  // Placed at the snap coordinate with icon-anchor:'top' and rotated by LRP→snap
  // bearing, so the tip always lands on the snap point and shaft trails toward LRP.
  const aw = 12, ah = 16;
  const arrowCanvas = document.createElement('canvas');
  arrowCanvas.width = aw; arrowCanvas.height = ah;
  const ac = arrowCanvas.getContext('2d');
  ac.clearRect(0, 0, aw, ah);
  ac.beginPath();
  ac.moveTo(aw / 2, 1);       // tip — top-center
  ac.lineTo(1,      ah - 1);  // bottom-left
  ac.lineTo(aw - 1, ah - 1);  // bottom-right
  ac.closePath();
  ac.fillStyle   = 'rgba(255,255,255,0.9)';
  ac.strokeStyle = 'rgba(0,0,0,0.6)';
  ac.lineWidth   = 1.5;
  ac.fill(); ac.stroke();
  map.addImage('displacement-arrow', ac.getImageData(0, 0, aw, ah));

}

// ── Map Component ──────────────────────────────────────────────────────────────

export default function MapView({ tilesBase, ready }) {
  const mapContainer    = useRef(null);
  const mapRef          = useRef(null);
  const tileCacheRef    = useRef(new Map());
  const pendingCountRef = useRef(0);
  const pmtilesRef      = useRef(null);
  const pulseRef        = useRef(null);
  const frontierPulseRef = useRef(null);
  const lrpPanelRef     = useRef(null);
  const segPanelRef     = useRef(null);
  // Incremental replay state — avoids O(N²) recomputation when stepping forward
  const replayVisualRef = useRef(null);   // last computed visual state
  const replayStepRef   = useRef(-1);     // step index of replayVisualRef
  const replayStepsKey  = useRef(null);   // identity check for replaySteps array
  const flashAnimRef    = useRef(null);   // rAF handle for sonar-ping fade animation
  const routePulseRef   = useRef(null);   // rAF handle for route-found pulse animation
  const candPanelRef    = useRef(null);

  const [status, setStatus] = useState(null);
  const [infoProps, setInfoProps] = useState(null);
  const [infoAnchor, setInfoAnchor] = useState(null);
  const [lrpInfo, setLrpInfo] = useState(null);
  const [candInfo, setCandInfo] = useState(null);
  const [candAnchor, setCandAnchor] = useState(null);
  const [basemap, setBasemap] = useState('liberty');

  const { pos: lrpPos,  onMouseDown: lrpMouseDown,  resetPos: lrpResetPos  } = useDraggable(lrpPanelRef);
  const { pos: segPos,  onMouseDown: segMouseDown,  resetPos: segResetPos  } = useDraggable(segPanelRef);
  const { pos: candPos, onMouseDown: candMouseDown, resetPos: candResetPos } = useDraggable(candPanelRef);

  const decodeResult          = useStore(s => s.decodeResult);
  const highlightedSegment    = useStore(s => s.highlightedSegment);
  const setHighlightedSegment = useStore(s => s.setHighlightedSegment);
  const traceHighlightSegIds  = useStore(s => s.traceHighlightSegIds);
  const traceLrpFocus         = useStore(s => s.traceLrpFocus);
  const setTraceLrpFocus      = useStore(s => s.setTraceLrpFocus);
  const showSegmentLayer      = useStore(s => s.showSegmentLayer);
  const searchRadiusM         = useStore(s => s.params.candidate_search_radius_m);
  const replayStep  = useStore(s => s.replayStep);
  const replaySteps = useStore(s => s.replaySteps);
  const replayStats = useStore(s => s.replayStats);
  const showReplay  = useStore(s => s.showReplay);

  // Reset drag position when a new popup target is clicked
  useEffect(() => { lrpResetPos(); }, [lrpInfo]);   // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => { segResetPos(); }, [infoProps]);  // eslint-disable-line react-hooks/exhaustive-deps

  // Always-current ref so the highlight effect can read decodeResult without
  // adding it to the dependency array (which would cause both effects to race).
  const decodeResultRef = useRef(decodeResult);
  useEffect(() => { decodeResultRef.current = decodeResult; }, [decodeResult]);

  // Store tilesBase in a ref so the loadVisibleTiles callback can see the latest value
  const tilesBaseRef = useRef(tilesBase);
  useEffect(() => { tilesBaseRef.current = tilesBase; }, [tilesBase]);

  // ── Init map ────────────────────────────────────────────────────────────────

  useEffect(() => {
    if (mapRef.current) return; // already initialized

    const map = new maplibregl.Map({
      container: mapContainer.current,
      style:     'https://tiles.openfreemap.org/styles/liberty',
      center:    [10, 48],
      zoom:      4,
      hash:      true,
    });
    mapRef.current = map;

    map.addControl(new maplibregl.NavigationControl(), 'top-right');

    // Re-add custom images whenever the style reloads (initial load + basemap switches).
    map.on('style.load', () => addMapImages(map));

    map.on('load', () => {
      // ── OLR segment source ────────────────────────────────────────────────
      map.addSource('olr-segments', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });

      for (let frc = 0; frc < 8; frc++) {
        map.addLayer({
          id:     `olr-frc${frc}`,
          type:   'line',
          source: 'olr-segments',
          filter: ['==', ['get', 'frc'], frc],
          layout: { visibility: 'none' }, // hidden until user enables Segments layer
          paint: {
            'line-color': FRC_COLOR[frc],
            'line-width': ['interpolate', ['linear'], ['zoom'], 10, FRC_WIDTH[frc] * 2.0, 16, FRC_WIDTH[frc] * 5.5],
            'line-opacity': 0.9,
          },
        });
      }

      // Highlight layer — activated on click or result-panel selection
      map.addLayer({
        id:     'olr-highlight',
        type:   'line',
        source: 'olr-segments',
        filter: ['boolean', false],
        layout: { visibility: 'none' }, // follows segment layer visibility
        paint: {
          'line-color':   '#ffe000',
          'line-width':   6,
          'line-opacity': 1,
        },
      });

      // ── Decoded path source + layer ───────────────────────────────────────
      map.addSource('decoded-path', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });

      map.addLayer({
        id:     'decoded-path-line',
        type:   'line',
        source: 'decoded-path',
        paint: {
          'line-color':   '#00d4ff',
          'line-width':   5,
          'line-opacity': 0.9,
        },
      });

      // ── Offset uncertainty caps (v3 [LB, UB] zone at path head/tail) ────
      // Path is now trimmed at LB, so these caps sit at the very START/END of
      // the solid path — no overlap.  Darker cyan dashes read as "same thing,
      // but uncertain" without any z-order tricks.
      map.addSource('offset-uncertainty', { type: 'geojson', data: { type: 'FeatureCollection', features: [] } });
      map.addLayer({
        id: 'offset-uncertainty-line', type: 'line', source: 'offset-uncertainty',
        paint: {
          'line-color':     '#0088bb',
          'line-width':     5,
          'line-opacity':   0.95,
          'line-dasharray': [1, 0.5],
        },
      });

      // ── LRP marker source + layer ─────────────────────────────────────────
      map.addSource('lrp-markers', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });

      map.addLayer({
        id:     'lrp-markers-circle',
        type:   'circle',
        source: 'lrp-markers',
        paint: {
          'circle-radius':       7,
          'circle-color': [
            'case',
            ['==', ['get', 'index'], 0],                              '#00bb44', // first  → green
            ['==', ['get', 'index'], ['-', ['get', 'total'], 1]],     '#ee2222', // last   → red
            '#0088ff',                                                            // middle → blue
          ],
          'circle-stroke-width': 2,
          'circle-stroke-color': '#ffffff',
        },
      });

      // ── LRP snap displacement lines (encoded coord → snap point) ─────────
      map.addSource('lrp-displacement', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });
      map.addLayer({
        id: 'lrp-displacement-line', type: 'line', source: 'lrp-displacement',
        paint: {
          'line-color':     '#000000',
          'line-width':     1.5,
          'line-opacity':   0.7,
          'line-dasharray': [3, 4],
        },
      }, 'lrp-markers-circle');

      // ── LRP snap arrowhead source (arrow tip at snap coord, rotated to LRP→snap bearing) ──
      map.addSource('lrp-snap', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });

      // ── Arrowhead at snap point (tip on road, shaft trailing toward LRP) ────
      map.addLayer({
        id: 'lrp-displacement-arrow', type: 'symbol', source: 'lrp-snap',
        layout: {
          'icon-image':             'displacement-arrow',
          'icon-rotate':            ['get', 'bearing'],
          'icon-rotation-alignment': 'map',
          'icon-anchor':            'top',   // tip of arrow at snap coord; shaft trails back
          'icon-size':              1.0,
          'icon-allow-overlap':     true,
          'icon-ignore-placement':  true,
        },
      }, 'lrp-markers-circle');


      // ── LRP bearing cone (shown when an LRP is selected) ─────────────────
      map.addSource('lrp-bearing', { type: 'geojson', data: { type: 'FeatureCollection', features: [] } });
      map.addLayer({
        id: 'lrp-bearing-fill', type: 'fill', source: 'lrp-bearing',
        paint: { 'fill-color': '#aa00ff', 'fill-opacity': 0.18 },
      }, 'lrp-markers-circle');
      map.addLayer({
        id: 'lrp-bearing-outline', type: 'line', source: 'lrp-bearing',
        paint: { 'line-color': '#aa00ff', 'line-width': 1.5, 'line-opacity': 0.8 },
      }, 'lrp-markers-circle');

      // ── Highlighted segment (sits above everything else) ──────────────────
      map.addSource('highlighted-segment', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });

      map.addLayer({
        id:     'highlighted-segment-halo',
        type:   'line',
        source: 'highlighted-segment',
        paint: {
          'line-color':   '#ffffff',
          'line-width':   14,
          'line-opacity': 0.6,
        },
      });

      map.addLayer({
        id:     'highlighted-segment-line',
        type:   'line',
        source: 'highlighted-segment',
        paint: {
          'line-color':   '#ffe000',
          'line-width':   6,
          'line-opacity': 1,
        },
      });

      // ── Trace-panel highlight (separate from result-panel highlight) ───────
      map.addSource('trace-segment', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });
      map.addLayer({
        id:     'trace-segment-halo',
        type:   'line',
        source: 'trace-segment',
        paint: {
          'line-color':   '#ff8c00',
          'line-width':   14,
          'line-opacity': 0.4,
          'line-blur':    3,
        },
      });
      map.addLayer({
        id:     'trace-segment-line',
        type:   'line',
        source: 'trace-segment',
        paint: {
          'line-color':   '#ff8c00',
          'line-width':   4,
          'line-opacity': 1,
        },
      });

      // ── Replay sources & layers ──────────────────────────────────────────
      const emptyFC = { type: 'FeatureCollection', features: [] };

      map.addSource('replay-radius',     { type: 'geojson', data: emptyFC });
      map.addSource('replay-route',      { type: 'geojson', data: emptyFC });
      map.addSource('replay-candidates', { type: 'geojson', data: emptyFC });
      map.addSource('replay-cloud',      { type: 'geojson', data: emptyFC });
      map.addSource('replay-frontier',   { type: 'geojson', data: emptyFC });
      map.addSource('replay-leg',        { type: 'geojson', data: emptyFC });

      // Search radius ring — pulsing fill + dashed border
      map.addLayer({
        id: 'replay-radius-fill', type: 'fill', source: 'replay-radius',
        paint: { 'fill-color': '#aa44ff', 'fill-opacity': 0.06 },
      });
      map.addLayer({
        id: 'replay-radius-line', type: 'line', source: 'replay-radius',
        paint: { 'line-color': '#cc66ff', 'line-width': 2, 'line-opacity': 0.85, 'line-dasharray': [4, 3] },
      });

      // Found route — pulsing line, updated each time a route_found step fires
      map.addLayer({
        id: 'replay-route-line', type: 'line', source: 'replay-route',
        paint: { 'line-color': '#ffe066', 'line-width': 4, 'line-opacity': 0.85, 'line-cap': 'round', 'line-join': 'round' },
      });

      // Candidate snap points — colour by verdict type
      map.addLayer({
        id: 'replay-candidates-circle', type: 'circle', source: 'replay-candidates',
        paint: {
          // Winners (chosen leg endpoints) are larger with a white ring.
          'circle-radius': ['case',
            ['boolean', ['get', 'winner'], false], 10,
            ['==', ['get', 'ctype'], 'accepted'],   7,
            5,
          ],
          'circle-opacity': 0.95,
          'circle-stroke-width': ['case',
            ['boolean', ['get', 'winner'], false], 3,
            ['==', ['get', 'ctype'], 'accepted'],   2,
            1,
          ],
          'circle-stroke-color': ['case',
            ['boolean', ['get', 'winner'], false], '#ffffff',
            'rgba(0,0,0,0.5)',
          ],
          'circle-color': ['match', ['get', 'ctype'],
            'accepted',  '#00ff88',
            'bearing',   '#ff8c00',
            'radius',    '#ffdd00',
            'score',     '#cc44ff',
            'direction', '#556677',
            /* default */ '#aaaaaa',
          ],
        },
      });

      // A* expansion cloud — pre-computed colour per node
      map.addLayer({
        id: 'replay-cloud-circle', type: 'circle', source: 'replay-cloud',
        paint: {
          'circle-radius':  3,
          'circle-opacity': 0.7,
          'circle-color':   ['get', 'color'],
          'circle-stroke-width': 0,
        },
      });

      // Frontier — bright white pulsing nodes
      map.addLayer({
        id: 'replay-frontier-circle', type: 'circle', source: 'replay-frontier',
        paint: {
          'circle-radius':       6,
          'circle-color':        '#ffffff',
          'circle-opacity':      0.95,
          'circle-blur':         0.3,
          'circle-stroke-width': 1.5,
          'circle-stroke-color': '#88ccff',
        },
      });

      // Leg from/to markers
      map.addLayer({
        id: 'replay-leg-from', type: 'circle', source: 'replay-leg',
        filter: ['==', ['get', 'role'], 'from'],
        paint: { 'circle-radius': 9, 'circle-color': '#00ff88', 'circle-stroke-width': 2, 'circle-stroke-color': '#fff' },
      });
      map.addLayer({
        id: 'replay-leg-to', type: 'circle', source: 'replay-leg',
        filter: ['==', ['get', 'role'], 'to'],
        paint: { 'circle-radius': 9, 'circle-color': '#ff4444', 'circle-stroke-width': 2, 'circle-stroke-color': '#fff' },
      });

      // Sonar-ping ring — tracks the latest A* node; animated via RAF in the replay effect.
      map.addSource('replay-flash', { type: 'geojson', data: emptyFC });
      map.addLayer({
        id: 'replay-flash-ring', type: 'circle', source: 'replay-flash',
        paint: {
          'circle-radius':         20,
          'circle-color':          'transparent',
          'circle-stroke-width':   2.5,
          'circle-stroke-color':   '#00eeff',
          'circle-stroke-opacity': 1.0,
          'circle-opacity':        0,
        },
      });

      // ── Click handlers ────────────────────────────────────────────────────
      for (let frc = 0; frc < 8; frc++) {
        map.on('click', `olr-frc${frc}`, onSegmentClick);
        map.on('mouseenter', `olr-frc${frc}`, () => map.getCanvas().style.cursor = 'pointer');
        map.on('mouseleave', `olr-frc${frc}`, () => map.getCanvas().style.cursor = '');
      }

      map.on('click', 'lrp-markers-circle', onLrpClick);
      map.on('mouseenter', 'lrp-markers-circle', () => map.getCanvas().style.cursor = 'pointer');
      map.on('mouseleave', 'lrp-markers-circle', () => map.getCanvas().style.cursor = '');

      map.on('click', 'replay-candidates-circle', (e) => {
        const props = e.features?.[0]?.properties;
        if (!props) return;
        setCandInfo(props);
        setCandAnchor({ x: e.point.x, y: e.point.y });
        e.stopPropagation?.();
      });
      map.on('mouseenter', 'replay-candidates-circle', () => map.getCanvas().style.cursor = 'pointer');
      map.on('mouseleave', 'replay-candidates-circle', () => map.getCanvas().style.cursor = '');

      map.on('click', 'decoded-path-line', onDecodedPathClick);
      map.on('mouseenter', 'decoded-path-line', () => map.getCanvas().style.cursor = 'pointer');
      map.on('mouseleave', 'decoded-path-line', () => map.getCanvas().style.cursor = '');

      map.on('click', onMapClick);

      loadVisibleTiles(map);
    });

    map.on('moveend', () => loadVisibleTiles(map));
    map.on('zoomend', () => loadVisibleTiles(map));

    return () => {
      if (pulseRef.current)         { cancelAnimationFrame(pulseRef.current);         pulseRef.current         = null; }
      if (frontierPulseRef.current) { cancelAnimationFrame(frontierPulseRef.current); frontierPulseRef.current = null; }
      if (routePulseRef.current)    { cancelAnimationFrame(routePulseRef.current);    routePulseRef.current    = null; }
      if (flashAnimRef.current)     { cancelAnimationFrame(flashAnimRef.current);     flashAnimRef.current     = null; }
      map.remove();
      mapRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ── Basemap switch ───────────────────────────────────────────────────────────

  function handleBasemapChange(id) {
    const map = mapRef.current;
    const entry = BASEMAPS.find(b => b.id === id);
    if (!map || !entry) return;
    map.setStyle(entry.style, {
      transformStyle: (previous, next) => ({
        ...next,
        sources: {
          ...next.sources,
          ...Object.fromEntries(
            Object.entries(previous.sources ?? {}).filter(([k]) => CUSTOM_SOURCES.has(k))
          ),
        },
        layers: [
          ...next.layers,
          ...(previous.layers ?? []).filter(l => CUSTOM_LAYER_IDS.has(l.id)),
        ],
      }),
    });
    setBasemap(id);
  }

  // ── Tile loading ─────────────────────────────────────────────────────────────

  async function loadVisibleTiles(map) {
    if (!map.isStyleLoaded()) return;
    const zoom = map.getZoom();
    if (zoom < MIN_LOAD_ZOOM) {
      setStatus(`Zoom in past ${MIN_LOAD_ZOOM} to load road segments`);
      return;
    }
    setStatus(null);

    // Ensure we have a PMTiles reader for the tile inspector.
    // We create a separate reader here (the decoder in store.js uses its own instance).
    // Both instances share the same underlying HTTP cache via the browser.
    if (!pmtilesRef.current) {
      try {
        const manifest = await fetch(`${tilesBaseRef.current}/manifest.json`).then(r => r.json());
        pmtilesRef.current = new PMTiles(`${tilesBaseRef.current}/${manifest.archive}`);
      } catch {
        return;
      }
    }

    const tileCache = tileCacheRef.current;
    const tiles   = tilesForBounds(map.getBounds(), TILE_ZOOM);
    const missing = tiles.filter(({ z, x, y }) => !tileCache.has(`${z}/${x}/${y}`));
    if (missing.length === 0) { rebuildSource(map, tiles); return; }

    pendingCountRef.current += missing.length;
    setStatus(`Loading ${pendingCountRef.current} tile${pendingCountRef.current > 1 ? 's' : ''}…`);

    await Promise.all(missing.map(async ({ z, x, y }) => {
      const key = `${z}/${x}/${y}`;
      try {
        const result = await pmtilesRef.current.getZxy(z, x, y);
        if (result?.data) {
          const fc = decodeTile(result.data, z, x, y);
          tileCache.set(key, fc.features);
        } else {
          tileCache.set(key, []);
        }
      } catch (e) {
        console.error(`Tile ${key} failed:`, e);
        tileCache.set(key, []);
      } finally {
        pendingCountRef.current = Math.max(0, pendingCountRef.current - 1);
        if (pendingCountRef.current === 0) setStatus(null);
      }
    }));

    rebuildSource(map, tiles);
  }

  function rebuildSource(map, visibleTiles) {
    if (!map.getSource('olr-segments')) return;
    const visibleKeys = new Set(visibleTiles.map(({ z, x, y }) => `${z}/${x}/${y}`));
    const features = [];
    for (const [key, feats] of tileCacheRef.current) {
      if (visibleKeys.has(key)) features.push(...feats);
    }
    map.getSource('olr-segments').setData({ type: 'FeatureCollection', features });
  }

  // ── Click interaction ────────────────────────────────────────────────────────

  function onSegmentClick(e) {
    if (!e.features?.length) return;
    const props = e.features[0].properties;
    const [z, x, y] = props.tile.split('/').map(Number);
    const segId = getSegmentId(z, x, y, props.local_index);
    setHighlightedSegment({ tile: props.tile, local_index: props.local_index });
    setInfoProps({ ...props, segment_id: segId >= 0 ? segId : null });
    setInfoAnchor({ x: e.point.x, y: e.point.y });
    setLrpInfo(null);
    e.originalEvent.stopPropagation();
  }

  function onDecodedPathClick(e) {
    // Path is a single WKT feature — no per-segment props. Stop propagation so
    // the general map click handler doesn't dismiss the result panel.
    e.originalEvent.stopPropagation();
  }

  function onLrpClick(e) {
    if (!e.features?.length) return;
    setLrpInfo(e.features[0].properties);
    setInfoAnchor({ x: e.point.x, y: e.point.y });
    setInfoProps(null);
    setHighlightedSegment(null);
    e.originalEvent.stopPropagation();
  }

  function onMapClick(e) {
    const layerIds = [...Array.from({ length: 8 }, (_, i) => `olr-frc${i}`), 'lrp-markers-circle', 'decoded-path-line'];
    const hits = mapRef.current.queryRenderedFeatures(e.point, { layers: layerIds });
    if (hits.length > 0) return;
    setHighlightedSegment(null);
    setInfoProps(null);
    setInfoAnchor(null);
    setLrpInfo(null);
  }

  // ── Highlight sync (store → map) ────────────────────────────────────────────
  // Depends only on highlightedSegment; reads decodeResult via ref so it never
  // races with the decode-result effect.

  useEffect(() => {
    const map = mapRef.current;

    if (pulseRef.current) { cancelAnimationFrame(pulseRef.current); pulseRef.current = null; }

    if (!map) return;

    const clearHighlight = () => {
      const src = map.getSource('highlighted-segment');
      if (src) src.setData({ type: 'FeatureCollection', features: [] });
      if (!traceHighlightSegIds?.length) {
        if (map.getLayer('olr-highlight')) map.setFilter('olr-highlight', ['boolean', false]);
      }
    };

    if (!highlightedSegment) { clearHighlight(); return; }

    // Look up geometry from the always-current ref (no dep needed)
    const seg = decodeResultRef.current?.segments?.find(
      s => s.tile === highlightedSegment.tile && s.local_index === highlightedSegment.local_index
    );

    if (seg?.geometry?.length >= 2) {
      const src = map.getSource('highlighted-segment');
      if (src) src.setData({
        type: 'FeatureCollection',
        features: [{ type: 'Feature', geometry: { type: 'LineString', coordinates: seg.geometry }, properties: {} }],
      });
      // Only clear tile-layer filter when trace panel isn't using it
      if (!traceHighlightSegIds?.length) {
        if (map.getLayer('olr-highlight')) map.setFilter('olr-highlight', ['boolean', false]);
      }

      // Fit map to the clicked segment's extent
      const lngs = seg.geometry.map(c => c[0]);
      const lats = seg.geometry.map(c => c[1]);
      map.fitBounds(
        [[Math.min(...lngs), Math.min(...lats)], [Math.max(...lngs), Math.max(...lats)]],
        { padding: 160, duration: 400, maxZoom: 18 },
      );

      // Sinusoidal halo pulse
      const t0 = performance.now();
      const pulse = (now) => {
        if (!map.getLayer('highlighted-segment-halo')) return;
        const phase = ((now - t0) / 700) * Math.PI * 2;
        map.setPaintProperty('highlighted-segment-halo', 'line-opacity',
          0.25 + 0.5 * (0.5 + 0.5 * Math.sin(phase)));
        pulseRef.current = requestAnimationFrame(pulse);
      };
      pulseRef.current = requestAnimationFrame(pulse);
    } else {
      // Background segment click — olr-highlight filter (skip if trace panel owns the filter)
      const src = map.getSource('highlighted-segment');
      if (src) src.setData({ type: 'FeatureCollection', features: [] });
      if (!traceHighlightSegIds?.length && map.getLayer('olr-highlight')) {
        map.setFilter('olr-highlight', ['all',
          ['==', ['get', 'tile'],        highlightedSegment.tile],
          ['==', ['get', 'local_index'], highlightedSegment.local_index],
        ]);
      }
    }
  }, [highlightedSegment, traceHighlightSegIds]); // ← decodeResult excluded; read via ref

  // ── Trace highlight sync (trace panel → dedicated trace-segment layer) ───────
  // Uses the decode-time geometry cache so highlights work regardless of
  // whether those tiles are currently loaded in the display cache.

  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;

    const traceSource = map.getSource('trace-segment');
    if (!traceSource) return;

    if (!traceHighlightSegIds?.length) {
      traceSource.setData({ type: 'FeatureCollection', features: [] });
      return;
    }

    const segGeomCache  = getSegGeomCache();
    const segIdToTile   = getSegIdToTile();
    const tileGeomCache = getTileGeomCache();
    const features      = [];
    const allCoords     = [];

    for (const segId of traceHighlightSegIds) {
      // Primary: direct segId → feature lookup (built at decode time)
      let feat = segGeomCache.get(segId);

      // Fallback: two-step lookup via segIdToTile + tileGeomCache
      if (!feat) {
        const mapping = segIdToTile.get(segId);
        if (mapping) {
          feat = tileGeomCache.get(mapping.tile_key)?.find(f => f.properties.local_index === mapping.local_index);
          if (feat) console.log('[trace-hl] two-step fallback hit segId', segId, 'mapping:', mapping);
        }
        if (!feat) {
          console.warn('[trace-hl] segId', segId, 'not found.',
            'segGeomCache.size:', segGeomCache.size,
            'mapping:', mapping,
            'tileKeys in tileGeomCache:', [...tileGeomCache.keys()].slice(0, 5));
          continue;
        }
      }
      features.push(feat);
      if (feat.geometry?.coordinates) allCoords.push(...feat.geometry.coordinates);
    }

    traceSource.setData({ type: 'FeatureCollection', features });

    // Pan to the bounding box of the highlighted segments
    if (allCoords.length >= 2) {
      const lngs = allCoords.map(c => c[0]);
      const lats = allCoords.map(c => c[1]);
      map.fitBounds(
        [[Math.min(...lngs), Math.min(...lats)], [Math.max(...lngs), Math.max(...lats)]],
        { padding: 160, duration: 400, maxZoom: 18 },
      );
    }

    // Show segment info popup for single-segment trace clicks
    if (features.length === 1) {
      setInfoProps({ ...features[0].properties });
      const coords = features[0].geometry?.coordinates;
      if (coords?.length) {
        const mid = coords[Math.floor(coords.length / 2)];
        const pixel = map.project(mid);
        setInfoAnchor({ x: pixel.x, y: pixel.y });
      }
    }
  }, [traceHighlightSegIds]);

  // ── Trace panel LRP focus (pan + popup) ─────────────────────────────────────

  useEffect(() => {
    if (!traceLrpFocus) return;
    const map = mapRef.current;
    if (!map) return;

    const { lon, lat, index, frc, fow, lfrcnp, bearing_lb, bearing_ub } = traceLrpFocus;
    map.flyTo({ center: [lon, lat], zoom: Math.max(map.getZoom(), 15), duration: 500 });
    // Enrich with snap info from decodeResult.lrps if available
    const lrpData = decodeResult?.lrps?.[index] ?? {};
    setLrpInfo({
      index, lat, lon, frc, fow, lfrcnp: lfrcnp ?? null, bearing_lb, bearing_ub,
      snap_lon: lrpData.snap_lon ?? null,
      snap_lat: lrpData.snap_lat ?? null,
      snap_is_endpoint: lrpData.snap_is_endpoint ?? null,
      snap_distance_m: lrpData.snap_distance_m ?? null,
    });
    setInfoProps(null);
    // Position popup near map center (LRP will fly there)
    setInfoAnchor({ x: map.getCanvas().clientWidth / 2, y: map.getCanvas().clientHeight / 2 });
    // Allow re-clicking same LRP by clearing after acting
    setTraceLrpFocus(null);
  }, [traceLrpFocus, setTraceLrpFocus]);

  // ── LRP bearing cone sync ─────────────────────────────────────────────────────

  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;
    const src = map.getSource('lrp-bearing');
    if (!src) return;
    if (!lrpInfo) { src.setData({ type: 'FeatureCollection', features: [] }); return; }
    const { lon, lat, snap_lon, snap_lat, bearing_lb, bearing_ub } = lrpInfo;
    const coneLon = snap_lon ?? lon;
    const coneLat = snap_lat ?? lat;
    src.setData(bearingConeGeoJSON(coneLon, coneLat, bearing_lb, bearing_ub, searchRadiusM ?? 100));
  }, [lrpInfo, searchRadiusM]);

  // ── Replay visual effect ─────────────────────────────────────────────────────

  const replayLayerIds = [
    'replay-radius-fill', 'replay-radius-line',
    'replay-candidates-circle',
    'replay-cloud-circle',
    'replay-frontier-circle',
    'replay-leg-from', 'replay-leg-to',
    'replay-flash-ring',
  ];

  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;

    if (frontierPulseRef.current) {
      cancelAnimationFrame(frontierPulseRef.current);
      frontierPulseRef.current = null;
    }
    if (flashAnimRef.current) {
      cancelAnimationFrame(flashAnimRef.current);
      flashAnimRef.current = null;
    }
    if (routePulseRef.current) {
      cancelAnimationFrame(routePulseRef.current);
      routePulseRef.current = null;
    }

    const emptyFC = { type: 'FeatureCollection', features: [] };
    const replaySources = ['replay-radius', 'replay-route', 'replay-candidates', 'replay-cloud', 'replay-frontier', 'replay-leg', 'replay-flash'];
    const vis = showReplay && replaySteps.length > 0 ? 'visible' : 'none';
    replayLayerIds.forEach(id => { if (map.getLayer(id)) map.setLayoutProperty(id, 'visibility', vis); });

    if (!showReplay || !replaySteps.length) {
      replaySources.forEach(s => { map.getSource(s)?.setData(emptyFC); });
      replayVisualRef.current = null;
      replayStepRef.current   = -1;
      replayStepsKey.current  = null;
      return;
    }

    const maxG = replayStats?.maxG ?? 0;

    // Reset incremental state when a new decode's steps arrive
    if (replayStepsKey.current !== replaySteps) {
      replayVisualRef.current = null;
      replayStepRef.current   = -1;
      replayStepsKey.current  = replaySteps;
    }

    // ── Incremental update ──────────────────────────────────────────────────
    // Forward step: apply only the new step(s) onto the existing state (O(1)).
    // Backward / jump: recompute from scratch (O(N)).
    let visualState;
    if (replayVisualRef.current && replayStep >= replayStepRef.current) {
      // Clone once, then apply each new step in place
      visualState = replayVisualRef.current;
      for (let i = replayStepRef.current + 1; i <= replayStep; i++) {
        applyStep(visualState, replaySteps[i], maxG);
        visualState.stepIdx = i;
      }
    } else {
      visualState = computeVisualState(replaySteps, replayStep, replayStats);
    }
    replayVisualRef.current = visualState;
    replayStepRef.current   = replayStep;

    // ── Push GeoJSON to sources ─────────────────────────────────────────────
    const gj = stateToGeoJSON(visualState);
    map.getSource('replay-radius')     ?.setData(gj.radiusFC);
    map.getSource('replay-candidates') ?.setData(gj.candFC);
    map.getSource('replay-cloud')      ?.setData(gj.cloudFC);
    map.getSource('replay-frontier')   ?.setData(gj.frontierFC);
    map.getSource('replay-leg')        ?.setData(gj.legFC);

    // Route segments — same two-step lookup as the trace-highlight effect
    const segCache   = getSegGeomCache();
    const segToTile  = getSegIdToTile();
    const tileCache  = getTileGeomCache();
    const routeFeats = (visualState.routeSegIds ?? []).map(id => {
      let f = segCache.get(id);
      if (!f) {
        const m = segToTile.get(id);
        if (m) f = tileCache.get(m.tile_key)?.find(x => x.properties.local_index === m.local_index);
      }
      return f;
    }).filter(Boolean);
    map.getSource('replay-route')?.setData({ type: 'FeatureCollection', features: routeFeats });

    // ── Frontier pulse animation ────────────────────────────────────────────
    if (gj.frontierFC.features.length > 0 && map.getLayer('replay-frontier-circle')) {
      const t0 = performance.now();
      const pulse = (now) => {
        if (!map.getLayer('replay-frontier-circle')) return;
        const phase = ((now - t0) / 600) * Math.PI * 2;
        try {
          map.setPaintProperty('replay-frontier-circle', 'circle-opacity', 0.6 + 0.4 * Math.sin(phase));
          map.setPaintProperty('replay-frontier-circle', 'circle-radius',  5   + 2   * Math.sin(phase));
        } catch (_) { return; }
        frontierPulseRef.current = requestAnimationFrame(pulse);
      };
      frontierPulseRef.current = requestAnimationFrame(pulse);
    }

    // ── Auto-pan ────────────────────────────────────────────────────────────
    const currentStep = replaySteps[replayStep];

    if (currentStep?.type === 'search_started') {
      map.flyTo({
        center:   [currentStep.coord[0], currentStep.coord[1]],
        zoom:     Math.max(map.getZoom(), 15),
        duration: 400,
      });
    }

    if (currentStep?.type === 'route_search_started') {
      const from = currentStep.from.projection.point;
      const to   = currentStep.to.projection.point;
      map.fitBounds(
        [[Math.min(from[0], to[0]), Math.min(from[1], to[1])],
         [Math.max(from[0], to[0]), Math.max(from[1], to[1])]],
        { padding: 120, maxZoom: 17, duration: 400 },
      );
    }

    // When a leg route is found: pan to full route extent, then pulse the line for 3 s.
    if (currentStep?.type === 'route_found') {
      if (routeFeats.length > 0) {
        let minLon = Infinity, minLat = Infinity, maxLon = -Infinity, maxLat = -Infinity;
        for (const feat of routeFeats) {
          const coords = feat.geometry.type === 'LineString'      ? feat.geometry.coordinates
                       : feat.geometry.type === 'MultiLineString' ? feat.geometry.coordinates.flat()
                       : [];
          for (const [lon, lat] of coords) {
            if (lon < minLon) minLon = lon; if (lon > maxLon) maxLon = lon;
            if (lat < minLat) minLat = lat; if (lat > maxLat) maxLat = lat;
          }
        }
        if (isFinite(minLon)) {
          map.fitBounds([[minLon, minLat], [maxLon, maxLat]], { padding: 80, maxZoom: 17, duration: 600 });
        }
      }
      if (routePulseRef.current) cancelAnimationFrame(routePulseRef.current);
      const rt0 = performance.now();
      const ROUTE_PULSE_MS = 3000;
      const animRoute = (now) => {
        if (!map.getLayer('replay-route-line')) return;
        const elapsed = now - rt0;
        const done    = elapsed >= ROUTE_PULSE_MS;
        const phase   = (elapsed / 500) * Math.PI;
        try {
          map.setPaintProperty('replay-route-line', 'line-width',   done ? 4 : 3 + 3 * Math.abs(Math.sin(phase)));
          map.setPaintProperty('replay-route-line', 'line-opacity', done ? 0.85 : 0.6 + 0.4 * Math.abs(Math.sin(phase)));
        } catch (_) { return; }
        if (!done) routePulseRef.current = requestAnimationFrame(animRoute);
        else routePulseRef.current = null;
      };
      routePulseRef.current = requestAnimationFrame(animRoute);
    }

    // Follow each A* node: instant jump so playback stays in sync.
    // Zoom 17 ≈ 700 m viewport width on a 1200 px screen — a typical road
    // segment (100–300 m) fills roughly half the map.
    if (currentStep?.type === 'astar_batch') {
      const last = currentStep.nodes[currentStep.nodes.length - 1];
      map.jumpTo({ center: [last.lon, last.lat], zoom: 17 });

      // Sonar-ping: expanding cyan ring that fades out over 2 s.
      // During rapid auto-play the ring stays bright (reset every 30 ms);
      // it fades only when stepping pauses.
      const flashSrc = map.getSource('replay-flash');
      if (flashSrc && map.getLayer('replay-flash-ring')) {
        flashSrc.setData({
          type: 'FeatureCollection',
          features: [{ type: 'Feature', geometry: { type: 'Point', coordinates: [last.lon, last.lat] }, properties: {} }],
        });
        if (flashAnimRef.current) cancelAnimationFrame(flashAnimRef.current);
        const t0 = performance.now();
        const FLASH_MS = 2000;
        const animFlash = (now) => {
          if (!map.getLayer('replay-flash-ring')) return;
          const p = Math.min(1, (now - t0) / FLASH_MS);
          try {
            map.setPaintProperty('replay-flash-ring', 'circle-stroke-opacity', 1 - p);
            map.setPaintProperty('replay-flash-ring', 'circle-radius', 20 + 18 * p);
          } catch (_) { return; }
          if (p < 1) {
            flashAnimRef.current = requestAnimationFrame(animFlash);
          } else {
            flashSrc.setData({ type: 'FeatureCollection', features: [] });
          }
        };
        flashAnimRef.current = requestAnimationFrame(animFlash);
      }
    }
  }, [showReplay, replayStep, replaySteps, replayStats]); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Segment layer visibility toggle ──────────────────────────────────────────

  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;
    const vis = showSegmentLayer ? 'visible' : 'none';
    for (let frc = 0; frc < 8; frc++) {
      if (map.getLayer(`olr-frc${frc}`)) map.setLayoutProperty(`olr-frc${frc}`, 'visibility', vis);
    }
    if (map.getLayer('olr-highlight')) map.setLayoutProperty('olr-highlight', 'visibility', vis);
  }, [showSegmentLayer]);

  // ── Decode result → map layers + camera ─────────────────────────────────────

  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;

    const pathSource        = map.getSource('decoded-path');
    const lrpSource         = map.getSource('lrp-markers');
    const snapSource        = map.getSource('lrp-snap');
    const displSource       = map.getSource('lrp-displacement');
    const uncertaintySource = map.getSource('offset-uncertainty');

    const emptyFC = { type: 'FeatureCollection', features: [] };
    if (!decodeResult) {
      pathSource?.setData(emptyFC);
      lrpSource?.setData(emptyFC);
      snapSource?.setData(emptyFC);
      displSource?.setData(emptyFC);
      uncertaintySource?.setData(emptyFC);
      setInfoProps(null);
      setInfoAnchor(null);
      setLrpInfo(null);
      return;
    }

    // ── LRP markers (success and failure) ────────────────────────────────────
    const lrps = decodeResult.lrps ?? [];
    lrpSource?.setData({
      type: 'FeatureCollection',
      features: lrps.map((lrp, idx) => ({
        type: 'Feature',
        geometry: { type: 'Point', coordinates: [lrp.lon, lrp.lat] },
        properties: {
          index: idx, total: lrps.length, lat: lrp.lat, lon: lrp.lon,
          frc: lrp.frc, fow: lrp.fow,
          lfrcnp: lrp.lfrcnp ?? null,
          bearing_lb: lrp.bearing_lb, bearing_ub: lrp.bearing_ub,
          snap_lon: lrp.snap_lon ?? null,
          snap_lat: lrp.snap_lat ?? null,
          snap_is_endpoint: lrp.snap_is_endpoint ?? null,
          snap_distance_m: lrp.snap_distance_m ?? null,
        },
      })),
    });

    // ── Snap markers and displacement lines ───────────────────────────────────
    const snapFeatures = lrps
      .filter(lrp => lrp.snap_lon != null)
      .map((lrp, idx) => ({
        type: 'Feature',
        geometry: { type: 'Point', coordinates: [lrp.snap_lon, lrp.snap_lat] },
        properties: {
          index: idx,
          is_endpoint: lrp.snap_is_endpoint ?? false,
          bearing: compassBearing(lrp.lon, lrp.lat, lrp.snap_lon, lrp.snap_lat),
        },
      }));
    snapSource?.setData({ type: 'FeatureCollection', features: snapFeatures });

    const displFeatures = lrps
      .filter(lrp => lrp.snap_lon != null)
      .map((lrp, idx) => ({
        type: 'Feature',
        geometry: { type: 'LineString', coordinates: [[lrp.lon, lrp.lat], [lrp.snap_lon, lrp.snap_lat]] },
        properties: { index: idx },
      }));
    displSource?.setData({ type: 'FeatureCollection', features: displFeatures });

    // ── Decoded path — use WKT for correctly offset-trimmed display ───────────
    // Per-segment geometries span full segments and ignore arc-offset trim;
    // the WKT from path_to_wkt already applies first_lrp_arc + pos_offset at
    // the head and last_lrp_arc - neg_offset at the tail.
    const wktCoords = parseWktLinestring(decodeResult.wkt);
    const pathFeatures = (decodeResult.ok && wktCoords?.length >= 2)
      ? [{ type: 'Feature', geometry: { type: 'LineString', coordinates: wktCoords }, properties: {} }]
      : [];
    pathSource?.setData({ type: 'FeatureCollection', features: pathFeatures });

    // ── Offset uncertainty bands ──────────────────────────────────────────────
    // Shown only when the offset is a v3 bucket interval (lb < ub).
    const uncertaintyFeatures = [];
    for (const [wkt, label] of [
      [decodeResult.pos_uncertainty_wkt, 'pos'],
      [decodeResult.neg_uncertainty_wkt, 'neg'],
    ]) {
      if (wkt) {
        const coords = parseWktLinestring(wkt);
        if (coords?.length >= 2) {
          uncertaintyFeatures.push({
            type: 'Feature',
            geometry: { type: 'LineString', coordinates: coords },
            properties: { label },
          });
        }
      }
    }
    uncertaintySource?.setData({ type: 'FeatureCollection', features: uncertaintyFeatures });

    // ── Fit camera — always include all LRP positions AND the decoded path ──────
    const lrpCoords = lrps.map(l => [l.lon, l.lat]);
    const fitCoords = [...lrpCoords, ...(wktCoords ?? [])];

    if (fitCoords.length > 0) {
      const lngs = fitCoords.map(c => c[0]);
      const lats = fitCoords.map(c => c[1]);
      const bounds = [[Math.min(...lngs), Math.min(...lats)], [Math.max(...lngs), Math.max(...lats)]];
      const doFit = () => map.fitBounds(bounds, { padding: 80, duration: 600, maxZoom: 17 });
      // Defer one frame so MapLibre has processed the setData calls first
      requestAnimationFrame(doFit);
    }
  }, [decodeResult]);

  // ── Render ───────────────────────────────────────────────────────────────────

  return (
    <div className="map-wrap">
      <div ref={mapContainer} className="map-container" />

      {/* Status overlay */}
      {status && <div className="map-status">{status}</div>}

      {/* Segment info panel */}
      {infoProps && (
        <div ref={segPanelRef} className="seg-info-panel"
          style={segPos ? { position: 'absolute', left: segPos.left, top: segPos.top, right: 'auto', bottom: 'auto' } : popupStyle(infoAnchor)}>
          <header className="seg-info-header" onMouseDown={segMouseDown}>
            <span>
              Segment{' '}
              {infoProps.osm_way_id != null
                ? <a href={`https://www.openstreetmap.org/way/${infoProps.osm_way_id}`} target="_blank" rel="noreferrer">{infoProps.osm_way_id}</a>
                : null}
            </span>
            <button
              className="seg-info-close"
              onClick={() => {
                setHighlightedSegment(null);
                setInfoProps(null);
                setInfoAnchor(null);
              }}
            >
              ✕
            </button>
          </header>
          <div className="seg-info-body">
            <table>
              <tbody>
                {[
                  ['FRC',       `${infoProps.frc} — ${infoProps.frc_name}`],
                  ['FOW',       `${infoProps.fow} — ${infoProps.fow_name}`],
                  ['Direction', infoProps.direction],
                  ['Length',    `${infoProps.length_m} m`],
                  ['Tile',      infoProps.tile],
                  ['Index',     infoProps.local_index],
                  ['Seg ID',    infoProps.segment_id != null ? infoProps.segment_id : '— (decode first)'],
                ].map(([k, v]) => (
                  <tr key={k}>
                    <td className="seg-info-key">{k}</td>
                    <td><b>{v}</b></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {/* LRP info panel */}
      {lrpInfo && (
        <div ref={lrpPanelRef} className="seg-info-panel"
          style={lrpPos ? { position: 'absolute', left: lrpPos.left, top: lrpPos.top, right: 'auto', bottom: 'auto' } : popupStyle(infoAnchor)}>
          <header className="seg-info-header" onMouseDown={lrpMouseDown}>
            <span>LRP {lrpInfo.index}</span>
            <button className="seg-info-close" onClick={() => { setLrpInfo(null); setInfoAnchor(null); }}>✕</button>
          </header>
          <div className="seg-info-body">
            <table>
              <tbody>
                {[
                  ['Lat',     lrpInfo.lat.toFixed(6)],
                  ['Lon',     lrpInfo.lon.toFixed(6)],
                  ['FRC',     lrpInfo.frc],
                  ['FOW',     lrpInfo.fow],
                  ['LFRCNP',  lrpInfo.lfrcnp !== null ? lrpInfo.lfrcnp : '— (last LRP)'],
                  ['Bearing', formatBearing(lrpInfo.bearing_lb, lrpInfo.bearing_ub)],
                ].map(([k, v]) => (
                  <tr key={k}><td className="seg-info-key">{k}</td><td><b>{v}</b></td></tr>
                ))}
                {lrpInfo.snap_lon != null && <>
                  <tr><td className="seg-info-divider" colSpan={2} /></tr>
                  <tr>
                    <td className="seg-info-key">Snap</td>
                    <td><b>{lrpInfo.snap_is_endpoint ? 'Endpoint' : 'Interior'}</b></td>
                  </tr>
                  <tr>
                    <td className="seg-info-key">Displacement</td>
                    <td><b>{Number(lrpInfo.snap_distance_m).toFixed(1)} m</b></td>
                  </tr>
                  <tr>
                    <td className="seg-info-key">Snap coord</td>
                    <td><b style={{fontSize:'11px'}}>{Number(lrpInfo.snap_lat).toFixed(6)}, {Number(lrpInfo.snap_lon).toFixed(6)}</b></td>
                  </tr>
                </>}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {/* Candidate info popup */}
      {candInfo && (
        <div ref={candPanelRef} className="seg-info-panel cand-panel"
          style={candPos ? { position: 'absolute', left: candPos.left, top: candPos.top, right: 'auto', bottom: 'auto' } : popupStyle(candAnchor, 320, 320)}>
          <header className="seg-info-header" onMouseDown={candMouseDown}>
            <span>
              LRP {candInfo.lrp_idx} candidate
              {candInfo.winner && <span className="cand-winner-badge"> ★ chosen</span>}
            </span>
            <button className="seg-info-close" onClick={() => { setCandInfo(null); setCandAnchor(null); candResetPos(); }}>✕</button>
          </header>
          <div className="seg-info-body">
            <CandidatePopupBody p={candInfo} />
          </div>
        </div>
      )}

      {/* FRC Legend — only shown when the Segs overlay is active */}
      {showSegmentLayer && (
        <div className="frc-legend">
          <h4>FRC</h4>
          {FRC_LABEL.map((label, i) => (
            <div key={i} className="legend-row">
              <div className="legend-swatch" style={{ background: FRC_COLOR[i] }} />
              <span>{label}</span>
            </div>
          ))}
        </div>
      )}

      {/* Basemap selector */}
      <div className="basemap-selector">
        <select value={basemap} onChange={e => handleBasemapChange(e.target.value)}>
          {BASEMAPS.map(b => (
            <option key={b.id} value={b.id}>{b.label}</option>
          ))}
        </select>
      </div>
    </div>
  );
}

// ── Candidate popup body ───────────────────────────────────────────────────────

function fmt(v, decimals = 2) {
  if (v == null) return '—';
  return typeof v === 'number' ? v.toFixed(decimals) : String(v);
}

/** Human-readable one-liner for a GateVerdict (serde externally-tagged). */
function formatVerdict(json) {
  if (!json) return null;
  let v;
  try { v = JSON.parse(json); } catch (_) { return null; }
  if (!v || v === 'Pass') return null;
  if (typeof v === 'string') return v;
  const key = Object.keys(v)[0];
  const val = v[key];
  switch (key) {
    case 'FailBearing':
      return `Bearing gate — exceeded by ${(val?.excess_deg ?? 0).toFixed(1)}°`;
    case 'FailRadius':
      return `Outside search radius`;
    case 'FailScore':
      return `Score too high${val?.score != null ? ` (${val.score.toFixed(4)})` : ''}`;
    case 'FailDirection':
      return `Wrong direction (one-way)`;
    default:
      return `${key}${typeof val === 'object' ? ': ' + JSON.stringify(val) : ''}`;
  }
}

const RESULT_LABEL = {
  accepted:  'Accepted',
  bearing:   'Bearing gate failed',
  radius:    'Outside search radius',
  score:     'Score gate failed',
  direction: 'Wrong direction',
  other:     'Rejected',
};

function CandidatePopupBody({ p }) {
  const accepted     = p.ctype === 'accepted';
  const resultLabel  = RESULT_LABEL[p.ctype] ?? p.ctype;
  const verdictLine  = !accepted ? formatVerdict(p.verdict_json) : null;

  return (
    <table className="cand-table">
      <tbody>
        <tr>
          <td className="seg-info-key">Result</td>
          <td><b className={accepted ? 'cand-accepted' : 'cand-rejected'}>{resultLabel}</b></td>
        </tr>
        {verdictLine &&
          <tr><td className="seg-info-key"></td><td className="cand-verdict-detail">{verdictLine}</td></tr>}
        {p.segment_id != null &&
          <tr><td className="seg-info-key">Seg ID</td><td><b>{p.segment_id}</b></td></tr>}
        {p.traversal &&
          <tr><td className="seg-info-key">Traversal</td><td><b>{p.traversal}</b></td></tr>}

        {/* Projection */}
        <tr><td colSpan={2} className="cand-section">Projection</td></tr>
        <tr><td className="seg-info-key">Dist from LRP</td><td><b>{fmt(p.distance_m)} m</b></td></tr>
        {p.arc_offset_m != null &&
          <tr><td className="seg-info-key">Arc offset</td><td><b>{fmt(p.arc_offset_m)} m</b></td></tr>}
        {p.bearing_deg != null &&
          <tr><td className="seg-info-key">Bearing</td><td><b>{fmt(p.bearing_deg, 1)}°</b></td></tr>}

        {/* Score breakdown — accepted only */}
        {accepted && <>
          <tr><td colSpan={2} className="cand-section">Score <span className="cand-lower">(lower = better)</span></td></tr>
          <tr><td className="seg-info-key">Total</td>     <td><b className="cand-score-total">{fmt(p.score_total, 4)}</b></td></tr>
          <tr><td className="seg-info-key">Distance</td>  <td><b>{fmt(p.score_distance, 4)}</b></td></tr>
          <tr><td className="seg-info-key">Bearing</td>   <td><b>{fmt(p.score_bearing, 4)}</b></td></tr>
          <tr><td className="seg-info-key">FRC</td>       <td><b>{fmt(p.score_frc, 4)}</b></td></tr>
          <tr><td className="seg-info-key">FOW</td>       <td><b>{fmt(p.score_fow, 4)}</b></td></tr>
          <tr><td className="seg-info-key">Wrong EP</td>  <td><b>{fmt(p.score_wrong_ep, 4)}</b></td></tr>
          <tr><td className="seg-info-key">Interior</td>  <td><b>{fmt(p.score_interior, 4)}</b></td></tr>
        </>}
      </tbody>
    </table>
  );
}
