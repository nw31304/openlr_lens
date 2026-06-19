# OpenLRLens — Web Frontend

This document describes the web frontend: its architecture, component tree, state
management, WASM decode protocol, MapLibre layer model, and the tile geometry caching
pattern. It is the canonical reference for resuming frontend work after a context gap.

---

## 1. Overview

The frontend is a **Vite + React SPA** that runs entirely client-side. There is no
backend server — the map data comes from range-read HTTP requests against a PMTiles
archive, and decoding runs inside a WASM module compiled from the Rust engine.

```
Browser
  App.jsx         — startup, WASM init, tile base URL from ?tiles= param
    TopBar.jsx    — OpenLR string input, preset, toggles, Decode button
    ParamsPanel.jsx — all DecodeParams fields; FRC/FOW penalty tables
    MapView.jsx   — MapLibre GL JS canvas; 6 custom GeoJSON sources/layers
    ResultPanel.jsx — decoded segment list; click-to-highlight
    TracePanel.jsx  — full decode trace: candidates, A*, DNP, offsets, result
```

Development entry: `web/` directory, `npm run dev` (Vite dev server on :5173, tile
server on :5176 with HTTP 206 range support).

---

## 2. Startup sequence (`App.jsx`)

1. Parse `?tiles=<base>` from the URL query string. In dev, prepend
   `http://localhost:5176` unless the value is an absolute URL.
2. Fetch `{base}/manifest.json` → `{ archive, tile_zoom }`.
3. Instantiate `PMTiles({base}/{archive})`.
4. Call `initWasm()` (`wasm.js`) → WASM module `decoder`.
5. Call `setPmtiles(pmtiles)`, `setDecoder(decoder)`, `setZoom(manifest.tile_zoom)` on
   the module-level refs in `store.js`.
6. Set `ready = true` → `MapView` receives `tilesBase` prop and begins loading.

---

## 3. State management (`store.js`)

Uses **Zustand**. All shared UI state lives here.

### Store fields

| Field | Type | Description |
|---|---|---|
| `openlrString` | string | Raw input string |
| `preset` | 'Permissive' \| 'Default' \| 'Strict' | Active preset name |
| `params` | `DecodeParams` object | All decode parameters |
| `showParams` | bool | ParamsPanel visible |
| `showTrace` | bool | TracePanel visible |
| `showSegmentLayer` | bool | OLR segment FRC layer visible |
| `showReplay` | bool | ReplayPanel visible |
| `decoding` | bool | Decode in progress |
| `decodeResult` | object \| null | Last decode result from WASM |
| `highlightedSegment` | `{tile, local_index}` \| null | Segment highlighted from ResultPanel |
| `traceHighlightSegIds` | `number[]` \| null | Segment IDs to highlight from TracePanel |
| `traceLrpFocus` | `{lon, lat, index, …, _tick}` \| null | LRP to pan to (with `_tick` to allow re-click) |
| `replaySteps` | `Step[]` | Pre-built display steps from `buildReplaySteps()` |
| `replayStats` | `{maxG, totalNodes, phases}` \| null | Summary stats for the replay (used for colour normalisation and timeline phases) |
| `replayStep` | number | Current display step index (0-based) |

### Zustand `persist` and the `merge` function

Params are persisted to `localStorage` under the key `openlrlens-settings`. The `persist`
middleware uses a custom `merge` function so that new fields added to `PRESETS.Default` in
a future release survive localStorage upgrades without reverting to `undefined`:

```js
merge: (persisted, current) => ({
  ...current,
  ...persisted,
  params: { ...current.params, ...persisted.params },
}),
```

This deep-merges `params`: persisted values win, but any field present in the current default
and absent from the persisted object (e.g. a newly added field) falls back to the current
default rather than being silently lost.

### Three module-level caches (not Zustand)

These are plain `Map` instances at module scope in `store.js`, rebuilt on every decode:

