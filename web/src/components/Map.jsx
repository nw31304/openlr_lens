import React, { useEffect, useRef, useState } from 'react';
import maplibregl from 'maplibre-gl';
import 'maplibre-gl/dist/maplibre-gl.css';
import { PMTiles } from 'pmtiles';
import { useStore, getSegmentId, getSegGeomCache, getSegIdToTile, getTileGeomCache } from '../store.js';
import { useDraggable } from '../hooks.js';
import { emptyState, applyStep, computeVisualState, stateToGeoJSON } from '../replayEngine.js';


// Inline SVG tip for speech bubbles — above: tip points down, below: tip points up.
// W/H are in px; tipLeft is the center of the tip within the popup (pixels from popup left).
function TipSvg({ placement, tipLeft }) {
  if (!placement || tipLeft == null) return null;
  const W = 24, H = 12;
  const left = tipLeft - W / 2;
  const fill   = 'rgba(20,20,36,0.97)';
  const stroke = 'rgba(255,255,255,0.18)';
  const base = { position: 'absolute', left, width: W, height: H, display: 'block', pointerEvents: 'none' };
  if (placement === 'above') {
    return (
      <svg style={{ ...base, bottom: -H }} viewBox={`0 0 ${W} ${H}`}>
        <polygon  points={`0,0 ${W/2},${H} ${W},0`}         fill={fill} />
        <polyline points={`0,0 ${W/2},${H} ${W},0`}         fill="none" stroke={stroke} strokeWidth="1" />
      </svg>
    );
  }
  return (
    <svg style={{ ...base, top: -H }} viewBox={`0 0 ${W} ${H}`}>
      <polygon  points={`0,${H} ${W/2},0 ${W},${H}`}        fill={fill} />
      <polyline points={`0,${H} ${W/2},0 ${W},${H}`}        fill="none" stroke={stroke} strokeWidth="1" />
    </svg>
  );
}

