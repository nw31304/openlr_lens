import React, { useEffect, useRef, useState } from 'react';
import maplibregl from 'maplibre-gl';
import 'maplibre-gl/dist/maplibre-gl.css';
import { PMTiles } from 'pmtiles';
import { useStore, getSegmentId, getSegGeomCache, getSegIdToTile, getTileGeomCache } from '../store.js';
import { useDraggable } from '../hooks.js';


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
  'offset-uncertainty', 'lrp-bearing', 'highlighted-segment', 'trace-segment',
]);
const CUSTOM_LAYER_IDS = new Set([
  'olr-frc0','olr-frc1','olr-frc2','olr-frc3','olr-frc4','olr-frc5','olr-frc6','olr-frc7',
  'olr-highlight', 'decoded-path-line', 'lrp-markers-circle',
  'offset-uncertainty-halo', 'offset-uncertainty-dash',
  'lrp-bearing-fill', 'lrp-bearing-outline',
  'highlighted-segment-halo', 'highlighted-segment-line',
  'trace-segment-halo', 'trace-segment-line',
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

// ── Map Component ──────────────────────────────────────────────────────────────

export default function MapView({ tilesBase, ready }) {
  const mapContainer = useRef(null);
  const mapRef = useRef(null);
  const tileCacheRef = useRef(new Map());
  const pendingCountRef = useRef(0);
  const pmtilesRef = useRef(null);
  const pulseRef   = useRef(null);
  const lrpPanelRef = useRef(null);
  const segPanelRef = useRef(null);

  const [status, setStatus] = useState(null);
  const [infoProps, setInfoProps] = useState(null);
  const [infoAnchor, setInfoAnchor] = useState(null);
  const [lrpInfo, setLrpInfo] = useState(null);
  const [basemap, setBasemap] = useState('liberty');

  const { pos: lrpPos, onMouseDown: lrpMouseDown, resetPos: lrpResetPos } = useDraggable(lrpPanelRef);
  const { pos: segPos, onMouseDown: segMouseDown, resetPos: segResetPos } = useDraggable(segPanelRef);

  const decodeResult          = useStore(s => s.decodeResult);
  const highlightedSegment    = useStore(s => s.highlightedSegment);
  const setHighlightedSegment = useStore(s => s.setHighlightedSegment);
  const traceHighlightSegIds  = useStore(s => s.traceHighlightSegIds);
  const traceLrpFocus         = useStore(s => s.traceLrpFocus);
  const setTraceLrpFocus      = useStore(s => s.setTraceLrpFocus);
  const showSegmentLayer      = useStore(s => s.showSegmentLayer);
  const searchRadiusM         = useStore(s => s.params.candidate_search_radius_m);

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
          'circle-color':        '#aa00ff',
          'circle-stroke-width': 2,
          'circle-stroke-color': '#ffffff',
        },
      });

      // ── Offset uncertainty bands (v3 [lb, ub] zone at path head/tail) ────
      map.addSource('offset-uncertainty', { type: 'geojson', data: { type: 'FeatureCollection', features: [] } });
      map.addLayer({
        id: 'offset-uncertainty-halo', type: 'line', source: 'offset-uncertainty',
        paint: { 'line-color': '#ffcc00', 'line-width': 12, 'line-opacity': 0.35 },
      });
      map.addLayer({
        id: 'offset-uncertainty-dash', type: 'line', source: 'offset-uncertainty',
        paint: { 'line-color': '#ffcc00', 'line-width': 4, 'line-opacity': 0.95, 'line-dasharray': [4, 3] },
      });

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

      // ── Click handlers ────────────────────────────────────────────────────
      for (let frc = 0; frc < 8; frc++) {
        map.on('click', `olr-frc${frc}`, onSegmentClick);
        map.on('mouseenter', `olr-frc${frc}`, () => map.getCanvas().style.cursor = 'pointer');
        map.on('mouseleave', `olr-frc${frc}`, () => map.getCanvas().style.cursor = '');
      }

      map.on('click', 'lrp-markers-circle', onLrpClick);
      map.on('mouseenter', 'lrp-markers-circle', () => map.getCanvas().style.cursor = 'pointer');
      map.on('mouseleave', 'lrp-markers-circle', () => map.getCanvas().style.cursor = '');

      map.on('click', 'decoded-path-line', onDecodedPathClick);
      map.on('mouseenter', 'decoded-path-line', () => map.getCanvas().style.cursor = 'pointer');
      map.on('mouseleave', 'decoded-path-line', () => map.getCanvas().style.cursor = '');

      map.on('click', onMapClick);

      loadVisibleTiles(map);
    });

    map.on('moveend', () => loadVisibleTiles(map));
    map.on('zoomend', () => loadVisibleTiles(map));

    return () => {
      if (pulseRef.current) { cancelAnimationFrame(pulseRef.current); pulseRef.current = null; }
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
    setLrpInfo({ index, lat, lon, frc, fow, lfrcnp: lfrcnp ?? null, bearing_lb, bearing_ub });
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
    const { lon, lat, bearing_lb, bearing_ub } = lrpInfo;
    src.setData(bearingConeGeoJSON(lon, lat, bearing_lb, bearing_ub, searchRadiusM ?? 100));
  }, [lrpInfo, searchRadiusM]);

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
    const uncertaintySource = map.getSource('offset-uncertainty');

    if (!decodeResult) {
      pathSource?.setData({ type: 'FeatureCollection', features: [] });
      lrpSource?.setData({ type: 'FeatureCollection', features: [] });
      uncertaintySource?.setData({ type: 'FeatureCollection', features: [] });
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
          index: idx, lat: lrp.lat, lon: lrp.lon,
          frc: lrp.frc, fow: lrp.fow,
          lfrcnp: lrp.lfrcnp ?? null,
          bearing_lb: lrp.bearing_lb, bearing_ub: lrp.bearing_ub,
        },
      })),
    });

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

    // ── Fit camera — prefer path coords, fall back to LRP coords ─────────────
    const fitCoords = wktCoords?.length
      ? wktCoords
      : lrps.map(l => [l.lon, l.lat]);

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