| Variable | Key | Value | Purpose |
|---|---|---|---|
| `_tileGeomCache` | `tile_key` (`"z/x/y"`) | `GeoJSON Feature[]` | All decoded-tile features for fallback lookup |
| `_segIdToTile` | `segment_id` (number) | `{tile_key, local_index}` | Bridge from engine segment ID to tile index |
| `_segGeomCache` | `segment_id` (number) | `GeoJSON Feature` | Direct O(1) lookup used by trace highlight |

These are exported via getter functions (`getSegGeomCache`, `getSegIdToTile`,
`getTileGeomCache`) and read in `Map.jsx` effects.

---

## 4. WASM decode protocol

The WASM module is a three-step call sequence managed in `store.js::runDecode()`:

```js
// Step 1 — parse OpenLR string, compute required tiles
const startResult = JSON.parse(decoder.start(openlrString, paramsJson, zoom));
// startResult.tiles = [[z, x, y], ...]

// Step 2 — fetch and load each tile
for each [z, x, y] of startResult.tiles:
  const bytes = await pmtiles.getZxy(z, x, y);
  decoder.load_tile(z, x, y, new Uint8Array(bytes.data));

// Step 3 — run decode
const result = JSON.parse(decoder.decode());
```

After step 2, the three caches are populated:
- `_tileGeomCache`: from `decodeTile(res.data, z, x, y).features` (JS-side tile decode)
- `_segIdToTile` and `_segGeomCache`: from `decoder.all_segment_tile_mappings()`, an
  O(n) WASM bulk export of `[[segId, z, x, y, local_index], …]`.

### Important: `isStyleLoaded()` must NOT guard custom-source effects

MapLibre's `isStyleLoaded()` returns `false` while the basemap background tiles are
loading — this can be long after the custom sources are fully set up. Effects that
operate on custom sources (e.g. `trace-segment`, `highlighted-segment`) must guard with
`if (!map.getSource('source-name')) return;` rather than `if (!map.isStyleLoaded())`.

---

## 5. MapLibre sources and layers (`Map.jsx`)

GeoJSON sources added on the `map.on('load')` callback, split into permanent and replay groups:

**Permanent sources:**

| Source | Layer(s) | Purpose |
|---|---|---|
| `olr-segments` | `olr-frc0` … `olr-frc7`, `olr-highlight` | All road segments, FRC-coloured; toggled by Segs button |
| `decoded-path` | `decoded-path-line` | Solid decoded path (cyan), trimmed at `pos_offset_ub` / `neg_offset_ub` |
| `decoded-path-uncertainty-pos` | `decoded-path-uncertainty-pos-line` | Dashed cyan uncertainty cap at path start |
| `decoded-path-uncertainty-neg` | `decoded-path-uncertainty-neg-line` | Dashed cyan uncertainty cap at path end |
| `lrp-markers` | `lrp-markers-circle` | LRP point markers (purple circles) |
| `highlighted-segment` | `highlighted-segment-halo`, `highlighted-segment-line` | Segment highlighted from ResultPanel or TracePanel; animated pulse halo |
| `trace-segment` | `trace-segment-halo`, `trace-segment-line` | Orange highlight driven by TracePanel segment buttons |

**Replay sources** (all toggled via `visibility` when `showReplay` changes):

| Source | Layer(s) | Purpose |
|---|---|---|
| `replay-radius` | `replay-radius-fill`, `replay-radius-line` | LRP candidate search radius ring (dashed purple) |
| `replay-route` | `replay-route-line` | Found leg route (gold line); pulsed for 3 s when `route_found` fires |
| `replay-candidates` | `replay-candidates-circle` | Candidate snap points, coloured by gate result (see §15) |
| `replay-cloud` | `replay-cloud-circle` | A* expanded-node cloud; colour by `g_m/maxG` ramp (blue→yellow→red) |
| `replay-frontier` | `replay-frontier-circle` | Latest `FRONTIER_SIZE=25` A* nodes; sinusoidal pulse animation |
| `replay-leg` | `replay-leg-from`, `replay-leg-to` | Green (from) / red (to) leg endpoint markers |
| `replay-flash` | `replay-flash-ring` | Sonar-ping ring on the newest A* node; expands and fades over 2 s |