// Returns { style, placement, tipLeft } for a speech-bubble popup.
// For callout-above: pins popup.bottom = anchor.y − tipH via the CSS `bottom`
// property (relative to the map container height).  No height estimate needed —
// the popup simply grows upward from that pinned edge, so the SVG tip child at
// `bottom: -tipH` always lands exactly on anchor.y regardless of content height.
// For callout-below: sets top = anchor.y + tipH; popup grows downward.
function popupPlacement(anchor, w = 260, containerW = null, containerH = null) {
  if (!anchor) return { style: undefined, placement: null, tipLeft: w / 2 };
  const edge = 8, tipH = 12;
  const cw = containerW || window.innerWidth;
  const ch = containerH || window.innerHeight;

  // Centre popup on anchor, then clamp to stay inside the container.
  const rawLeft = anchor.x - w / 2;
  const left    = Math.max(edge, Math.min(rawLeft, cw - w - edge));

  // Place above when the anchor is in the lower half — more room to grow upward.
  const above = anchor.y > ch / 2;

  // tipLeft: horizontal center of the tip within the popup (clamped 12–w–12).
  const tipLeft = Math.max(12, Math.min(anchor.x - left, w - 12));

  if (above) {
    // `bottom: ch - anchor.y + tipH` → popup.bottom = anchor.y − tipH.
    // SVG tip child at `bottom: -tipH` → tip visual bottom = anchor.y. ✓
    return {
      style: { position: 'absolute', left, bottom: ch - anchor.y + tipH, top: 'auto', right: 'auto' },
      placement: 'above',
      tipLeft,
    };
  }
  return {
    style: { position: 'absolute', left, top: anchor.y + tipH, bottom: 'auto', right: 'auto' },
    placement: 'below',
    tipLeft,
  };
}
import { decodeTile } from '../tileDecoder.js';
import { diagnoseSegment } from '../diagnosis.js';

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
  'olr-segments', 'olr-nodes', 'decoded-path', 'lrp-markers',
  'lrp-snap', 'lrp-displacement',
  'offset-uncertainty', 'lrp-bearing', 'highlighted-segment', 'trace-segment',
  'replay-radius', 'replay-route', 'replay-candidates', 'replay-cloud', 'replay-frontier', 'replay-leg', 'replay-flash',
  'measure-line', 'measure-points',
]);
const CUSTOM_LAYER_IDS = new Set([
  'olr-frc0','olr-frc1','olr-frc2','olr-frc3','olr-frc4','olr-frc5','olr-frc6','olr-frc7',
  'olr-highlight', 'olr-nodes-circle', 'decoded-path-line', 'lrp-markers-circle',
  'lrp-displacement-line', 'lrp-displacement-arrow',
  'offset-uncertainty-line',
  'lrp-bearing-fill', 'lrp-bearing-outline',
  'highlighted-segment-halo', 'highlighted-segment-line',
  'trace-segment-halo', 'trace-segment-line',
  'replay-radius-fill', 'replay-radius-line',
  'replay-route-casing', 'replay-route-line',
  'replay-candidates-circle',
  'replay-cloud-circle',
  'replay-frontier-circle',
  'replay-leg-from', 'replay-leg-to',
  'replay-flash-ring',
  'measure-line-layer', 'measure-points-layer',
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

// 32-sector compass rose matching v3 bearing quantization (11.25° per sector).
// Active sectors (those inside [lb, ub]) are highlighted in magenta.
function BearingCompass({ lb, ub, size = 76 }) {
  const N = 32;
  const SECTOR = 360 / N;
  const cx = size / 2, cy = size / 2;
  const outerR = size / 2 - 4;
  const innerR = outerR * 0.42;

  function sectorActive(i) {
    const mid = ((i + 0.5) * SECTOR + 360) % 360;
    const lo = ((lb % 360) + 360) % 360;
    const hi = ((ub % 360) + 360) % 360;
    if (lo <= hi) return mid >= lo && mid <= hi;
    return mid >= lo || mid <= hi; // wraparound, e.g. 350°–10°
  }

  function toXY(bearingDeg, r) {
    const rad = ((bearingDeg - 90) * Math.PI) / 180;
    return [cx + r * Math.cos(rad), cy + r * Math.sin(rad)];
  }

  function sectorPath(i) {
    const a0 = i * SECTOR, a1 = a0 + SECTOR;
    const [ox0, oy0] = toXY(a0, outerR);
    const [ox1, oy1] = toXY(a1, outerR);
    const [ix0, iy0] = toXY(a0, innerR);
    const [ix1, iy1] = toXY(a1, innerR);
    return `M ${ox0} ${oy0} A ${outerR} ${outerR} 0 0 1 ${ox1} ${oy1} L ${ix1} ${iy1} A ${innerR} ${innerR} 0 0 0 ${ix0} ${iy0} Z`;
  }

  return (
    <svg width={size} height={size} style={{ display: 'block', margin: '4px auto 0' }}>
      {Array.from({ length: N }, (_, i) => (
        <path key={i} d={sectorPath(i)}
          fill={sectorActive(i) ? '#e040fb' : '#252535'}
          stroke="#111" strokeWidth="0.8" />
      ))}
      <text x={cx} y={10} textAnchor="middle" fill="#888" fontSize="7" fontFamily="sans-serif" fontWeight="bold">N</text>
      <circle cx={cx} cy={cy} r={2.5} fill="#555" />
    </svg>
  );
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

function haversineM(lon1, lat1, lon2, lat2) {
  const R = 6371000;
  const φ1 = lat1 * Math.PI / 180, φ2 = lat2 * Math.PI / 180;
  const Δφ = (lat2 - lat1) * Math.PI / 180;
  const Δλ = (lon2 - lon1) * Math.PI / 180;
  const a = Math.sin(Δφ / 2) ** 2 + Math.cos(φ1) * Math.cos(φ2) * Math.sin(Δλ / 2) ** 2;
  return R * 2 * Math.atan2(Math.sqrt(a), Math.sqrt(1 - a));
}

function fmtDist(m) {
  if (m < 1000) return `${Math.round(m)} m`;
  return `${(m / 1000).toFixed(2)} km`;
}

// Interpolated midpoint of a polyline by vertex index — handles 2-vertex segments
// (where Math.floor(n/2) = 1 = endpoint) by interpolating between the two flanking vertices.
function polylineMid(coords) {
  if (!coords?.length) return null;
  if (coords.length === 1) return coords[0];
  const t = (coords.length - 1) / 2;
  const i = Math.floor(t), j = Math.ceil(t);
  if (i === j) return coords[i];
  const f = t - i;
  return [coords[i][0] + f * (coords[j][0] - coords[i][0]),
          coords[i][1] + f * (coords[j][1] - coords[i][1])];
}

function parseLatLon(str) {
  const m = str.trim().match(/^(-?\d+(?:\.\d+)?)[,\s]+(-?\d+(?:\.\d+)?)$/);
  if (!m) return null;
  const lat = parseFloat(m[1]), lon = parseFloat(m[2]);
  if (lat < -90 || lat > 90 || lon < -180 || lon > 180) return null;
  return { lat, lon };
}

function applyOffsets(coords, posM, negM) {
  let pts = [...coords];
  if (posM > 0) {
    let remaining = posM;
    while (pts.length > 2) {
      const d = haversineM(pts[0][0], pts[0][1], pts[1][0], pts[1][1]);
      if (remaining <= d) {
        const t = remaining / d;
        pts[0] = [pts[0][0] + t * (pts[1][0] - pts[0][0]), pts[0][1] + t * (pts[1][1] - pts[0][1])];
        break;
      }
      remaining -= d;
      pts.shift();
    }
  }
  if (negM > 0) {
    let remaining = negM;
    while (pts.length > 2) {
      const last = pts.length - 1;
      const d = haversineM(pts[last-1][0], pts[last-1][1], pts[last][0], pts[last][1]);
      if (remaining <= d) {
        const t = remaining / d;
        pts[last] = [pts[last][0] + t * (pts[last-1][0] - pts[last][0]), pts[last][1] + t * (pts[last-1][1] - pts[last][1])];
        break;
      }
      remaining -= d;
      pts.pop();
    }
  }
  return pts;
}

function computeTraversalDirections(segments) {
  const cache = getSegGeomCache();
  const n = segments.length;
  if (n === 0) return [];
  const dirs = segments.map(s =>
    s.direction === 'Forward' ? 'Forward' : s.direction === 'Backward' ? 'Reverse' : null
  );
  const feats = segments.map(s => cache.get(s.segment_id));
  if (dirs[0] === null) {
    const f0 = feats[0], f1 = n > 1 ? feats[1] : null;
    if (f0 && f1) {
      const c0 = f0.geometry.coordinates, c1 = f1.geometry.coordinates;
      const dFF = haversineM(c0[c0.length-1][0], c0[c0.length-1][1], c1[0][0], c1[0][1]);
      const dFR = haversineM(c0[c0.length-1][0], c0[c0.length-1][1], c1[c1.length-1][0], c1[c1.length-1][1]);
      const dRF = haversineM(c0[0][0], c0[0][1], c1[0][0], c1[0][1]);
      const dRR = haversineM(c0[0][0], c0[0][1], c1[c1.length-1][0], c1[c1.length-1][1]);
      dirs[0] = Math.min(dFF, dFR) <= Math.min(dRF, dRR) ? 'Forward' : 'Reverse';
    } else {
      dirs[0] = 'Forward';
    }
  }
  for (let i = 1; i < n; i++) {
    if (dirs[i] !== null) continue;
    const fi = feats[i], fi1 = feats[i - 1];
    if (!fi || !fi1) { dirs[i] = 'Forward'; continue; }
    const ci = fi.geometry.coordinates, ci1 = fi1.geometry.coordinates;
    const prevEnd = dirs[i-1] === 'Forward' ? ci1[ci1.length-1] : ci1[0];
    const dFwd = haversineM(prevEnd[0], prevEnd[1], ci[0][0], ci[0][1]);
    const dRev = haversineM(prevEnd[0], prevEnd[1], ci[ci.length-1][0], ci[ci.length-1][1]);
    dirs[i] = dFwd <= dRev ? 'Forward' : 'Reverse';
  }
  return dirs;
}

// Clip a polyline [lon,lat][] to start at the nearest point to (snapLon, snapLat).
// Returns the tail portion of the polyline from that snap point onward.
function clipGeomFromPoint(coords, snapLon, snapLat) {
  if (!coords || coords.length < 2) return coords;
  let bestIdx = 0, bestT = 0, bestDist = Infinity;
  for (let i = 0; i < coords.length - 1; i++) {
    const ax = coords[i][0], ay = coords[i][1];
    const bx = coords[i + 1][0], by = coords[i + 1][1];
    const dx = bx - ax, dy = by - ay;
    const len2 = dx * dx + dy * dy;
    const t = len2 === 0 ? 0 : Math.max(0, Math.min(1, ((snapLon - ax) * dx + (snapLat - ay) * dy) / len2));
    const ex = snapLon - (ax + t * dx), ey = snapLat - (ay + t * dy);
    const d = ex * ex + ey * ey;
    if (d < bestDist) { bestDist = d; bestIdx = i; bestT = t; }
  }
  const clipPt = [
    coords[bestIdx][0] + bestT * (coords[bestIdx + 1][0] - coords[bestIdx][0]),
    coords[bestIdx][1] + bestT * (coords[bestIdx + 1][1] - coords[bestIdx][1]),
  ];
  return [clipPt, ...coords.slice(bestIdx + 1)];
}

// Clip a polyline [lon,lat][] to end at the nearest point to (snapLon, snapLat).
// Returns the head portion of the polyline up to that snap point.
function clipGeomToPoint(coords, snapLon, snapLat) {
  if (!coords || coords.length < 2) return coords;
  let bestIdx = 0, bestT = 0, bestDist = Infinity;
  for (let i = 0; i < coords.length - 1; i++) {
    const ax = coords[i][0], ay = coords[i][1];
    const bx = coords[i + 1][0], by = coords[i + 1][1];
    const dx = bx - ax, dy = by - ay;
    const len2 = dx * dx + dy * dy;
    const t = len2 === 0 ? 0 : Math.max(0, Math.min(1, ((snapLon - ax) * dx + (snapLat - ay) * dy) / len2));
    const ex = snapLon - (ax + t * dx), ey = snapLat - (ay + t * dy);
    const d = ex * ex + ey * ey;
    if (d < bestDist) { bestDist = d; bestIdx = i; bestT = t; }
  }
  const clipPt = [
    coords[bestIdx][0] + bestT * (coords[bestIdx + 1][0] - coords[bestIdx][0]),
    coords[bestIdx][1] + bestT * (coords[bestIdx + 1][1] - coords[bestIdx][1]),
  ];
  return [...coords.slice(0, bestIdx + 1), clipPt];
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

  // Direction triangle — solid filled, points right (→), rotated by MapLibre to follow
  // the line direction. Registered as SDF so icon-color can tint it per layer.
  const tw = 14, th = 14;
  const triCanvas = document.createElement('canvas');
  triCanvas.width = tw; triCanvas.height = th;
  const tc = triCanvas.getContext('2d');
  tc.clearRect(0, 0, tw, th);
  // Solid white fill (SDF tinting overrides colour at render time)
  tc.beginPath();
  tc.moveTo(2, 1); tc.lineTo(tw - 1, th / 2); tc.lineTo(2, th - 1);
  tc.closePath();
  tc.fillStyle = 'white';
  tc.fill();
  map.addImage('direction-triangle', tc.getImageData(0, 0, tw, th), { sdf: true });
  // Keep legacy name so any surviving refs still resolve
  map.addImage('candidate-chevron',  tc.getImageData(0, 0, tw, th), { sdf: true });

  // Numbered LRP marker circles (1–20). Canvas-drawn so no glyph/font dependency.
  const ms = 24;
  for (let n = 1; n <= 20; n++) {
    const mc = document.createElement('canvas');
    mc.width = ms; mc.height = ms;
    const mc2d = mc.getContext('2d');
    mc2d.beginPath();
    mc2d.arc(ms / 2, ms / 2, ms / 2 - 2, 0, 2 * Math.PI);
    mc2d.fillStyle = '#e040fb';
    mc2d.fill();
    mc2d.strokeStyle = '#ffffff';
    mc2d.lineWidth = 2;
    mc2d.stroke();
    mc2d.fillStyle = '#ffffff';
    mc2d.font = `bold ${n > 9 ? 9 : 11}px Arial, sans-serif`;
    mc2d.textAlign = 'center';
    mc2d.textBaseline = 'middle';
    mc2d.fillText(String(n), ms / 2, ms / 2 + 0.5);
    map.addImage(`lrp-num-${n}`, mc2d.getImageData(0, 0, ms, ms));
  }

}

// ── Map Component ──────────────────────────────────────────────────────────────

export default function MapView({ tilesBase, ready }) {
  const mapContainer    = useRef(null);
  const mapRef          = useRef(null);
  const tileCacheRef    = useRef(new Map());
  const nodesCacheRef   = useRef(new Map());
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
  const candPanelRef        = useRef(null);
  const candidatePopupRef   = useRef(null);
  const capturePopupRef     = useRef(null);
  const pendingPopupCoordRef    = useRef(null); // geographic coord to project after fitBounds animation
  const pendingCandAnchorCoordRef = useRef(null); // candidate popup snap coord, same deferred scheme

  const [status, setStatus] = useState(null);
  const [infoProps, setInfoProps] = useState(null);
  const [infoAnchor, setInfoAnchor] = useState(null);
  const [lrpInfo, setLrpInfo] = useState(null);
  const [nodeInfo, setNodeInfo] = useState(null);
  const [nodeAnchor, setNodeAnchor] = useState(null);
  const [candAnchor, setCandAnchor] = useState(null);
  const [basemap, setBasemap] = useState('liberty');
  const [segDiagnosis, setSegDiagnosis] = useState(null);

  const [measuring, setMeasuring] = useState(false);
  const [measurePts, setMeasurePts] = useState([]);
  const [measureCursor, setMeasureCursor] = useState(null);
  const measuringRef  = useRef(false);
  const measurePtsRef = useRef([]);

  const [coordCaptureActive, setCoordCaptureActive] = useState(false);
  const coordCaptureActiveRef = useRef(false);
  const [cursorCoord, setCursorCoord] = useState(null);
  const cursorCoordRef = useRef(null);
  const [coordCopied, setCoordCopied] = useState(false);
  const [copiedText, setCopiedText] = useState('');
  const [locPins, setLocPins] = useState([]);
  const locPinMarkersRef = useRef({});
  const [showZoomPanel, setShowZoomPanel] = useState(false);
  const [zoomInput, setZoomInput] = useState('');
  const [zoomError, setZoomError] = useState(false);
  const [bearingActive, setBearingActive] = useState(false);
  const bearingActiveRef = useRef(false);
  const [bearingPts, setBearingPts] = useState([]);
  const bearingPtsRef = useRef([]);
  const [permalinkCopied, setPermalinkCopied] = useState(false);
  const [exportFlash, setExportFlash] = useState(false);
  const [toolbarOpen, setToolbarOpen] = useState(false);

  const { pos: lrpPos,  onMouseDown: lrpMouseDown,  resetPos: lrpResetPos  } = useDraggable(lrpPanelRef);
  const { pos: segPos,  onMouseDown: segMouseDown,  resetPos: segResetPos  } = useDraggable(segPanelRef);
  const { pos: candPos, onMouseDown: candMouseDown, resetPos: candResetPos } = useDraggable(candPanelRef);

  const decodeResult               = useStore(s => s.decodeResult);
  const highlightedSegment         = useStore(s => s.highlightedSegment);
  const setHighlightedSegment      = useStore(s => s.setHighlightedSegment);
  const requestedInfoSegment       = useStore(s => s.requestedInfoSegment);
  const clearRequestedInfoSegment  = useStore(s => s.clearRequestedInfoSegment);
  const traceHighlightSegIds  = useStore(s => s.traceHighlightSegIds);
  const traceHighlightSnaps   = useStore(s => s.traceHighlightSnaps);
  const traceLrpFocus         = useStore(s => s.traceLrpFocus);
  const setTraceLrpFocus      = useStore(s => s.setTraceLrpFocus);
  const showSegmentLayer      = useStore(s => s.showSegmentLayer);
  const searchRadiusM         = useStore(s => s.params.candidate_search_radius_m);
  const lfrcnpTolerance       = useStore(s => s.params.lfrcnp_tolerance ?? 0);
  const replayStep  = useStore(s => s.replayStep);
  const replaySteps = useStore(s => s.replaySteps);
  const replayStats = useStore(s => s.replayStats);
  const showReplay         = useStore(s => s.showReplay);
  const showTrace          = useStore(s => s.showTrace);
  const candidatePopup     = useStore(s => s.candidatePopup);
  const clearCandidatePopup = useStore(s => s.clearCandidatePopup);

  const openlrString    = useStore(s => s.openlrString);
  const setOpenlrString = useStore(s => s.setOpenlrString);
  const runDecode       = useStore(s => s.runDecode);

  const permalinkAutoLoaded = useRef(false);
  useEffect(() => {
    if (!ready || permalinkAutoLoaded.current) return;
    const hash = window.location.hash;
    if (hash.startsWith('#q=')) {
      const q = decodeURIComponent(hash.slice(3));
      if (q) {
        permalinkAutoLoaded.current = true;
        setOpenlrString(q);
        runDecode();
      }
    }
  }, [ready]); // eslint-disable-line react-hooks/exhaustive-deps

  // Reset drag position when a new popup target is clicked
  useEffect(() => { lrpResetPos(); }, [lrpInfo]);   // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => { segResetPos(); }, [infoProps]);  // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {                                  // eslint-disable-line react-hooks/exhaustive-deps
    candidatePopupRef.current = candidatePopup;
    candResetPos();
    const map = mapRef.current;

    // Update trace-segment-line color and trace-segment-arrow visibility based on accept/reject
    const isAccepted = candidatePopup?.winner || candidatePopup?.ctype === 'accepted';
    if (map?.getLayer('trace-segment-line')) {
      map.setPaintProperty('trace-segment-line', 'line-color',
        candidatePopup ? (isAccepted ? '#22cc66' : '#ee4444') : '#ff8c00');
      map.setPaintProperty('trace-segment-line', 'line-width', candidatePopup ? 5 : 4);
    }
    if (map?.getLayer('trace-segment-arrow')) {
      map.setLayoutProperty('trace-segment-arrow', 'visibility',
        candidatePopup ? 'visible' : 'none');
      if (candidatePopup) {
        const arrowColor = isAccepted ? '#22cc66' : '#ee4444';
        map.setPaintProperty('trace-segment-arrow', 'icon-color',       arrowColor);
        map.setPaintProperty('trace-segment-arrow', 'icon-halo-color',  'white');
        map.setPaintProperty('trace-segment-arrow', 'icon-halo-width',  4);
        map.setPaintProperty('trace-segment-arrow', 'icon-halo-blur',   0);
        map.setPaintProperty('trace-segment-arrow', 'icon-opacity',     1);
      }
    }

    if (!candidatePopup?.snap_lon) {
      setCandAnchor(null);
      pendingCandAnchorCoordRef.current = null;
      // Hide direction arrows when no candidate is selected
      if (map?.getLayer('replay-candidates-arrow')) {
        map.setLayoutProperty('replay-candidates-arrow', 'visibility', 'none');
        map.setFilter('replay-candidates-arrow', null);
      }
      return;
    }
    if (!map) return;

    // Defer anchor projection until after the fitBounds animation (triggered by
    // the traceHighlightSegIds effect in the same render cycle) completes.
    // setTimeout(0) fallback handles the case where the segment was already
    // highlighted (no fitBounds called), so traceHighlightSegIds effect won't fire.
    // Anchor to segment midpoint (not snap point, which lands at or near an endpoint)
    const segFeat = candidatePopup.segment_id != null ? getSegGeomCache().get(candidatePopup.segment_id) : null;
    const anchorCoord = polylineMid(segFeat?.geometry?.coordinates)
      ?? [candidatePopup.snap_lon, candidatePopup.snap_lat];
    pendingCandAnchorCoordRef.current = anchorCoord;
    setCandAnchor(null);
    const fallbackId = setTimeout(() => {
      if (pendingCandAnchorCoordRef.current) {
        pendingCandAnchorCoordRef.current = null;
        const pt = mapRef.current?.project(anchorCoord);
        if (pt) setCandAnchor({ x: pt.x, y: pt.y });
      }
    }, 0);

    // Show arrows filtered to the selected traversal direction only
    if (map.getLayer('replay-candidates-arrow') && candidatePopup.segment_id != null) {
      map.setFilter('replay-candidates-arrow', ['all',
        ['==', ['get', 'segment_id'], candidatePopup.segment_id],
        ['==', ['get', 'traversal'],  candidatePopup.traversal ?? ''],
      ]);
      map.setLayoutProperty('replay-candidates-arrow', 'visibility', 'visible');
    }
    return () => clearTimeout(fallbackId);
  }, [candidatePopup]);

  // Open the segment info popup when ResultPanel (or decoded-path click) requests it.
  useEffect(() => {
    if (!requestedInfoSegment) return;
    const { tile, local_index } = requestedInfoSegment;
    clearRequestedInfoSegment();
    const [z, x, y] = tile.split('/').map(Number);
    const segId = getSegmentId(z, x, y, local_index);
    const feat = getSegGeomCache().get(segId);
    if (!feat) return;

    // Set popup content immediately, but defer anchor projection.
    // The highlightedSegment effect (running in the same render cycle) will call
    // fitBounds and then register the moveend listener — storing the target coord
    // in pendingPopupCoordRef lets it project AFTER the animation settles.
    setLrpInfo(null);
    setInfoAnchor(null);
    setInfoProps({ ...feat.properties, segment_id: segId });
    setSegDiagnosis(null);

    const coords = feat.geometry.coordinates;
    pendingPopupCoordRef.current = polylineMid(coords);
  }, [requestedInfoSegment]); // eslint-disable-line react-hooks/exhaustive-deps

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

      // ── Node intersection markers ─────────────────────────────────────────
      map.addSource('olr-nodes', {
        type: 'geojson',
        data: { type: 'FeatureCollection', features: [] },
      });
      map.addLayer({
        id:     'olr-nodes-circle',
        type:   'circle',
        source: 'olr-nodes',
        layout: { visibility: 'none' },
        paint: {
          'circle-radius':       5,
          'circle-color':        '#ffffff',
          'circle-stroke-width': 1.5,
          'circle-stroke-color': '#444444',
          'circle-opacity':      0.9,
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
      // Direction on the decoded path is conveyed by the numbered LRP markers (1 → N).

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

      // Single icon layer: canvas-generated numbered circles (no glyph/font dependency).
      // ID kept as 'lrp-markers-circle' so existing beforeId refs in layers below still work.
      map.addLayer({
        id:     'lrp-markers-circle',
        type:   'symbol',
        source: 'lrp-markers',
        layout: {
          'icon-image':             ['concat', 'lrp-num-', ['to-string', ['+', ['get', 'index'], 1]]],
          'icon-allow-overlap':     true,
          'icon-ignore-placement':  true,
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
      // Direction triangles on the selected candidate segment (shown when candidatePopup is open)
      map.addLayer({
        id:     'trace-segment-arrow',
        type:   'symbol',
        source: 'trace-segment',
        layout: {
          'symbol-placement':    'line',
          'symbol-spacing':      18,
          'icon-image':          'direction-triangle',
          'icon-size':           1.4,
          'icon-allow-overlap':  true,
          'icon-ignore-placement': true,
          'visibility':          'none',
        },
        paint: {
          'icon-color':        'white',
          'icon-halo-color':   'white',
          'icon-halo-width':   4,
          'icon-halo-blur':    0,
          'icon-opacity':      1.0,
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

      // Found route — dark casing underneath keeps it readable over any basemap colour.
      map.addLayer({
        id: 'replay-route-casing', type: 'line', source: 'replay-route',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: { 'line-color': '#0f172a', 'line-width': 10, 'line-opacity': 0.75 },
      });
      map.addLayer({
        id: 'replay-route-line', type: 'line', source: 'replay-route',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: { 'line-color': '#16a34a', 'line-width': 6, 'line-opacity': 0.95 },
      });

      // Candidate segments — thick green (accepted) / thick red (rejected)
      map.addLayer({
        id: 'replay-candidates-line', type: 'line', source: 'replay-candidates',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: {
          'line-color': ['case',
            ['boolean', ['get', 'winner'], false], '#00ff88',
            ['==', ['get', 'ctype'], 'accepted'],  '#22cc66',
            '#dd2222',
          ],
          'line-width': ['case',
            ['boolean', ['get', 'winner'], false], 7,
            ['==', ['get', 'ctype'], 'accepted'],  5,
            4,
          ],
          'line-opacity': ['case',
            ['boolean', ['get', 'winner'], false], 1.0,
            ['==', ['get', 'ctype'], 'accepted'],  0.9,
            0.75,
          ],
        },
      });
      // Direction triangles — hidden by default; shown only when a candidate is selected
      map.addLayer({
        id: 'replay-candidates-arrow', type: 'symbol', source: 'replay-candidates',
        layout: {
          'symbol-placement':      'line',
          'symbol-spacing':        18,
          'icon-image':            'direction-triangle',
          'icon-size':             1.0,
          'icon-allow-overlap':    true,
          'icon-ignore-placement': true,
          'visibility':            'none',
        },
        paint: {
          'icon-color':   'white',
          'icon-opacity': 0.9,
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

      // Leg from/to markers — inserted below lrp-markers-circle so LRP numbers stay readable
      map.addLayer({
        id: 'replay-leg-from', type: 'circle', source: 'replay-leg',
        filter: ['==', ['get', 'role'], 'from'],
        paint: { 'circle-radius': 9, 'circle-color': '#00ff88', 'circle-stroke-width': 2, 'circle-stroke-color': '#fff' },
      }, 'lrp-markers-circle');
      map.addLayer({
        id: 'replay-leg-to', type: 'circle', source: 'replay-leg',
        filter: ['==', ['get', 'role'], 'to'],
        paint: { 'circle-radius': 9, 'circle-color': '#ff4444', 'circle-stroke-width': 2, 'circle-stroke-color': '#fff' },
      }, 'lrp-markers-circle');

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

      // ── Measurement tool sources + layers ────────────────────────────────
      const emptyFC2 = { type: 'FeatureCollection', features: [] };
      map.addSource('measure-line',   { type: 'geojson', data: emptyFC2 });
      map.addSource('measure-points', { type: 'geojson', data: emptyFC2 });
      map.addLayer({
        id: 'measure-line-layer', type: 'line', source: 'measure-line',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: {
          'line-color':     '#ffffff',
          'line-width':     2,
          'line-dasharray': [4, 4],
          'line-opacity':   0.9,
        },
      });
      map.addLayer({
        id: 'measure-points-layer', type: 'circle', source: 'measure-points',
        paint: {
          'circle-radius':       5,
          'circle-color':        '#ffffff',
          'circle-stroke-color': '#333333',
          'circle-stroke-width': 1.5,
        },
      });

      // ── PointAlongLine result marker ──────────────────────────────────────
      map.addSource('pal-point', { type: 'geojson', data: emptyFC2 });
      map.addLayer({
        id: 'pal-point-layer', type: 'circle', source: 'pal-point',
        paint: {
          'circle-radius':       9,
          'circle-color':        '#ff6b35',
          'circle-stroke-color': '#ffffff',
          'circle-stroke-width': 2.5,
          'circle-opacity':      0.9,
        },
      });

      // ── Click handlers ────────────────────────────────────────────────────
      const pointerOn  = () => { if (!measuringRef.current && !bearingActiveRef.current && !coordCaptureActiveRef.current) map.getCanvas().style.cursor = 'pointer'; };
      const pointerOff = () => {
        if (coordCaptureActiveRef.current) map.getCanvas().style.cursor = 'crosshair';
        else if (!measuringRef.current && !bearingActiveRef.current) map.getCanvas().style.cursor = '';
      };

      for (let frc = 0; frc < 8; frc++) {
        map.on('click', `olr-frc${frc}`, onSegmentClick);
        map.on('mouseenter', `olr-frc${frc}`, pointerOn);
        map.on('mouseleave', `olr-frc${frc}`, pointerOff);
      }

      map.on('click',      'olr-nodes-circle', onNodeClick);
      map.on('mouseenter', 'olr-nodes-circle', pointerOn);
      map.on('mouseleave', 'olr-nodes-circle', pointerOff);

      map.on('click', 'lrp-markers-circle', onLrpClick);
      map.on('mouseenter', 'lrp-markers-circle', pointerOn);
      map.on('mouseleave', 'lrp-markers-circle', pointerOff);

      map.on('mouseenter', 'replay-candidates-line',  pointerOn);
      map.on('mouseleave', 'replay-candidates-line',  pointerOff);

      map.on('click', 'decoded-path-line', onDecodedPathClick);
      map.on('mouseenter', 'decoded-path-line', pointerOn);
      map.on('mouseleave', 'decoded-path-line', pointerOff);

      map.on('click', onMapClick);
      map.on('mousemove', e => { const c = [e.lngLat.lng, e.lngLat.lat]; setCursorCoord(c); cursorCoordRef.current = c; });
      map.getCanvas().addEventListener('mouseleave', () => { setCursorCoord(null); cursorCoordRef.current = null; });

      loadVisibleTiles(map);
    });

    map.on('moveend', () => loadVisibleTiles(map));
    map.on('zoomend', () => loadVisibleTiles(map));

    // Resize the map whenever its container changes size (panel open/close).
    // Debounce so the WebGL canvas only resets once after a CSS transition
    // completes — resizing on every animation frame during a width transition
    // causes one blank frame per resize call, which is visible as flicker.
    let resizeTimer = null;
    const resizeObs = new ResizeObserver(() => {
      if (resizeTimer) clearTimeout(resizeTimer);
      resizeTimer = setTimeout(() => map.resize(), 220);
    });
    resizeObs.observe(mapContainer.current);

    return () => {
      if (resizeTimer) clearTimeout(resizeTimer);
      resizeObs.disconnect();
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
          nodesCacheRef.current.set(key, fc.nodeFeatures ?? []);
        } else {
          tileCache.set(key, []);
          nodesCacheRef.current.set(key, []);
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

    if (map.getSource('olr-nodes')) {
      const nodeFeatures = [];
      for (const [key, nFeats] of nodesCacheRef.current) {
        if (visibleKeys.has(key)) nodeFeatures.push(...nFeats);
      }
      map.getSource('olr-nodes').setData({ type: 'FeatureCollection', features: nodeFeatures });
    }
  }

  // ── Click interaction ────────────────────────────────────────────────────────

  function onNodeClick(e) {
    if (bearingActiveRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = bearingPtsRef.current.length >= 2 ? [pt] : [...bearingPtsRef.current, pt];
      bearingPtsRef.current = next;
      setBearingPts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    if (measuringRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = [...measurePtsRef.current, pt];
      measurePtsRef.current = next;
      setMeasurePts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    if (!e.features?.length) return;
    const props = e.features[0].properties;
    setNodeInfo(props);
    setNodeAnchor({ x: e.point.x, y: e.point.y });
    setInfoProps(null);
    setLrpInfo(null);
    setSegDiagnosis(null);
    e.originalEvent.stopPropagation();
  }

  function onSegmentClick(e) {
    if (bearingActiveRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = bearingPtsRef.current.length >= 2 ? [pt] : [...bearingPtsRef.current, pt];
      bearingPtsRef.current = next;
      setBearingPts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    if (measuringRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = [...measurePtsRef.current, pt];
      measurePtsRef.current = next;
      setMeasurePts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    if (!e.features?.length) return;

    // When multiple features overlap near a segment boundary, pick the one whose
    // polyline geometry is closest to the click point in PIXEL space.  Geographic
    // distance fails at shared endpoints: both segments are equidistant there, so
    // whichever MapLibre returns first wins.  Pixel space matches what the user sees.
    const map = mapRef.current;
    const cp = e.point;                  // {x, y} pixels
    let bestFeat = e.features[0];
    let bestDist = Infinity;
    for (const feat of e.features) {
      const coords = feat.geometry?.coordinates;
      if (!coords?.length) continue;
      let minD = Infinity;
      for (let i = 0; i < coords.length - 1; i++) {
        const ap = map.project(coords[i]);
        const bp = map.project(coords[i + 1]);
        const dx = bp.x - ap.x, dy = bp.y - ap.y;
        const len2 = dx * dx + dy * dy;
        const t = len2 === 0 ? 0 : Math.max(0, Math.min(1, ((cp.x - ap.x) * dx + (cp.y - ap.y) * dy) / len2));
        const ex = cp.x - (ap.x + t * dx), ey = cp.y - (ap.y + t * dy);
        const d = ex * ex + ey * ey;
        if (d < minD) minD = d;
      }
      if (minD < bestDist) { bestDist = minD; bestFeat = feat; }
    }
    const props = bestFeat.properties;
    const [z, x, y] = props.tile.split('/').map(Number);
    const segId = getSegmentId(z, x, y, props.local_index);
    const segCoords = bestFeat.geometry?.coordinates;
    if (segCoords?.length) {
      pendingPopupCoordRef.current = polylineMid(segCoords);
    }
    setHighlightedSegment({ tile: props.tile, local_index: props.local_index });
    setInfoProps({ ...props, segment_id: segId >= 0 ? segId : null });
    setInfoAnchor(null);
    setLrpInfo(null);
    setSegDiagnosis(null);
    e.originalEvent.stopPropagation();
  }

  function onDecodedPathClick(e) {
    if (bearingActiveRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = bearingPtsRef.current.length >= 2 ? [pt] : [...bearingPtsRef.current, pt];
      bearingPtsRef.current = next;
      setBearingPts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    if (measuringRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = [...measurePtsRef.current, pt];
      measurePtsRef.current = next;
      setMeasurePts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    e.originalEvent.stopPropagation();
    const segments = decodeResultRef.current?.segments;
    if (!segments?.length) return;

    const map = mapRef.current;
    const cp = e.point;
    const cache = getSegGeomCache();
    let best = null, bestDist = Infinity;

    for (const s of segments) {
      const [z, x, y] = s.tile.split('/').map(Number);
      const segId = getSegmentId(z, x, y, s.local_index);
      const feat = segId >= 0 ? cache.get(segId) : null;
      if (!feat) continue;
      const coords = feat.geometry.coordinates;
      let minD = Infinity;
      for (let i = 0; i < coords.length - 1; i++) {
        const ap = map.project(coords[i]);
        const bp = map.project(coords[i + 1]);
        const dx = bp.x - ap.x, dy = bp.y - ap.y;
        const len2 = dx * dx + dy * dy;
        const t = len2 === 0 ? 0 : Math.max(0, Math.min(1, ((cp.x - ap.x) * dx + (cp.y - ap.y) * dy) / len2));
        const ex = cp.x - (ap.x + t * dx), ey = cp.y - (ap.y + t * dy);
        const d = ex * ex + ey * ey;
        if (d < minD) minD = d;
      }
      if (minD < bestDist) { bestDist = minD; best = { feat, segId }; }
    }

    if (best) {
      const bestCoords = best.feat.geometry?.coordinates;
      if (bestCoords?.length) {
        pendingPopupCoordRef.current = polylineMid(bestCoords);
      }
      setLrpInfo(null);
      setInfoAnchor(null);
      setInfoProps({ ...best.feat.properties, segment_id: best.segId });
      setSegDiagnosis(null);
      setHighlightedSegment({
        tile:        best.feat.properties.tile,
        local_index: best.feat.properties.local_index,
      });
    }
  }

  function onLrpClick(e) {
    if (bearingActiveRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = bearingPtsRef.current.length >= 2 ? [pt] : [...bearingPtsRef.current, pt];
      bearingPtsRef.current = next;
      setBearingPts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    if (measuringRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = [...measurePtsRef.current, pt];
      measurePtsRef.current = next;
      setMeasurePts(next);
      e.originalEvent.stopPropagation();
      return;
    }
    if (!e.features?.length) return;
    setLrpInfo(e.features[0].properties);
    setInfoAnchor({ x: e.point.x, y: e.point.y });
    setInfoProps(null);
    setHighlightedSegment(null);
    e.stopPropagation();           // stop lower-Z layers (segments) from also firing
    e.originalEvent.stopPropagation();
  }

  function onMapClick(e) {
    if (coordCaptureActiveRef.current) {
      cursorCoordRef.current = [e.lngLat.lng, e.lngLat.lat];
      commitCoordCapture();
      return;
    }
    if (bearingActiveRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = bearingPtsRef.current.length >= 2 ? [pt] : [...bearingPtsRef.current, pt];
      bearingPtsRef.current = next;
      setBearingPts(next);
      return;
    }
    if (measuringRef.current) {
      const pt = [e.lngLat.lng, e.lngLat.lat];
      const next = [...measurePtsRef.current, pt];
      measurePtsRef.current = next;
      setMeasurePts(next);
      return;
    }
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

      // If a popup anchor is waiting, register the moveend listener HERE — after
      // fitBounds — so it can't be triggered by any prior map movement.
      if (pendingPopupCoordRef.current) {
        const coord = pendingPopupCoordRef.current;
        pendingPopupCoordRef.current = null;
        map.once('moveend', () => {
          const pt = map.project(coord);
          setInfoAnchor({ x: Math.max(pt.x, 20), y: pt.y });
        });
      }

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
      // No fitBounds — project pending popup anchor immediately
      if (pendingPopupCoordRef.current) {
        const coord = pendingPopupCoordRef.current;
        pendingPopupCoordRef.current = null;
        const pt = map.project(coord);
        setInfoAnchor({ x: Math.max(pt.x, 20), y: pt.y });
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

    // Clip first/last segment at LRP snap points when highlighting a leg route.
    if (traceHighlightSnaps && features.length > 0) {
      const { from, to } = traceHighlightSnaps;
      if (from && features[0]?.geometry?.coordinates) {
        const coords = clipGeomFromPoint(features[0].geometry.coordinates, from[0], from[1]);
        if (coords) features[0] = { ...features[0], geometry: { type: 'LineString', coordinates: coords } };
      }
      const last = features.length - 1;
      if (to && features[last]?.geometry?.coordinates) {
        const coords = clipGeomToPoint(features[last].geometry.coordinates, to[0], to[1]);
        if (coords) features[last] = { ...features[last], geometry: { type: 'LineString', coordinates: coords } };
      }
    }

    // When a candidate popup is active for a Backward traversal, reverse the
    // coordinate order so trace-segment-arrow chevrons point the correct way.
    const cp = candidatePopupRef.current;
    if (
      cp?.traversal === 'Backward' &&
      features.length === 1 &&
      traceHighlightSegIds?.length === 1 &&
      traceHighlightSegIds[0] === cp.segment_id
    ) {
      const f = features[0];
      features[0] = {
        ...f,
        geometry: { type: 'LineString', coordinates: [...f.geometry.coordinates].reverse() },
      };
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

    // Consume the pending candidate anchor (set by candidatePopup effect) if
    // fitBounds was just called — project it in the same moveend callback.
    const pendingCandCoord = (allCoords.length >= 2) ? pendingCandAnchorCoordRef.current : null;
    if (pendingCandCoord) pendingCandAnchorCoordRef.current = null;

    // Show segment info popup for single-segment trace clicks, but not
    // when a candidate evaluation popup is already being shown.
    let moveEndHandler = null;

    if (features.length === 1 && !candidatePopupRef.current) {
      const feat = features[0];
      // segId is the WASM runtime segment_id — include it so the popup
      // doesn't show "— (decode first)" for Internal ID.
      setInfoProps({ ...feat.properties, segment_id: traceHighlightSegIds[0] });
      setInfoAnchor(null); // defer until fitBounds animation completes
      const coords = feat.geometry?.coordinates;
      if (coords?.length && allCoords.length >= 2) {
        const mid = polylineMid(coords);
        moveEndHandler = () => {
          const pixel = map.project(mid);
          setInfoAnchor({ x: Math.max(pixel.x, 20), y: pixel.y });
          if (pendingCandCoord) {
            const pt = map.project(pendingCandCoord);
            setCandAnchor({ x: pt.x, y: pt.y });
          }
        };
      } else if (coords?.length) {
        // No fitBounds — project immediately
        const pixel = map.project(polylineMid(coords));
        setInfoAnchor({ x: Math.max(pixel.x, 20), y: pixel.y });
      }
    } else if (pendingCandCoord) {
      // Candidate popup open, no segment info popup — just project the cand anchor
      moveEndHandler = () => {
        const pt = map.project(pendingCandCoord);
        setCandAnchor({ x: pt.x, y: pt.y });
      };
    }

    if (moveEndHandler) {
      map.once('moveend', moveEndHandler);
      return () => map.off('moveend', moveEndHandler);
    }
  }, [traceHighlightSegIds, traceHighlightSnaps]);

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
    'replay-candidates-line', 'replay-candidates-arrow',
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
    // Arrow layer is always hidden here; it is shown only when a candidate is selected.
    replayLayerIds.forEach(id => {
      if (id === 'replay-candidates-arrow') return;
      if (map.getLayer(id)) map.setLayoutProperty(id, 'visibility', vis);
    });
    // Hide the full decoded path while replay is active so the per-leg highlight is unambiguous.
    if (map.getLayer('decoded-path-line')) {
      map.setLayoutProperty('decoded-path-line', 'visibility', vis === 'visible' ? 'none' : 'visible');
    }
    if (map.getLayer('replay-candidates-arrow')) {
      map.setLayoutProperty('replay-candidates-arrow', 'visibility', 'none');
    };

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
    const gj = stateToGeoJSON(visualState, id => getSegGeomCache().get(id));
    map.getSource('replay-radius')     ?.setData(gj.radiusFC);
    map.getSource('replay-candidates') ?.setData(gj.candFC);
    map.getSource('replay-cloud')      ?.setData(gj.cloudFC);
    map.getSource('replay-frontier')   ?.setData(gj.frontierFC);
    map.getSource('replay-leg')        ?.setData(gj.legFC);

    // Route geometry — clip the already-correct decoded path WKT between from_snap and to_snap.
    // The decoded path is the canonical geometry (assembled and offset-trimmed in Rust from the
    // full graph); clipping it in JS between the two LRP snap points gives exactly the right
    // portion of the path for this leg without any segment-assembly or traversal-direction logic.
    const { routeFromSnap, routeToSnap } = visualState;
    let routeFeats = [];
    if (routeFromSnap && routeToSnap) {
      const wktCoords = parseWktLinestring(decodeResultRef.current?.wkt);
      if (wktCoords?.length >= 2) {
        const seg1 = clipGeomFromPoint(wktCoords,  routeFromSnap[0], routeFromSnap[1]);
        const seg2 = seg1?.length >= 2
          ? clipGeomToPoint(seg1, routeToSnap[0], routeToSnap[1])
          : null;
        if (seg2?.length >= 2) {
          routeFeats = [{ type: 'Feature', geometry: { type: 'LineString', coordinates: seg2 }, properties: {} }];
        }
      }
    }
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
        zoom:     Math.max(map.getZoom(), 16),
        duration: 400,
      });
    }

    if (currentStep?.type === 'candidates_ranked') {
      // Collect the LRP coord from the preceding search_started for this LRP.
      const pts = [];
      for (let i = replayStep - 1; i >= 0; i--) {
        const s = replaySteps[i];
        if (s.type === 'search_started' && s.lrp_idx === currentStep.lrp_idx) {
          pts.push(s.coord); break;
        }
      }
      for (const c of currentStep.accepted ?? []) {
        if (c.projection?.point) pts.push(c.projection.point);
      }
      for (const r of currentStep.rejected ?? []) {
        if (r.point) pts.push(r.point);
      }
      if (pts.length > 0) {
        const lons = pts.map(p => p[0]), lats = pts.map(p => p[1]);
        const w = Math.min(...lons), e = Math.max(...lons);
        const s = Math.min(...lats), n = Math.max(...lats);
        if (w === e && s === n) {
          map.flyTo({ center: [w, s], zoom: Math.max(map.getZoom(), 16), duration: 300 });
        } else {
          map.fitBounds([[w, s], [e, n]], { padding: 120, maxZoom: 17, duration: 400 });
        }
      }
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
        const swell   = Math.abs(Math.sin(phase));
        try {
          map.setPaintProperty('replay-route-line',   'line-width',   done ? 6  : 5  + 4 * swell);
          map.setPaintProperty('replay-route-line',   'line-opacity', done ? 0.95 : 0.7 + 0.3 * swell);
          map.setPaintProperty('replay-route-casing', 'line-width',   done ? 10 : 9  + 4 * swell);
          map.setPaintProperty('replay-route-casing', 'line-opacity', done ? 0.75 : 0.5 + 0.3 * swell);
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
    if (map.getLayer('olr-highlight'))     map.setLayoutProperty('olr-highlight',     'visibility', vis);
    if (map.getLayer('olr-nodes-circle')) map.setLayoutProperty('olr-nodes-circle', 'visibility', vis);
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
    const palSource         = map.getSource('pal-point');

    const emptyFC = { type: 'FeatureCollection', features: [] };
    if (!decodeResult) {
      pathSource?.setData(emptyFC);
      lrpSource?.setData(emptyFC);
      snapSource?.setData(emptyFC);
      displSource?.setData(emptyFC);
      uncertaintySource?.setData(emptyFC);
      palSource?.setData(emptyFC);
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

    // ── PointAlongLine result point ───────────────────────────────────────────
    if (decodeResult.ok && decodeResult.location_type === 'PointAlongLine' &&
        decodeResult.point_lon != null && decodeResult.point_lat != null) {
      palSource?.setData({
        type: 'FeatureCollection',
        features: [{
          type: 'Feature',
          geometry: { type: 'Point', coordinates: [decodeResult.point_lon, decodeResult.point_lat] },
          properties: {
            orientation: decodeResult.orientation,
            side_of_road: decodeResult.side_of_road,
          },
        }],
      });
    } else {
      palSource?.setData(emptyFC);
    }

    // ── Fit camera — always include all LRP positions AND the decoded path ──────
    const lrpCoords = lrps.map(l => [l.lon, l.lat]);
    const fitCoords = [
      ...lrpCoords,
      ...(wktCoords ?? []),
      ...(decodeResult.ok && decodeResult.location_type === 'PointAlongLine' &&
          decodeResult.point_lon != null
        ? [[decodeResult.point_lon, decodeResult.point_lat]]
        : []),
    ];

    if (fitCoords.length > 0) {
      const lngs = fitCoords.map(c => c[0]);
      const lats = fitCoords.map(c => c[1]);
      const bounds = [[Math.min(...lngs), Math.min(...lats)], [Math.max(...lngs), Math.max(...lats)]];
      const doFit = () => map.fitBounds(bounds, { padding: 80, duration: 600, maxZoom: 17 });
      // Defer one frame so MapLibre has processed the setData calls first
      requestAnimationFrame(doFit);
    }
  }, [decodeResult]);

  // ── Measurement tool ──────────────────────────────────────────────────────────

  function toggleMeasure() {
    if (measuringRef.current) {
      measuringRef.current = false;
      measurePtsRef.current = [];
      setMeasuring(false);
      setMeasurePts([]);
      setMeasureCursor(null);
    } else {
      measuringRef.current = true;
      measurePtsRef.current = [];
      setMeasuring(true);
      setMeasurePts([]);
    }
  }

  // Activate/deactivate measure mode: cursor, mousemove, dblclick.
  useEffect(() => {
    measuringRef.current = measuring;
    const map = mapRef.current;
    if (!map) return;
    if (!measuring) {
      map.getCanvas().style.cursor = '';
      return;
    }
    map.getCanvas().style.cursor = 'crosshair';
    map.doubleClickZoom.disable();

    const onMove = (e) => setMeasureCursor([e.lngLat.lng, e.lngLat.lat]);
    const onDblClick = () => {
      // The second click of the dblclick already added a point via onMapClick;
      // remove that spurious duplicate and finish.
      const trimmed = measurePtsRef.current.slice(0, -1);
      measurePtsRef.current = trimmed;
      setMeasurePts([...trimmed]);
      measuringRef.current = false;
      setMeasuring(false);
      setMeasureCursor(null);
    };

    map.on('mousemove', onMove);
    map.on('dblclick', onDblClick);
    return () => {
      map.off('mousemove', onMove);
      map.off('dblclick', onDblClick);
      map.doubleClickZoom.enable();
      if (!measuringRef.current) map.getCanvas().style.cursor = '';
    };
  }, [measuring]); // eslint-disable-line react-hooks/exhaustive-deps

  // Escape cancels measurement.
  useEffect(() => {
    const onKey = (e) => {
      if (e.key === 'Escape') {
        if (measuringRef.current) {
          measuringRef.current = false;
          measurePtsRef.current = [];
          setMeasuring(false);
          setMeasurePts([]);
          setMeasureCursor(null);
        } else if (bearingActiveRef.current) {
          bearingActiveRef.current = false;
          bearingPtsRef.current = [];
          setBearingActive(false);
          setBearingPts([]);
        }
      }
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, []);

  // Sync measure GeoJSON sources whenever points or cursor change.
  useEffect(() => {
    const map = mapRef.current;
    if (!map || !map.getSource('measure-line')) return;
    const pts = measureCursor ? [...measurePts, measureCursor] : measurePts;
    const lineData = pts.length >= 2
      ? { type: 'FeatureCollection', features: [{ type: 'Feature', geometry: { type: 'LineString', coordinates: pts }, properties: {} }] }
      : { type: 'FeatureCollection', features: [] };
    const pointsData = {
      type: 'FeatureCollection',
      features: measurePts.map(pt => ({ type: 'Feature', geometry: { type: 'Point', coordinates: pt }, properties: {} })),
    };
    map.getSource('measure-line').setData(lineData);
    map.getSource('measure-points').setData(pointsData);
  }, [measurePts, measureCursor]);

  // ── Bearing tool ──────────────────────────────────────────────────────────────

  function toggleBearing() {
    if (bearingActiveRef.current) {
      bearingActiveRef.current = false;
      bearingPtsRef.current = [];
      setBearingActive(false);
      setBearingPts([]);
    } else {
      bearingActiveRef.current = true;
      bearingPtsRef.current = [];
      setBearingActive(true);
      setBearingPts([]);
    }
  }

  useEffect(() => {
    bearingActiveRef.current = bearingActive;
    const map = mapRef.current;
    if (!map) return;
    if (!bearingActive) {
      if (!measuringRef.current) map.getCanvas().style.cursor = '';
      return;
    }
    map.getCanvas().style.cursor = 'crosshair';
    return () => {
      if (!bearingActiveRef.current && !measuringRef.current) map.getCanvas().style.cursor = '';
    };
  }, [bearingActive]); // eslint-disable-line react-hooks/exhaustive-deps

  // ── GeoJSON export ────────────────────────────────────────────────────────────

  function doExportGeoJSON() {
    if (!decodeResult?.ok) return;
    const cache = getSegGeomCache();
    const segments = decodeResult.segments ?? [];
    const traversalDirs = computeTraversalDirections(segments);

    let allCoords = [];
    for (let i = 0; i < segments.length; i++) {
      const feat = cache.get(segments[i].segment_id);
      if (!feat) continue;
      let coords = feat.geometry.coordinates;
      if (traversalDirs[i] === 'Reverse') coords = [...coords].reverse();
      if (allCoords.length === 0) allCoords.push(...coords);
      else allCoords.push(...coords.slice(1));
    }

    const posM = ((decodeResult.pos_offset_lb ?? 0) + (decodeResult.pos_offset_ub ?? 0)) / 2;
    const negM = ((decodeResult.neg_offset_lb ?? 0) + (decodeResult.neg_offset_ub ?? 0)) / 2;
    const trimmed = applyOffsets(allCoords, posM, negM);

    let wkt = null;
    if (decodeResult.location_type === 'PointAlongLine' && decodeResult.point_lon != null) {
      wkt = `POINT(${decodeResult.point_lon.toFixed(7)} ${decodeResult.point_lat.toFixed(7)})`;
    } else if (trimmed.length >= 2) {
      wkt = `LINESTRING(${trimmed.map(([lo, la]) => `${lo.toFixed(7)} ${la.toFixed(7)}`).join(', ')})`;
    }

    const features = segments.map((seg, i) => {
      const feat = cache.get(seg.segment_id);
      let coords = feat?.geometry?.coordinates ?? null;
      if (coords && traversalDirs[i] === 'Reverse') coords = [...coords].reverse();
      return {
        type: 'Feature',
        properties: {
          frc:       seg.frc,
          fow:       seg.fow,
          direction: traversalDirs[i],
          length_m:  seg.length_m,
        },
        geometry: coords ? { type: 'LineString', coordinates: coords } : null,
      };
    });

    const hasPos = (decodeResult.pos_offset_ub ?? 0) > 0;
    const hasNeg = (decodeResult.neg_offset_ub ?? 0) > 0;
    const fc = {
      type: 'FeatureCollection',
      metadata: {
        openlr:           openlrString,
        location_type:    decodeResult.location_type,
        pos_offset_range: hasPos ? [decodeResult.pos_offset_lb, decodeResult.pos_offset_ub] : null,
        neg_offset_range: hasNeg ? [decodeResult.neg_offset_lb, decodeResult.neg_offset_ub] : null,
        ...(decodeResult.location_type === 'PointAlongLine' && decodeResult.point_lon != null
          ? { point_lat: decodeResult.point_lat, point_lon: decodeResult.point_lon }
          : {}),
        wkt,
      },
      features,
    };

    const blob = new Blob([JSON.stringify(fc, null, 2)], { type: 'application/geo+json' });
    const url  = URL.createObjectURL(blob);
    const a    = document.createElement('a');
    a.href     = url;
    a.download = 'openlr-path.geojson';
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    URL.revokeObjectURL(url);

    setExportFlash(true);
    setTimeout(() => setExportFlash(false), 1200);
  }

  function doPermalink() {
    const url = `${window.location.origin}${window.location.pathname}#q=${encodeURIComponent(openlrString)}`;
    navigator.clipboard.writeText(url).catch(() => {});
    setPermalinkCopied(true);
    setTimeout(() => setPermalinkCopied(false), 1500);
  }

  function doZoomGo() {
    const parsed = parseLatLon(zoomInput);
    if (!parsed) { setZoomError(true); return; }
    setZoomError(false);
    const id = `lp-${parsed.lat.toFixed(6)}-${parsed.lon.toFixed(6)}-${performance.now().toFixed(0)}`;
    setLocPins(prev => [...prev, { id, lat: parsed.lat, lon: parsed.lon }]);
    mapRef.current?.flyTo({ center: [parsed.lon, parsed.lat], zoom: 16, duration: 800 });
  }

  function toggleCoordCapture() {
    const nowActive = !coordCaptureActive;
    coordCaptureActiveRef.current = nowActive;
    setCoordCaptureActive(nowActive);
    const canvas = mapRef.current?.getCanvas();
    if (canvas) canvas.style.cursor = nowActive ? 'crosshair' : '';
    if (!nowActive) {
      setCursorCoord(null);
      cursorCoordRef.current = null;
      if (capturePopupRef.current) { capturePopupRef.current.remove(); capturePopupRef.current = null; }
    }
  }

  function commitCoordCapture() {
    const coord = cursorCoordRef.current;
    if (!coord) return;
    const [lon, lat] = coord;
    const text = `${lat.toFixed(6)}, ${lon.toFixed(6)}`;

    // Copy immediately
    navigator.clipboard.writeText(text).catch(() => {});
    setCopiedText(text);
    setCoordCopied(true);
    setTimeout(() => { setCoordCopied(false); setCopiedText(''); }, 1500);

    // Deactivate capture mode
    coordCaptureActiveRef.current = false;
    setCoordCaptureActive(false);
    setCursorCoord(null);
    cursorCoordRef.current = null;
    const canvas = mapRef.current?.getCanvas();
    if (canvas) canvas.style.cursor = '';

    // Show popup at the captured location offering "Add pin"
    const map = mapRef.current;
    if (!map) return;
    if (capturePopupRef.current) { capturePopupRef.current.remove(); capturePopupRef.current = null; }

    const content = document.createElement('div');
    content.className = 'loc-pin-popup';
    content.innerHTML = `<div class="loc-pin-coord">✓ Copied: ${text}</div>
      <div class="loc-pin-btns"><button class="loc-pin-dismiss capture-addpin-btn">Add pin</button></div>`;

    const popup = new maplibregl.Popup({ closeButton: true, offset: 0, className: 'loc-pin-popup-wrap' })
      .setLngLat([lon, lat])
      .setDOMContent(content)
      .addTo(map);

    capturePopupRef.current = popup;
    popup.on('close', () => { capturePopupRef.current = null; });

    content.querySelector('.capture-addpin-btn').addEventListener('click', () => {
      const id = `lp-${lat.toFixed(6)}-${lon.toFixed(6)}-${performance.now().toFixed(0)}`;
      setLocPins(prev => [...prev, { id, lat, lon }]);
      popup.remove();
    });
  }

  useEffect(() => {
    if (!coordCaptureActive) return;
    function onKeyDown(e) {
      if (e.key === 'Enter') { e.preventDefault(); commitCoordCapture(); }
      else if (e.key === 'Escape') { toggleCoordCapture(); }
    }
    document.addEventListener('keydown', onKeyDown);
    return () => document.removeEventListener('keydown', onKeyDown);
  }, [coordCaptureActive]); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Location pin markers ─────────────────────────────────────────────────────
  useEffect(() => {
    const map = mapRef.current;
    if (!map) return;

    // Remove markers for pins that were dismissed
    const currentIds = new Set(locPins.map(p => p.id));
    Object.keys(locPinMarkersRef.current).forEach(id => {
      if (!currentIds.has(id)) {
        locPinMarkersRef.current[id].marker.remove();
        delete locPinMarkersRef.current[id];
      }
    });

    // Add markers for new pins
    locPins.forEach(pin => {
      if (locPinMarkersRef.current[pin.id]) return;

      const el = document.createElement('div');
      el.className = 'loc-pin-marker';
      el.textContent = '📍';

      const content = document.createElement('div');
      content.className = 'loc-pin-popup';
      content.innerHTML = `<div class="loc-pin-coord">${pin.lat.toFixed(6)}, ${pin.lon.toFixed(6)}</div>
        <div class="loc-pin-btns">
          <button class="loc-pin-dismiss">Dismiss</button>
          <button class="loc-pin-dismiss-all">Dismiss all</button>
        </div>`;
      content.querySelector('.loc-pin-dismiss').addEventListener('click', () =>
        setLocPins(prev => prev.filter(p => p.id !== pin.id)));
      content.querySelector('.loc-pin-dismiss-all').addEventListener('click', () =>
        setLocPins([]));

      const popup = new maplibregl.Popup({ closeButton: true, offset: 28, className: 'loc-pin-popup-wrap' })
        .setDOMContent(content);

      const marker = new maplibregl.Marker({ element: el, anchor: 'bottom' })
        .setLngLat([pin.lon, pin.lat])
        .setPopup(popup)
        .addTo(map);

      locPinMarkersRef.current[pin.id] = { marker, popup };
    });
  }, [locPins]); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Render ───────────────────────────────────────────────────────────────────

  return (
    <div className="map-wrap">
      <div ref={mapContainer} className="map-container" />

      {/* Status overlay */}
      {status && <div className="map-status">{status}</div>}

      {/* Segment info panel */}
      {infoProps && infoAnchor && (() => {
        const { style: segStyle, placement: segPl, tipLeft: segTipLeft } = popupPlacement(infoAnchor, 260, mapContainer.current?.offsetWidth, mapContainer.current?.offsetHeight);
        return (
        <div ref={segPanelRef}
          className="seg-info-panel"
          style={segPos ? { position: 'absolute', left: segPos.left, top: segPos.top, right: 'auto', bottom: 'auto' } : segStyle}>
          <header className="seg-info-header" onMouseDown={segMouseDown}>
            <span>
              Segment{infoProps.source_id != null ? ` ${infoProps.source_id}` : ''}
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
                  ['FRC',       `${infoProps.frc_name} (${infoProps.frc})`],
                  ['FOW',       `${infoProps.fow_name} (${infoProps.fow})`],
                  ['Direction', infoProps.direction],
                  ['Length',    `${infoProps.length_m} m`],
                  ['Tile',         infoProps.tile],
                  ['Tile Index',   infoProps.local_index],
                  ['Start Node',   infoProps.start_node  ?? '—'],
                  ['End Node',     infoProps.end_node    ?? '—'],
                  ['Segment Key',  infoProps.source_id   ?? '—'],
                  ['Internal ID',  infoProps.segment_id  != null ? infoProps.segment_id : '— (decode first)'],
                ].map(([k, v]) => (
                  <tr key={k}>
                    <td className="seg-info-key">{k}</td>
                    <td><b>{v}</b></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {decodeResult && !segDiagnosis && (
            <button
              className="seg-diag-btn"
              onClick={() => setSegDiagnosis(diagnoseSegment(
                infoProps.segment_id ?? null,
                infoProps,
                decodeResult,
                decodeResult?.trace?.params?.lfrcnp_tolerance ?? lfrcnpTolerance,
              ))}
            >
              Why didn't the location cover this segment?
            </button>
          )}
          {segDiagnosis && (
            <div className="seg-diag-body">
              <div className="seg-diag-headline">{segDiagnosis.headline}</div>
              {segDiagnosis.bullets.length > 0 && (
                <ul className="seg-diag-list">
                  {segDiagnosis.bullets.map((b, i) => <li key={i}>{b}</li>)}
                </ul>
              )}
              {segDiagnosis.suggestions.length > 0 && (
                <div className="seg-diag-suggestions">
                  <span className="seg-diag-try">Try:</span>
                  <ul className="seg-diag-list">
                    {segDiagnosis.suggestions.map((s, i) => <li key={i}>{s}</li>)}
                  </ul>
                </div>
              )}
              <button className="seg-diag-back" onClick={() => setSegDiagnosis(null)}>
                ↩ Back
              </button>
            </div>
          )}
          {!segPos && <TipSvg placement={segPl} tipLeft={segTipLeft} />}
        </div>
        );
      })()}

      {/* LRP info panel */}
      {lrpInfo && infoAnchor && (() => {
        const { style: lrpStyle, placement: lrpPl, tipLeft: lrpTipLeft } = popupPlacement(infoAnchor, 260, mapContainer.current?.offsetWidth, mapContainer.current?.offsetHeight);
        return (
        <div ref={lrpPanelRef}
          className="seg-info-panel"
          style={lrpPos ? { position: 'absolute', left: lrpPos.left, top: lrpPos.top, right: 'auto', bottom: 'auto' } : lrpStyle}>
          <header className="seg-info-header" onMouseDown={lrpMouseDown}>
            <span>LRP {lrpInfo.index + 1}</span>
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
                  ['LFRCNP',  lrpInfo.lfrcnp !== null
                    ? (lfrcnpTolerance > 0
                      ? `${lrpInfo.lfrcnp} → ${Math.min(lrpInfo.lfrcnp + lfrcnpTolerance, 7)}`
                      : lrpInfo.lfrcnp)
                    : '— (last LRP)'],
                  ['Bearing', formatBearing(lrpInfo.bearing_lb, lrpInfo.bearing_ub)],
                ].map(([k, v]) => (
                  <tr key={k}><td className="seg-info-key">{k}</td><td><b>{v}</b></td></tr>
                ))}
                <tr>
                  <td colSpan={2} style={{ paddingTop: '4px' }}>
                    <BearingCompass lb={lrpInfo.bearing_lb} ub={lrpInfo.bearing_ub} />
                  </td>
                </tr>
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
          {!lrpPos && <TipSvg placement={lrpPl} tipLeft={lrpTipLeft} />}
        </div>
        );
      })()}

      {/* Node intersection popup */}
      {nodeInfo && (() => {
        const { style: nodeStyle, placement: nodePl, tipLeft: nodeTipLeft } = popupPlacement(nodeAnchor, 260, mapContainer.current?.offsetWidth, mapContainer.current?.offsetHeight);
        return (
        <div
          className="seg-info-panel"
          style={nodeStyle}>
          <header className="seg-info-header">
            <span>Node {nodeInfo.local_index}</span>
            <button className="seg-info-close" onClick={() => { setNodeInfo(null); setNodeAnchor(null); }}>✕</button>
          </header>
          <div className="seg-info-body">
            <table>
              <tbody>
                {[
                  ['Lat',      Number(nodeInfo.lat).toFixed(6)],
                  ['Lon',      Number(nodeInfo.lon).toFixed(6)],
                  ['Node ID',  nodeInfo.node_id],
                  ['Tile',     nodeInfo.tile],
                ].map(([k, v]) => (
                  <tr key={k}><td className="seg-info-key">{k}</td><td><b>{v}</b></td></tr>
                ))}
              </tbody>
            </table>
          </div>
          <TipSvg placement={nodePl} tipLeft={nodeTipLeft} />
        </div>
        );
      })()}

      {/* Candidate info popup */}
      {candidatePopup && candAnchor && (() => {
        const { style: candStyle, placement: candPl, tipLeft: candTipLeft } = popupPlacement(candAnchor, 320, mapContainer.current?.offsetWidth, mapContainer.current?.offsetHeight);
        return (
        <div ref={candPanelRef}
          className="seg-info-panel cand-panel"
          style={candPos ? { position: 'absolute', left: candPos.left, top: candPos.top, right: 'auto', bottom: 'auto' } : candStyle}>
          <header className="seg-info-header" onMouseDown={candMouseDown}>
            <span>
              LRP {candidatePopup.lrp_idx + 1} candidate
              {candidatePopup.winner && <span className="cand-winner-badge"> ★ chosen</span>}
            </span>
            <button className="seg-info-close" onClick={() => { clearCandidatePopup(); setCandAnchor(null); candResetPos(); }}>✕</button>
          </header>
          <div className="seg-info-body">
            <CandidatePopupBody p={candidatePopup} />
          </div>
          {!candPos && <TipSvg placement={candPl} tipLeft={candTipLeft} />}
        </div>
        );
      })()}

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

      {/* ── Map tools toolbar ──────────────────────────────────────────────── */}

      {/* Toggle button — always visible */}
      <button
        className={`map-toolbar-toggle${toolbarOpen ? ' active' : ''}`}
        onClick={() => setToolbarOpen(v => !v)}
        title={toolbarOpen ? 'Hide tools' : 'Show tools'}
      >⚙</button>

      {/* Collapsible tool buttons */}
      <div className={`map-toolbar${toolbarOpen ? ' open' : ''}`}>
        <button
          className={`map-tool-btn${coordCaptureActive ? ' coord-capture-active' : ''}`}
          onClick={toggleCoordCapture}
          title={coordCaptureActive ? 'Cancel (Esc)' : 'Capture coordinates'}
        >📍</button>
        <button
          className={`map-tool-btn${showZoomPanel ? ' active' : ''}`}
          onClick={() => { setShowZoomPanel(v => !v); setZoomError(false); }}
          title="Zoom to coordinates"
        >🔍</button>
        <button
          className={`map-tool-btn${measuring ? ' active' : ''}`}
          onClick={toggleMeasure}
          title={measuring ? 'Cancel measurement (Esc)' : measurePts.length > 0 ? 'Clear measurement' : 'Measure distance'}
        >📏</button>
        <button
          className={`map-tool-btn${bearingActive ? ' active' : ''}`}
          onClick={toggleBearing}
          title={bearingActive ? 'Cancel bearing tool (Esc)' : 'Measure bearing and distance between two points'}
        >🧭</button>
        <button
          className={`map-tool-btn${exportFlash ? ' flash' : ''}${!decodeResult?.ok ? ' disabled' : ''}`}
          onClick={doExportGeoJSON}
          disabled={!decodeResult?.ok}
          title={decodeResult?.ok ? 'Export decoded path as GeoJSON' : 'Decode a location first'}
        >⬇</button>
        <button
          className={`map-tool-btn${permalinkCopied ? ' flash' : ''}`}
          onClick={doPermalink}
          title="Copy permalink to clipboard"
        >🔗</button>
      </div>

      {/* Tool panels — outside toolbar so they stay visible when toolbar collapses */}
      {coordCaptureActive && cursorCoord && (
        <div className="coord-display" title="Click map or press Enter to copy">
          {cursorCoord[1].toFixed(5)}, {cursorCoord[0].toFixed(5)}
        </div>
      )}
      {coordCopied && copiedText && (
        <div className="coord-display copied">✓ {copiedText}</div>
      )}

      {showZoomPanel && (
        <div className="zoomloc-panel">
          <input
            className={`zoomloc-input${zoomError ? ' error' : ''}`}
            placeholder="lat, lon"
            value={zoomInput}
            onChange={e => { setZoomInput(e.target.value); setZoomError(false); }}
            onKeyDown={e => e.key === 'Enter' && doZoomGo()}
            autoFocus
          />
          <button className="zoomloc-go" onClick={doZoomGo}>Go</button>
        </div>
      )}

      {(measuring || measurePts.length > 0) && (() => {
        const total = measurePts.reduce((sum, pt, i) =>
          i === 0 ? 0 : sum + haversineM(measurePts[i-1][0], measurePts[i-1][1], pt[0], pt[1]), 0);
        const pending = measuring && measureCursor && measurePts.length > 0
          ? haversineM(measurePts[measurePts.length-1][0], measurePts[measurePts.length-1][1], measureCursor[0], measureCursor[1])
          : null;
        return (
          <div className="measure-panel">
            {measurePts.length === 0 && <span className="measure-hint">Click to start</span>}
            {measurePts.length === 1 && pending == null && <span className="measure-hint">Click to add points</span>}
            {measurePts.length >= 2 && <span className="measure-total">{fmtDist(total)}</span>}
            {pending != null && (
              <span className="measure-pending">
                {measurePts.length >= 2 ? ' + ' : ''}{fmtDist(pending)}
                {measurePts.length >= 2 && <span className="measure-grand"> = {fmtDist(total + pending)}</span>}
              </span>
            )}
            {measuring && measurePts.length >= 1 && (
              <span className="measure-hint"> · dbl-click to finish</span>
            )}
          </div>
        );
      })()}

      {(bearingActive || bearingPts.length > 0) && (() => {
        const result = bearingPts.length === 2 ? (() => {
          const [p1, p2] = bearingPts;
          const dist = haversineM(p1[0], p1[1], p2[0], p2[1]);
          const bear = compassBearing(p1[0], p1[1], p2[0], p2[1]);
          return { dist, bear };
        })() : null;
        return (
          <div className="bearing-panel">
            {bearingPts.length === 0 && <span className="measure-hint">Click to set start point</span>}
            {bearingPts.length === 1 && <span className="measure-hint">Click to set end point</span>}
            {result && <>
              <span className="measure-total">{result.bear.toFixed(1)}°</span>
              <span className="bearing-sep"> · </span>
              <span className="measure-total">{fmtDist(result.dist)}</span>
              {bearingActive && <span className="measure-hint"> · click to remeasure</span>}
            </>}
          </div>
        );
      })()}

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
  const segKey       = p.source_id ?? p.segment_id;

  return (
    <table className="cand-table">
      <tbody>
        {/* Verdict */}
        <tr>
          <td className="seg-info-key">Result</td>
          <td><b className={accepted ? 'cand-accepted' : 'cand-rejected'}>{resultLabel}</b></td>
        </tr>
        {verdictLine &&
          <tr><td className="seg-info-key"></td><td className="cand-verdict-detail">{verdictLine}</td></tr>}

        {/* Segment */}
        <tr><td colSpan={2} className="cand-section">Segment</td></tr>
        {segKey != null &&
          <tr><td className="seg-info-key">Key</td><td><b>{segKey}</b></td></tr>}
        {p.traversal &&
          <tr><td className="seg-info-key">Traversal</td><td><b>{p.traversal}</b></td></tr>}
        {p.frc_name != null &&
          <tr><td className="seg-info-key">FRC</td><td><b>{p.frc_name} ({p.frc})</b></td></tr>}
        {p.fow_name != null &&
          <tr><td className="seg-info-key">FOW</td><td><b>{p.fow_name} ({p.fow})</b></td></tr>}
        {p.direction != null &&
          <tr><td className="seg-info-key">Direction</td><td><b>{p.direction}</b></td></tr>}
        {p.length_m != null &&
          <tr><td className="seg-info-key">Length</td><td><b>{p.length_m} m</b></td></tr>}

        {/* Projection */}
        <tr><td colSpan={2} className="cand-section">Projection</td></tr>
        <tr><td className="seg-info-key">Dist from LRP</td><td><b>{fmt(p.distance_m)} m</b></td></tr>
        {p.arc_offset_m != null &&
          <tr><td className="seg-info-key">Arc offset</td><td><b>{fmt(p.arc_offset_m)} m</b></td></tr>}
        {p.bearing_deg != null &&
          <tr><td className="seg-info-key">Bearing</td><td><b>{fmt(p.bearing_deg, 1)}°</b></td></tr>}
        {p.snap_lat != null &&
          <tr><td className="seg-info-key">Snap point</td><td><b style={{fontSize:'11px'}}>{Number(p.snap_lat).toFixed(6)}, {Number(p.snap_lon).toFixed(6)}</b></td></tr>}

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