### Offset uncertainty visualization

The decoded path uses three geometrically non-overlapping segments to visualize v3 offset
uncertainty:

1. **Solid path** (`decoded-path`): the conservative result, trimmed at `pos_offset_ub`
   forward and `neg_offset_ub` backward. This is the portion definitely inside the reference.
2. **Positive cap** (`decoded-path-uncertainty-pos`): dashed line from `pos_offset_lb` to
   `pos_offset_ub` at the path head — the uncertain "start" zone.
3. **Negative cap** (`decoded-path-uncertainty-neg`): dashed line from `neg_offset_ub` to
   `neg_offset_lb` at the path tail — the uncertain "end" zone.

The dashes use `line-dasharray: [1, 0.5]` (dense short dashes) at `line-width: 2`.

The three segments are geometrically non-overlapping by construction: the solid path starts
where the positive cap ends (`pos_offset_ub`), and ends where the negative cap starts
(`neg_offset_ub`). The WASM result carries both `wkt` (midpoint trim, for the midpoint
estimate) and `conservative_wkt` (UB trim, for the solid portion). When no offsets are
present, only the solid `decoded-path` source is populated.

### `olr-segments` — tile inspector layer

Populated by `loadVisibleTiles()`, which runs on map load, `moveend`, and `zoomend`.
It maintains its own `tileCacheRef` (a `Map` keyed by tile key) and its own PMTiles
reader (`pmtilesRef`) — separate from the decode-time reader in `store.js`. Both
readers share the same underlying HTTP cache via the browser.

The segment layer is hidden (`visibility: 'none'`) until the user clicks "Segs". It
becomes visible once `showSegmentLayer` is true and `zoom ≥ MIN_LOAD_ZOOM (10)`.

### Highlight sync effects

Three React effects in `Map.jsx` react to Zustand state:

1. **`[highlightedSegment, traceHighlightSegIds]`**: updates `highlighted-segment`
   source. Reads decode result via `decodeResultRef` (a ref, not a dependency) to
   avoid racing with the decode-result effect. Starts/cancels the sinusoidal halo pulse
   animation via `requestAnimationFrame`.

2. **`[traceHighlightSegIds]`**: updates `trace-segment` source using the three
   module-level caches. Primary path: `segGeomCache.get(segId)`. Fallback: two-step
   lookup via `segIdToTile` + `tileGeomCache`. If a single segment is highlighted,
   also shows a segment info popup.

3. **`[traceLrpFocus]`**: calls `map.flyTo()` to pan to the LRP and shows the LRP
   info popup. Clears `traceLrpFocus` after acting so the same LRP can be clicked again.

4. **`[showSegmentLayer]`**: toggles visibility of `olr-frc*` and `olr-highlight` layers.

5. **`[decodeResult]`**: populates `decoded-path` and `lrp-markers` sources; calls
   `map.fitBounds()` (deferred one frame via `requestAnimationFrame` to allow `setData`
   to process first).

---

## 6. Tile decoder (`tileDecoder.js`)

Decodes the custom OLRL v2 binary tile payload into GeoJSON features. Used both by
the segment-inspector layer (in `Map.jsx`) and at decode time (in `store.js`) to build
`_tileGeomCache`.

### Binary layout read by the JS decoder

```
Header            40 bytes
Segment array     segment_count × 32 bytes
Seg GERS-id table segment_count × 16 bytes   (new in v2)
Geometry pool     geom_vertex_count × 8 bytes  (lon_e7: i32, lat_e7: i32, LE)
Node table        node_count × 28 bytes
Intra restrictions restriction_count × 16 bytes
Cross restrictions xrestriction_count × 40 bytes
```

Each feature's `properties` includes: `frc`, `frc_name`, `fow`, `fow_name`, `direction`,
`length_m`, `tile` (`"z/x/y"`), `local_index` (segment array index), `osm_way_id`
(extracted from the GERS-id stable-ID encoding — bytes 8–15 must be zero).

The `local_index` property is the canonical join key between JS feature caches and
WASM segment references.

---

## 7. TopBar (`TopBar.jsx`)

Contains:
- **OpenLR input**: text input; Enter key triggers decode
- **Gear menu** (`⚙`): dropdown with toggle rows for Road segments, Trace panel, and Replay; plus a **Trace level** button group (Off / Summary / Full) that sets `params.trace_level`; plus Parameters… and Reset to defaults actions
- **▶ Replay button**: appears only when `replaySteps.length > 0`; toggles `showReplay`
- **Decode button**: calls `runDecode()`; disabled while `decoding` is true

`trace_level` must be set to **Full** before decoding to get `AStarNodeExpanded` events, which are required for A\* visualisation in the replay. Summary level records candidates and routing outcomes only.

---

## 8. Params panel (`ParamsPanel.jsx`)

Shows all fields from `DecodeParams` as labelled inputs, including:
- Spatial: `candidate_search_radius_m`, `snap_to_endpoint_threshold_m`
- Weights: `distance_weight`, `bearing_weight`, `bearing_penalty_per_bucket`,
  `frc_weight`, `fow_weight`, `interior_weight`, `wrong_endpoint_weight`
- Hard gates: `max_bearing_deviation_deg`, `max_candidate_score`
- Routing: `max_candidates_per_lrp`, `dnp_tolerance_pct`, `max_path_search_factor`,
  `max_astar_expansions`, `lfrcnp_tolerance`
- Trace: `trace_level` (Off / Summary / Full)

Also renders two editable 8×8 penalty tables (`frc_penalty_table`, `fow_penalty_table`).
Changes call `setParam(key, value)` or `setTableCell(tableKey, row, col, value)` on the
store. Mutating any cell clears the preset name (shows "Custom").

---

## 9. Result panel (`ResultPanel.jsx`)

Shown after a successful decode. Draggable (via `useDraggable`). Shifts left when
the trace panel is open (`right: '416px'` unless dragged).

Lists all decoded segments with Seg ID, FRC, FOW, and OSM way link. Clicking a row
calls `setHighlightedSegment({tile, local_index})`, which the highlight-sync effect
in `Map.jsx` turns into a map highlight + camera pan.

---

## 10. Trace panel (`TracePanel.jsx`)

Shown when the `⚡` button is active. Draggable. Shows a structured view of the
`decodeResult.trace.events` array.

### Trace on decode failure

When a decode fails, the WASM module now includes the partial trace in the error response
(via `DecodeFailure` in the engine). The TracePanel renders normally even on failure — the
user can inspect which candidates were found, which were rejected, and why. The Copy JSON
button is enabled whenever `decodeResult` is non-null (success or failure).

### Event parsing (`parseTraceEvents`)

Partitions the flat event list into:
- `candidates[lrp_idx]` — `{ searchStart, evaluated[], ranked }` per LRP
- `routing[leg]` — `{ start, astarNodes[], astarSkipped[], result, dnp }` per leg
- `offsets[]` — offset application events
- `decodeComplete` — terminal outcome

### Sections rendered

- **Codec**: input string + LRP table (lon, lat, FRC, FOW, bearing, LFRCNP). Clicking
  a row calls `setTraceLrpFocus({…, index})` → map pans to LRP, shows LRP popup.

- **Candidates — LRP N**: accepted candidates ranked by score (lower = better). The
  top row has the `tp-best-row` style. Each candidate's Seg cell is a `SegBtn`.
  Below the accepted table, a collapsible `RejectedTable` shows all rejected candidates
  (expandable via "▸ N rejected" toggle). Each row shows: segment ID (`SegBtn`),
  direction (Fwd/Bwd), distance, bearing, and a colour-coded gate-failure pill:
  - `tp-gate-bearing` (amber): bearing deviation exceeded `max_bearing_deviation_deg`
  - `tp-gate-radius` (yellow): outside `candidate_search_radius_m`
  - `tp-gate-score` (purple): total score exceeded `max_candidate_score`
  - `tp-gate-other` (grey): degenerate geometry (`FailDirection`)

- **Routing — Leg N**: From/To segment buttons; path highlight button `[N segs]`;
  DNP check result. For direct-match legs (both LRPs on the same segment, DNP = 0),
  shows the segment from the top candidates with "same-segment match" note instead of
  From/To. If `trace_level = Full`, shows A* node expansion table (capped at 200
  rows) and skipped-edge details.

- **Offsets**: positive/negative trim values.

- **Result**: success/failure, segment count, offset amounts, WKT preview.
  The "Copy WKT" button uses `conservative_wkt ?? wkt` — the conservative (UB-trimmed)
  path when offsets are present, otherwise the midpoint-trimmed path.
  The "Copy JSON" button copies the full `decodeResult` (including partial trace on
  failure) and is enabled whenever `decodeResult` is non-null.

### `SegBtn`

Clickable badge that calls `setTraceHighlight([segId])`. The `e.stopPropagation()`
call is required to prevent the section collapse from intercepting the click.
`setTraceHighlight` in the store sets `traceHighlightSegIds`, which triggers the
trace highlight effect in `Map.jsx`.

### Direct-match detection

```js
const isDirect = !start && dnp?.actual_m === 0;
```

When both LRPs of a leg project onto the same segment, the engine emits a `RouteFound`
with an empty path and DNP = 0, but no `RouteSearchStarted`. The `isDirect` condition
handles this: `!start` means no `RouteSearchStarted` was emitted; `dnp?.actual_m === 0`
confirms the zero-length direct match. The segment IDs come from the top accepted
candidates for the two surrounding LRPs.

---

## 11. `useDraggable` hook (`hooks.js`)

Makes a panel draggable by its header element.

```js
const { pos, onMouseDown } = useDraggable(panelRef);
// pos = null (use CSS defaults) | { left, top } (panel has been dragged)
// onMouseDown = attach to the drag handle element's onMouseDown prop
```

Internally: `onMouseDown` records the initial panel rect and mouse position into
`dragState.current`; document-level `mousemove`/`mouseup` listeners (added in a
`useEffect`) drive `setPos()` and clean up on mouse-up. The listeners are added
once on mount and removed on unmount.

---

## 12. DNP display in TracePanel

The DNP row clamps the lower bound to zero before display:

```jsx
DNP {fmtM(dnp.actual_m)} ∈ [{fmtM(Math.max(0, dnp.interval?.lb ?? 0))}, {fmtM(dnp.interval?.ub)}]
```

This prevents a visual `-29.3 m` lower bound for v3 bucket 0 (lb = 0, delta applied
symmetrically, but the semantically valid lower bound cannot be negative). The engine's
`validate_dnp` uses `window = dnp.widen(delta)` where `delta = path_length_m ×
dnp_tolerance_pct`; for a zero-length path, delta = 0 and the window is exactly
`[0.0, 58.6]`, so clamping is not needed in that case — the clamping is only a display
guard for intermediate trace events where the interval might differ.

---

## 13. Decode replay system

### Overview

After a decode the `DecodeTrace.events` array is converted into a visual step-by-step replay. Two new files implement this:

- **`replayEngine.js`** — pure transformation logic (no MapLibre dependency)
- **`ReplayPanel.jsx`** — the panel UI: ◀ / ▶ buttons, step counter, scrubable timeline

The trace engine emits events; the replay engine converts them into *display steps* and accumulates a mutable *visual state* that `Map.jsx` maps to GeoJSON sources.

### `replayEngine.js`

**`buildReplaySteps(events)`** converts the flat `events` array into a `{ steps, stats }` pair:
- One step per `CandidateSearchStarted`, `CandidatesRanked`, `RouteSearchStarted`, and `RouteFound`/`RouteFailed`/`DnpChecked`/`OffsetApplied`/`DecodeComplete` event
- `AStarNodeExpanded` events are grouped into batches of `ASTAR_BATCH=1` (one node per display step)
- `CandidateEvaluated` and `AStarEdgeSkipped` events are discarded (summary data only)
- `stats.maxG` normalises A\* node colours; `stats.phases` drives the timeline colour strips

**`emptyState()` / `applyStep(state, step, maxG)`** — incrementally mutate the visual state object. `applyStep` is O(1) per call; forward stepping is O(1) total. Backward jumps fall back to `computeVisualState` (O(N) full replay from step 0).

**Visual state fields:**

| Field | Purpose |
|---|---|
| `searchRadius` | Current LRP search circle: `{ lon, lat, radiusM, lrpIdx }` |
| `candidates` | All candidate snap points; each carries full projection + score detail |
| `astarNodes` | Accumulated A\* expanded nodes; each has a pre-computed `color` |
| `frontier` | Last 25 A\* nodes (for the pulsing frontier layer) |
| `currentLeg` | Active leg: `{ leg, fromPt, toPt, fromSegId, toSegId }` |
| `routeSegIds` | Segment IDs of the most recently found route |

**Candidate `winner` flag**: set to `true` on `route_search_started` for the candidates whose `segment_id` matches `step.from.segment_id` or `step.to.segment_id`. Winners render with a white ring and larger radius in the `replay-candidates-circle` layer.

**`stateToGeoJSON(state)`** converts visual state to `{ radiusFC, candFC, cloudFC, frontierFC, legFC }`. Route geometry (`replay-route`) is built separately in `Map.jsx` using `getSegGeomCache()` / `getSegIdToTile()` / `getTileGeomCache()` (the same two-step fallback as the trace highlight effect).

### Candidate GeoJSON properties

All candidate detail is flattened into GeoJSON feature properties so the click popup can read them without additional lookups:

| Property | Accepted | Rejected |
|---|---|---|
| `ctype` | `'accepted'` | `'bearing'` / `'radius'` / `'score'` / `'direction'` / `'other'` |
| `winner` | true if chosen as leg endpoint | false |
| `segment_id`, `traversal` | ✓ | if available |
| `distance_m`, `arc_offset_m`, `bearing_deg` | ✓ | bearing/distance if available |
| `score_total`, `score_distance`, `score_bearing`, `score_frc`, `score_fow`, `score_wrong_ep`, `score_interior` | ✓ | — |
| `verdict_json` | — | JSON-stringified `GateVerdict` |

### `ReplayPanel.jsx`

Bottom-anchored panel containing:
- **◀ / ▶ step buttons** (also ← / → arrow keys)
- **Step counter**: `N / total · X A* nodes` (A\* badge hidden if 0, which means Summary trace level)
- **Status line**: human-readable description of the current step
- **Full-width trace hint bar**: shown when `totalNodes === 0`, prompting the user to set Trace level → Full
- **Scrubable timeline**: click/drag to jump to any step; colour strips from `replayStats.phases` mark LRP and leg phases

### Map auto-pan behaviour during replay

| Step type | Action |
|---|---|
| `search_started` | `map.flyTo()` to LRP at zoom ≥ 15 |
| `route_search_started` | `map.fitBounds()` between from/to leg endpoints |
| `astar_batch` | `map.jumpTo()` to latest node at zoom 17 (instant, stays in sync with 30 ms playback) |
| `route_found` | `map.fitBounds()` to the full route extent; gold route line pulses for 3 s |

### Candidate click popup

Clicking any candidate dot opens a draggable popup (`cand-panel` CSS class) showing:
- **Accepted**: projection (distance from LRP, arc offset, bearing) + full 7-component score breakdown
- **Rejected**: human-readable gate failure reason (e.g. "Bearing gate — exceeded by 143.97°") + available projection fields
- **Chosen** candidates (winners) show a gold ★ badge in the header

### A\* node colour ramp

`nodeColorAt(t)` maps `t = g_m / maxG ∈ [0, 1]` to a CSS hex colour:
- `t < 0.33`: blue → cyan
- `0.33 ≤ t < 0.66`: cyan → yellow
- `t ≥ 0.66`: yellow → red

### Incremental state update (performance)

`Map.jsx` keeps `replayVisualRef` (last visual state) and `replayStepRef` (its step index). On each render:
- **Forward step**: calls `applyStep` only for new steps — O(1) per step
- **Backward / scrub**: calls `computeVisualState` from step 0 — O(N), acceptable for scrubbing

This avoids the O(N²) cost of recomputing from step 0 on every step during forward playback with thousands of A\* nodes.

---

## 14. File map

| File | Contents |
|---|---|
| `src/main.jsx` | React root mount; `<StrictMode>` wrapper |
| `src/App.jsx` | Startup, WASM init, tile base URL, component tree |
| `src/App.css` | All styles (TopBar, panels, map overlays, trace panel, replay panel) |
| `src/store.js` | Zustand store; 3 module-level caches; `runDecode()`; PRESETS; replay state |
| `src/replayEngine.js` | `buildReplaySteps`, `applyStep`, `emptyState`, `computeVisualState`, `stateToGeoJSON` |
| `src/tileDecoder.js` | OLRL v2 binary → GeoJSON |
| `src/wasm.js` | WASM module loader (`initWasm()`) |
| `src/hooks.js` | `useDraggable` |
| `src/components/Map.jsx` | MapLibre GL JS; all sources/layers; tile loader; highlight + replay effects |
| `src/components/TopBar.jsx` | Input bar, gear menu, Trace level, ▶ Replay button, Decode button |
| `src/components/ParamsPanel.jsx` | DecodeParams editor; FRC/FOW penalty tables |
| `src/components/ResultPanel.jsx` | Decoded segment list; click-to-highlight |
| `src/components/TracePanel.jsx` | Full decode trace view |
| `src/components/ReplayPanel.jsx` | Step replay UI: ◀/▶ buttons, step counter, scrubable timeline |
| `vite.config.js` | Dev server; serve-tiles plugin (HTTP 206 / range support) |

---

## 15. Candidate colour key

| Dot colour | `ctype` value | Meaning |
|---|---|---|
| Green (larger, white ring) | `accepted`, `winner: true` | Accepted and chosen as leg endpoint |
| Green | `accepted`, `winner: false` | Accepted but beaten by another candidate's route cost |
| Orange | `bearing` | Bearing outside `[LB−τ, UB+τ]` gate |
| Yellow | `radius` | Outside candidate search radius |
| Purple | `score` | Total score exceeded `max_candidate_score` |
| Dark grey | `direction` | Wrong direction (one-way road) |
| Light grey | `other` | Other / degenerate geometry |

A green dot overlaid with a smaller orange dot at the same location is a bidirectional road where one traversal direction was accepted and the opposite direction failed the bearing gate.

---

## 16. Known issues / next steps

- **No offline fallback for missing tiles**: if a PMTiles archive is unreachable,
  the decode fails with a generic error. A clear "tile not found" message would help.

- **Segment layer flickers on pan**: `rebuildSource()` replaces the entire GeoJSON
  feature collection on every move. For large tile caches this causes a noticeable
  repaint. A diff-and-patch approach or switching to a tile-protocol source would fix it.

- **TracePanel A\* table capped at 200 rows**: full A\* data is available in the JSON
  via "Copy JSON" but not browseable in the UI for large expansions.

- **Popup position for trace highlights**: when a trace highlight hits multiple segments
  the popup is suppressed. For multi-segment path highlights a summary popup (total
  length, FRC range) would be useful.

- **Rejected candidate `SegBtn` clicks**: clicking a rejected candidate's segment ID
  in `RejectedTable` highlights the segment on the map. However, rejected candidates
  may be outside the loaded tile region (the tile wasn't fetched because the segment
  was beyond the search radius). In that case the highlight silently no-ops.

- **Replay route geometry fallback**: the `replay-route` source uses the same two-step
  segment geometry cache as the trace highlight. If a route segment ID is not in either
  cache (e.g. the tile was not loaded), that segment is silently omitted from the gold
  route line. The route will still show partially in most cases.
