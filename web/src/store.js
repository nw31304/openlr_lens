import { create } from 'zustand';
import { persist } from 'zustand/middleware';
import { decodeTile } from './tileDecoder.js';
import { buildReplaySteps } from './replayEngine.js';
import { loadLlmConfig, saveLlmConfig, clearLlmConfig as clearLlmStorage, chatComplete } from './llmClient.js';
import { buildSystemContext } from './llmDiagnosis.js';
import { TOOL_DEFINITIONS, executeTool } from './llm/tools.js';

let _pmtiles = null;
let _decoder = null;
let _zoom = 12;
/** segment_id → { tile_key, local_index } — rebuilt after every decode */
let _segIdToTile = new Map();
/** tile_key → GeoJSON features[] — built from tile bytes during decode */
let _tileGeomCache = new Map();
/** segment_id → GeoJSON feature — direct lookup, built from the two caches above */
let _segGeomCache = new Map();

export function setPmtiles(p) { _pmtiles = p; }
export function setDecoder(d) { _decoder = d; }
export function setZoom(z)    { _zoom = z; }
export function getSegIdToTile()   { return _segIdToTile; }
export function getTileGeomCache() { return _tileGeomCache; }
export function getSegGeomCache()  { return _segGeomCache; }

/** Look up the internal graph segment ID by tile + local index.  Returns -1 when
 *  the tile hasn't been loaded by the decoder yet (e.g. before the first decode). */
export function getSegmentId(z, x, y, localIndex) {
  if (!_decoder) return -1;
  return _decoder.segment_id_at(z, x, y, localIndex);
}

function defaultFrcTable() {
  const p = [0.00, 0.10, 0.25, 0.45, 0.65, 0.80, 0.90, 1.00];
  return Array.from({ length: 8 }, (_, i) =>
    Array.from({ length: 8 }, (_, j) => p[Math.abs(i - j)])
  );
}

const DEFAULT_FOW_TABLE = [
  [0.00, 0.30, 0.30, 0.30, 0.30, 0.30, 0.30, 0.30],
  [0.30, 0.00, 0.10, 0.40, 0.60, 0.70, 0.20, 0.80],
  [0.30, 0.10, 0.00, 0.20, 0.40, 0.50, 0.25, 0.70],
  [0.30, 0.40, 0.20, 0.00, 0.20, 0.25, 0.30, 0.40],
  [0.30, 0.60, 0.40, 0.20, 0.00, 0.30, 0.40, 0.50],
  [0.30, 0.70, 0.50, 0.25, 0.30, 0.00, 0.50, 0.40],
  [0.30, 0.20, 0.25, 0.30, 0.40, 0.50, 0.00, 0.50],
  [0.30, 0.80, 0.70, 0.40, 0.50, 0.40, 0.50, 0.00],
];

export const PRESETS = {
  Permissive: {
    candidate_search_radius_m:    200.0,
    snap_to_endpoint_threshold_m:  25.0,
    distance_weight:                0.5,
    bearing_weight:                 0.2,
    bearing_penalty_per_bucket:     0.03,
    frc_weight:                     0.05,
    fow_weight:                     0.10,
    interior_weight:                0.05,
    wrong_endpoint_weight:          0.10,
    frc_penalty_table: defaultFrcTable(),
    fow_penalty_table: DEFAULT_FOW_TABLE,
    max_bearing_deviation_deg:     90.0,
    max_candidate_score:            1.5,
    max_candidates_per_lrp:        10,
    dnp_tolerance_pct:              0.40,
    max_path_search_factor:         4.0,
    max_astar_expansions:       50000,
    lfrcnp_tolerance:               2,
    max_routing_attempts:           0,
    trace_level: 'Summary',
  },
  Default: {
    candidate_search_radius_m:     30.0,
    snap_to_endpoint_threshold_m:  15.0,
    distance_weight:                0.5,
    bearing_weight:                 0.3,
    bearing_penalty_per_bucket:     0.05,
    frc_weight:                     0.10,
    fow_weight:                     0.20,
    interior_weight:                0.10,
    wrong_endpoint_weight:          5.00,
    frc_penalty_table: defaultFrcTable(),
    fow_penalty_table: DEFAULT_FOW_TABLE,
    max_bearing_deviation_deg:     45.0,
    max_candidate_score:            1.5,
    max_candidates_per_lrp:         8,
    dnp_tolerance_pct:              0.25,
    max_path_search_factor:         5.0,
    max_astar_expansions:      100000,
    lfrcnp_tolerance:               2,
    max_routing_attempts:          10,
    trace_level: 'Summary',
  },
  Strict: {
    candidate_search_radius_m:     50.0,
    snap_to_endpoint_threshold_m:  10.0,
    distance_weight:                0.5,
    bearing_weight:                 0.4,
    bearing_penalty_per_bucket:     0.08,
    frc_weight:                     0.20,
    fow_weight:                     0.30,
    interior_weight:                0.20,
    wrong_endpoint_weight:          0.30,
    frc_penalty_table: defaultFrcTable(),
    fow_penalty_table: DEFAULT_FOW_TABLE,
    max_bearing_deviation_deg:     30.0,
    max_candidate_score:            1.0,
    max_candidates_per_lrp:         5,
    dnp_tolerance_pct:              0.10,
    max_path_search_factor:         3.0,
    max_astar_expansions:           0,
    lfrcnp_tolerance:               0,
    max_routing_attempts:           5,
    trace_level: 'Summary',
  },
};

// Produce a human-readable label for a tool call, incorporating key arguments.
function toolCallLabel(name, args) {
  switch (name) {
    case 'get_lrp_candidates': return `get_lrp_candidates(${args.lrp_index ?? '?'})`;
    case 'get_leg_summary':    return `get_leg_summary(${args.leg_index ?? '?'})`;
    case 'get_route_segments': return `get_route_segments(${args.leg_index ?? '?'})`;
    default: return name;
  }
}

// Summarise an array of tool call records into the shape stored in llmLastToolActivity.
function buildToolActivity(calls) {
  return {
    calls,
    total_result_bytes: calls.reduce((s, c) => s + c.result_bytes, 0),
  };
}

export const useStore = create(persist(
 (set, get) => ({
  openlrString: '',
  tileUrl: 'http://localhost:5176',
  params: { ...PRESETS.Default },
  showParams: false,
  showLlmSettings: false,
  showTrace: false,
  showResult: false,
  showReplay: false,
  llmConfig: loadLlmConfig(),
  llmChatOpen: false,
  llmMessages: [],       // display: { role, content, display?, error? }
  llmApiHistory: [],     // api: full history including tool call/result turns (not shown in UI)
  llmLastToolActivity: null, // { calls: [{label, result_bytes}], total_bytes } for last exchange
  llmLoading: false,
  llmStreamingContent: null, // string while final response is streaming, null otherwise
  showSegmentLayer: false,
  decoding: false,
  decodeResult: null,
  decodeToast: null,         // { message: string } | null; set on failure, cleared by component
  forcedDecoding: false,
  forcedDecodeResult: null,   // result from decode_forced(), null until user runs it
  pinnedCandidates: {},       // { [lrpIdx]: { segment_id, traversal, arc_offset_m, snap_lon, snap_lat } | null }
  savedParamSets: {},      // { [name: string]: DecodeParams }
  highlightedSegment: null,
  traceHighlightSegIds: null,
  traceHighlightSnaps: null,   // { from: [lon,lat], to: [lon,lat] } when highlighting a leg route
  traceLrpFocus: null,
  mapFlyTo: null,           // { lat, lon, zoom, _tick } — consumed by Map to call map.flyTo()
  candidatePopup: null,
  // ── Replay state ─────────────────────────────────────────────────────────
  replaySteps: [],        // pre-built display steps from buildReplaySteps()
  replayStats: null,      // { maxG, totalNodes, phases }
  replayStep: 0,          // current display step index

  setOpenlrString: (s) => set({ openlrString: s }),
  setTileUrl: (url) => set({ tileUrl: url }),

  resetToDefaults: () => set({ params: { ...PRESETS.Default } }),

  loadPreset: (name) => set({ params: { ...PRESETS[name] } }),

  saveParamSet: (name, params) => set(state => ({
    savedParamSets: { ...state.savedParamSets, [name]: { ...params } },
  })),
  deleteParamSet: (name) => set(state => {
    const next = { ...state.savedParamSets };
    delete next[name];
    return { savedParamSets: next };
  }),
  loadParamSet: (name) => set(state => ({
    params: { ...state.savedParamSets[name] },
  })),

  setParam: (key, value) => set(state => ({
    params: { ...state.params, [key]: value },
  })),

  setTraceLevel: (level) => set(state => ({
    params: { ...state.params, trace_level: level },
  })),

  setTableCell: (tableKey, row, col, value) => set(state => {
    const table = state.params[tableKey].map(r => [...r]);
    table[row][col] = value;
    return { params: { ...state.params, [tableKey]: table } };
  }),

  toggleParams:        () => set(state => ({ showParams:        !state.showParams })),
  toggleLlmSettings:   () => set(state => ({ showLlmSettings:   !state.showLlmSettings })),

  setLlmConfig: (config) => { saveLlmConfig(config); set({ llmConfig: config }); },
  clearLlmConfig: () => { clearLlmStorage(); set({ llmConfig: null }); },

  toggleLlmChat: () => set(s => ({ llmChatOpen: !s.llmChatOpen })),
  clearLlmChat:  () => set({ llmMessages: [], llmApiHistory: [], llmLastToolActivity: null, llmLoading: false, llmStreamingContent: null }),

  // content = text sent to the API (may include appended format hints)
  // display = text shown in the chat bubble (the user's original words)
  sendLlmMessage: async (content, display) => {
    const { llmMessages, llmApiHistory, decodeResult, params, llmConfig } = get();
    if (!llmConfig || !decodeResult) return;

    const userDisplayMsg = { role: 'user', content, display: display ?? content };
    set({ llmMessages: [...llmMessages, userDisplayMsg], llmLastToolActivity: null, llmLoading: true });

    // Rebuild system context each turn so parameter changes are reflected immediately
    const systemContext = buildSystemContext(decodeResult, params);

    // Cap history at 20 entries (~10 exchange pairs) to bound context window growth.
    // The model can re-call tools if it needs data that has aged out.
    const MAX_API_HISTORY = 20;
    const rawHistory = llmApiHistory.slice(-MAX_API_HISTORY);
    // Trim to a clean conversation boundary: never start with an orphaned tool result.
    // A clean start is a plain user message (string content, not a tool-result array).
    let trimStart = 0;
    for (let i = 0; i < rawHistory.length; i++) {
      const m = rawHistory[i];
      if (m.role === 'user' && typeof m.content === 'string') {
        trimStart = i;
        break;
      }
    }
    const trimmedHistory = rawHistory.slice(trimStart);

    // apiHistory is the full multi-turn API conversation (includes tool call/result turns)
    let apiHistory = [
      { role: 'system', content: systemContext },
      ...trimmedHistory,
      { role: 'user', content },
    ];

    // Track new entries added this turn so we can persist them after the loop
    const newApiEntries = [{ role: 'user', content }];
    // Accumulate tool call activity for the strip display
    const toolCalls = [];

    // onDelta streams text into llmStreamingContent as the final response arrives.
    // Tool-call steps may also stream text (brief preamble) which gets cleared when
    // tools are detected.
    const onDelta = (chunk) => {
      set(s => ({ llmStreamingContent: (s.llmStreamingContent ?? '') + chunk }));
    };

    const MAX_STEPS = 20;
    for (let step = 0; step < MAX_STEPS; step++) {
      set({ llmStreamingContent: null }); // typing dots until first text chunk
      const resp = await chatComplete(llmConfig, apiHistory, TOOL_DEFINITIONS, onDelta);

      if (!resp.ok) {
        set(s => ({
          llmMessages: [...s.llmMessages, { role: 'assistant', content: resp.error ?? 'Unknown error', error: true }],
          llmLastToolActivity: toolCalls.length ? buildToolActivity(toolCalls) : null,
          llmLoading: false,
          llmStreamingContent: null,
        }));
        return;
      }

      if (!resp.tool_calls?.length) {
        // Final text response — streaming content is already displayed; commit to history
        const assistantMsg = { role: 'assistant', content: resp.content ?? '' };
        const finalApiEntry = { role: 'assistant', content: resp.content ?? '' };
        set(s => ({
          llmMessages: [...s.llmMessages, assistantMsg],
          llmApiHistory: [...llmApiHistory, ...newApiEntries, finalApiEntry],
          llmLastToolActivity: toolCalls.length ? buildToolActivity(toolCalls) : null,
          llmLoading: false,
          llmStreamingContent: null,
        }));
        return;
      }

      // Tool-call step: clear any streamed preamble text; pre-populate strip
      // with pending calls so the user sees them before execution starts
      const pendingCalls = resp.tool_calls.map(tc => {
        let args = {};
        try { args = JSON.parse(tc.function.arguments); } catch {}
        return { label: toolCallLabel(tc.function.name, args), args_bytes: tc.function.arguments.length, result_bytes: 0, pending: true };
      });
      set({ llmStreamingContent: null, llmLastToolActivity: buildToolActivity([...toolCalls, ...pendingCalls]) });

      // Tool-use round: add assistant tool-call message to history and execute each tool
      const assistantApiEntry = {
        role: 'assistant',
        content: resp.content ?? null,
        tool_calls: resp.tool_calls,
      };
      newApiEntries.push(assistantApiEntry);
      apiHistory = [...apiHistory, assistantApiEntry];

      const storeActions = {
        setPinnedCandidates: (snapsArray) => {
          get().clearPinnedCandidates();
          snapsArray.forEach(({ lrp_index, ...snap }) => get().setPinnedCandidate(lrp_index, snap));
        },
        runForcedDecodeAndGet: async () => {
          await get().runForcedDecode();
          return get().forcedDecodeResult;
        },
        highlightSegments: (segIds) => get().setTraceHighlight(segIds),
        flyTo: (lat, lon, zoom) => get().setMapFlyTo(lat, lon, zoom),
      };

      for (const tc of resp.tool_calls) {
        let toolResult;
        try {
          const args = JSON.parse(tc.function.arguments);
          const forcedDecodeResult = get().forcedDecodeResult;
          toolResult = await executeTool(tc.function.name, args, { decodeResult, params, decoder: _decoder, storeActions, forcedDecodeResult });
          toolCalls.push({
            label: toolCallLabel(tc.function.name, args),
            args_bytes: tc.function.arguments.length,
            result_bytes: toolResult.length,
          });
        } catch (e) {
          toolResult = JSON.stringify({ error: e.message });
          toolCalls.push({ label: tc.function.name, args_bytes: 0, result_bytes: toolResult.length });
        }
        // Update strip in real-time so each tool appears as it completes
        set({ llmLastToolActivity: buildToolActivity(toolCalls) });
        const toolApiEntry = { role: 'tool', tool_call_id: tc.id, content: toolResult };
        newApiEntries.push(toolApiEntry);
        apiHistory = [...apiHistory, toolApiEntry];
      }
    }

    // Reached max steps without a final answer
    set(s => ({
      llmMessages: [...s.llmMessages, { role: 'assistant', content: '[Max tool call steps reached without a final response]', error: true }],
      llmLastToolActivity: toolCalls.length ? buildToolActivity(toolCalls) : null,
      llmLoading: false,
      llmStreamingContent: null,
    }));
  },
  toggleTrace:         () => set(state => ({ showTrace:         !state.showTrace })),
  toggleReplay:        () => set(state => ({ showReplay:        !state.showReplay })),
  toggleSegmentLayer:  () => set(state => ({ showSegmentLayer:  !state.showSegmentLayer })),

  setReplayStep:  (n)  => set(state => ({ replayStep: Math.max(0, Math.min(n, state.replaySteps.length - 1)) })),
  stepReplay: (delta) => set(state => ({
    replayStep: Math.max(0, Math.min(state.replayStep + delta, state.replaySteps.length - 1)),
  })),

  // Re-decode at an elevated trace level and open the trace panel.
  // Off → Summary on first call; Summary or Full → Full on subsequent calls.
  debugDecode: async () => {
    const { params } = get();
    const current = params.trace_level ?? 'Summary';
    const elevated = current === 'Off' ? 'Summary' : 'Full';
    set(state => ({ params: { ...state.params, trace_level: elevated }, showTrace: true }));
    await get().runDecode();
  },

  hideResult:    () => set({ showResult: false }),
  toggleResult:  () => set(state => ({ showResult: !state.showResult })),
  clearDecodeToast: () => set({ decodeToast: null }),
  clearResult: () => set({ decodeResult: null, showResult: false, highlightedSegment: null, traceHighlightSegIds: null, traceHighlightSnaps: null, traceLrpFocus: null, candidatePopup: null, llmApiHistory: [] }),
  setHighlightedSegment: (seg) => set({ highlightedSegment: seg }),
  // Request the segment info popup to open for a given tile+local_index.
  // Map.jsx watches this and opens the popup; call clearRequestedInfoSegment() after handling.
  requestedInfoSegment: null,
  requestInfoSegment:      (tile, local_index) => set({ requestedInfoSegment: { tile, local_index } }),
  clearRequestedInfoSegment: () => set({ requestedInfoSegment: null }),
  setTraceHighlight: (ids, snaps) => set({ traceHighlightSegIds: ids?.length ? ids : null, traceHighlightSnaps: snaps ?? null }),
  setCandidatePopup: (data) => set({ candidatePopup: data }),
  clearCandidatePopup: () => set({ candidatePopup: null }),
  setTraceLrpFocus: (lrp) => set({ traceLrpFocus: lrp ? { ...lrp, _tick: Date.now() } : null }),
  setMapFlyTo: (lat, lon, zoom) => set({ mapFlyTo: { lat, lon, zoom, _tick: Date.now() } }),

  setPinnedCandidate: (lrpIdx, snap) => set(state => ({
    pinnedCandidates: { ...state.pinnedCandidates, [lrpIdx]: snap ?? null },
    forcedDecodeResult: null,  // invalidate when pins change
  })),

  clearPinnedCandidates: () => set({ pinnedCandidates: {}, forcedDecodeResult: null }),

  runForcedDecode: async () => {
    const { decodeResult, pinnedCandidates, params } = get();
    if (!_decoder || !decodeResult) return;

    const lrpCount = decodeResult.lrps?.length ?? 0;
    const snaps = Array.from({ length: lrpCount }, (_, i) => pinnedCandidates[i]);
    if (snaps.some(s => !s)) return;  // not all LRPs pinned

    set({ forcedDecoding: true, forcedDecodeResult: null });
    try {
      const snapsJson = JSON.stringify(snaps);
      const attemptedTiles = new Set();
      const MAX_DYNAMIC_LOADS = 10;
      let result = null;
      for (let attempt = 0; attempt <= MAX_DYNAMIC_LOADS; attempt++) {
        result = JSON.parse(_decoder.decode_forced(snapsJson));
        if (!result.needs_tile) break;
        const [z, x, y] = result.needs_tile;
        const tileKey = `${z}/${x}/${y}`;
        if (attemptedTiles.has(tileKey)) break;
        attemptedTiles.add(tileKey);
        try {
          const res = await _pmtiles.getZxy(z, x, y);
          if (res?.data) {
            _decoder.load_tile(z, x, y, new Uint8Array(res.data));
          } else {
            _decoder.load_tile(z, x, y, new Uint8Array(0));
          }
        } catch (e) {
          console.warn(`[forced-decode] tile ${tileKey} load failed:`, e?.message ?? e);
          break;
        }
      }
      // Enrich segments with source_id from tile geometry cache
      for (const seg of result.segments ?? []) {
        const feat = _segGeomCache.get(seg.segment_id);
        if (feat) seg.source_id = feat.properties.source_id ?? null;
      }

      // Splice replay: candidate events from original trace, routing events from forced trace.
      const originalEvents = get().decodeResult?.trace?.events ?? [];
      const forcedEvents   = result.trace?.events ?? [];
      const firstRouteOrig  = originalEvents.findIndex(e => e.RouteSearchStarted != null);
      const firstRouteForced = forcedEvents.findIndex(e => e.RouteSearchStarted != null);
      const candidateEvents = firstRouteOrig  >= 0 ? originalEvents.slice(0, firstRouteOrig)  : originalEvents;
      const routingEvents   = firstRouteForced >= 0 ? forcedEvents.slice(firstRouteForced)      : [];
      const splicedEvents   = [...candidateEvents, ...routingEvents];
      const replayData = splicedEvents.length
        ? buildReplaySteps(splicedEvents)
        : { steps: [], stats: { maxG: 0, totalNodes: 0, phases: [] } };

      set({
        forcedDecoding: false,
        forcedDecodeResult: result,
        replaySteps: replayData.steps,
        replayStats: replayData.stats,
        replayStep:  0,
      });
    } catch (e) {
      set({ forcedDecoding: false, forcedDecodeResult: { ok: false, error: String(e), segments: [] } });
    }
  },

  runDecode: async () => {
    const { openlrString, params } = get();
    if (!openlrString.trim() || !_pmtiles || !_decoder) return;

    set(state => ({
      decoding: true,
      decodeResult: null,
      // Reset transient UI state for the new decode; preserve showResult so an
      // open panel stays open rather than collapsing and re-expanding (flicker).
      showTrace: false,
      showReplay: false,
      showSegmentLayer: false,
      highlightedSegment: null,
      traceHighlightSegIds: null,
      traceHighlightSnaps: null,
      traceLrpFocus: null,
      candidatePopup: null,
      replaySteps: [],
      replayStats: null,
      replayStep: 0,
      pinnedCandidates: {},
      forcedDecodeResult: null,
      llmMessages: [],
      llmApiHistory: [],
      llmLoading: false,
    }));
    _tileGeomCache = new Map();
    _segIdToTile   = new Map();
    _segGeomCache  = new Map();
    // Hoisted so the catch block can inspect it even if an exception occurs mid-processing.
    let result = null;
    try {
      const t0 = performance.now();
      _decoder.reset_tiles();
      const paramsJson = JSON.stringify(params);
      console.log('[params] fow_weight:', params.fow_weight, 'frc_weight:', params.frc_weight,
        'fow[3][7]:', params.fow_penalty_table[3][7], 'fow[7][3]:', params.fow_penalty_table[7][3],
        'lfrcnp_tolerance:', params.lfrcnp_tolerance);
      const startResult = JSON.parse(_decoder.start(openlrString.trim(), paramsJson, _zoom));
      console.log(`[timing] start(): ${(performance.now()-t0).toFixed(1)} ms`);

      console.log('[decode] requested tiles:', startResult.tiles.map(([z,x,y]) => `${z}/${x}/${y}`));
      let loadedTiles = 0;
      let wasmLoadMs = 0;
      let jsDecodeMs = 0;
      const tFetch0 = performance.now();
      await Promise.all(startResult.tiles.map(async ([z, x, y]) => {
        try {
          const res = await _pmtiles.getZxy(z, x, y);
          if (res?.data) {
            const tWasm0 = performance.now();
            _decoder.load_tile(z, x, y, new Uint8Array(res.data));
            wasmLoadMs += performance.now() - tWasm0;
            loadedTiles++;
            const tileKey = `${z}/${x}/${y}`;
            const wasmCount = _decoder.tile_segment_count(z, x, y);
            const tJs0 = performance.now();
            // Cache tile geometry so the trace panel can pan/highlight decoded segments
            _tileGeomCache.set(tileKey, decodeTile(res.data, z, x, y).features);
            jsDecodeMs += performance.now() - tJs0;
            console.log(`[tile] loaded ${tileKey} (${res.data.byteLength} bytes, ${wasmCount} segs in WASM)`);
          } else {
            console.warn(`[tile] no data for ${z}/${x}/${y} (tile not in archive)`);
          }
        } catch (e) {
          console.warn(`[tile] ${z}/${x}/${y} load failed:`, e?.message ?? e);
        }
      }));
      console.log(`[timing] tile fetch+load total: ${(performance.now()-tFetch0).toFixed(1)} ms  (WASM load_tile: ${wasmLoadMs.toFixed(1)} ms, JS decodeTile: ${jsDecodeMs.toFixed(1)} ms)`);

      const segs = _decoder.loaded_segment_count();
      console.log(`[decode] tiles requested=${startResult.tiles.length} loaded=${loadedTiles} segments=${segs}`);

      // Run decode, loading any tiles A* discovers it needs along the way.
      // Each call either returns a result (ok or error) or a { needs_tile: [z,x,y] }
      // signal.  We cap retries to prevent runaway in degenerate cases.
      const attemptedTiles = new Set(startResult.tiles.map(([z,x,y]) => `${z}/${x}/${y}`));
      const MAX_DYNAMIC_LOADS = 20;
      for (let attempt = 0; attempt <= MAX_DYNAMIC_LOADS; attempt++) {
        const tDecode0 = performance.now();
        result = JSON.parse(_decoder.decode());
        console.log(`[timing] decode() attempt ${attempt}: ${(performance.now()-tDecode0).toFixed(1)} ms`);
        if (!result.needs_tile) {
          console.log(
            `[decode-result] ok=${result.ok} format="${result.format ?? '(absent)'}"` +
            ` lrps=${result.lrps == null ? 'ABSENT' : result.lrps.length}` +
            ` trace=${result.trace == null ? 'ABSENT' : ('events=' + (result.trace.events?.length ?? '?'))}` +
            ` error="${result.error ?? ''}"`
          );
        }

        if (!result.needs_tile) break;

        const [z, x, y] = result.needs_tile;
        const tileKey = `${z}/${x}/${y}`;

        if (attemptedTiles.has(tileKey)) {
          // Guard: same tile requested twice means the graph didn't register it as
          // loaded (shouldn't happen, but prevents an infinite loop).
          console.warn(`[tile] A* re-requested ${tileKey} — already attempted, stopping`);
          break;
        }
        attemptedTiles.add(tileKey);
        console.log(`[tile] A* needs ${tileKey} (dynamic load, attempt ${attempt + 1})`);

        try {
          const res = await _pmtiles.getZxy(z, x, y);
          if (res?.data) {
            _decoder.load_tile(z, x, y, new Uint8Array(res.data));
            _tileGeomCache.set(tileKey, decodeTile(res.data, z, x, y).features);
            console.log(`[tile] dynamic loaded ${tileKey} (${res.data.byteLength} bytes)`);
          } else {
            // Tile not in archive — mark as loaded (empty) so A* stops requesting it.
            _decoder.load_tile(z, x, y, new Uint8Array(0));
            console.warn(`[tile] dynamic ${tileKey}: not in archive, marked empty`);
          }
        } catch (e) {
          console.warn(`[tile] dynamic ${tileKey} load failed:`, e?.message ?? e);
          break;
        }
      }

      // Build segment_id → tile + segment_id → feature maps.
      // Done after the dynamic-tile loop so all loaded tiles are included.
      // Pre-index each tile's features by local_index so the per-segment lookup is O(1)
      // rather than O(tile_size) — avoiding an O(N²) scan over 200k+ segments.
      const tIdx0 = performance.now();
      const tileFeatureIndex = new Map();
      for (const [tileKey, features] of _tileGeomCache) {
        const idx = new Map();
        for (const feat of features) idx.set(feat.properties.local_index, feat);
        tileFeatureIndex.set(tileKey, idx);
      }
      console.log(`[timing] tile feature index build: ${(performance.now()-tIdx0).toFixed(1)} ms`);

      const tMap0 = performance.now();
      const rawMappings = JSON.parse(_decoder.all_segment_tile_mappings());
      console.log(`[timing] all_segment_tile_mappings serialize+parse: ${(performance.now()-tMap0).toFixed(1)} ms`);

      const tCache0 = performance.now();
      for (const [segId, z, x, y, li] of rawMappings) {
        const tileKey = `${z}/${x}/${y}`;
        _segIdToTile.set(segId, { tile_key: tileKey, local_index: li });
        // O(1) lookup via pre-built index — was O(tile_size) with .find()
        const feat = tileFeatureIndex.get(tileKey)?.get(li);
        if (feat) _segGeomCache.set(segId, feat);
      }
      console.log(`[timing] segGeomCache build (${rawMappings.length} segs): ${(performance.now()-tCache0).toFixed(1)} ms`);
      console.log(`[segGeomCache] ${_segGeomCache.size}/${rawMappings.length} segments have geometry`);
      // Enrich decoded segments with source_id from the tile geometry cache.
      for (const seg of result.segments ?? []) {
        const feat = _segGeomCache.get(seg.segment_id);
        if (feat) seg.source_id = feat.properties.source_id ?? null;
      }
      console.log('[PATH] segments:', result.segments?.map(s => s.source_id));
      console.log('[LRPs]', result.lrps?.map((l, i) =>
        `LRP${i}: lon=${l.lon.toFixed(5)} lat=${l.lat.toFixed(5)}` +
        ` bear=[${l.bearing_lb.toFixed(2)},${l.bearing_ub.toFixed(2)}]` +
        ` frc=${l.frc} fow=${l.fow}` +
        (l.lfrcnp != null ? ` lfrcnp=${l.lfrcnp} (effective floor=${Math.min(l.lfrcnp + (params.lfrcnp_tolerance ?? 0), 7)})` : ' [last LRP]')
      ));
      if (result.trace?.events) {
        result.trace.events.filter(e => e.CandidatesRanked).forEach(e => {
          const r = e.CandidatesRanked;
          console.log(`[TRACE] LRP${r.lrp_idx} candidates (${r.accepted.length} accepted, ${r.rejected_count} rejected):`);
          r.accepted.forEach((c, i) => console.log(
            `  #${i} seg=${c.segment_id} ${c.traversal} arc=${c.projection.arc_offset_m.toFixed(1)}m` +
            ` dist=${c.projection.distance_m.toFixed(2)}m bear=${c.projection.bearing_deg.toFixed(1)}°` +
            ` score=${c.score.total.toFixed(4)} (dist=${c.score.distance_score.toFixed(4)}` +
            ` bear=${c.score.bearing_score.toFixed(4)} frc=${c.score.frc_score.toFixed(4)}` +
            ` fow=${c.score.fow_score.toFixed(4)} wrong_ep=${c.score.wrong_endpoint_score.toFixed(4)}` +
            ` int=${c.score.interior_score.toFixed(4)})`
          ));
        });
        const routes = result.trace.events.filter(e => e.RouteSearchStarted || e.DnpChecked);
        console.log('[TRACE] Routing events:', JSON.stringify(routes, null, 2));
        // Show A* termination stats — these confirm whether LFRCNP is biting
        result.trace.events.filter(e => e.AStarTerminated).forEach(e => {
          const t = e.AStarTerminated;
          console.log(
            `[TRACE] A* leg ${t.leg}: ${t.nodes_expanded} expansions, reason=${JSON.stringify(t.reason)}` +
            ` skipped: frc=${t.edges_skipped_frc} dir=${t.edges_skipped_direction}` +
            ` turn=${t.edges_skipped_turn} dist=${t.edges_skipped_distance}`
          );
        });
      }
      // Build replay steps from trace events (if any)
      const replayData = result.trace?.events?.length
        ? buildReplaySteps(result.trace.events)
        : { steps: [], stats: null };
      const toast = result.ok ? null : { message: result.error ?? 'Decode failed' };
      set({
        decoding: false,
        decodeResult: result,
        decodeToast: toast,
        replaySteps: replayData.steps,
        replayStats:  replayData.stats,
        replayStep:   0,
      });
    } catch (e) {
      const stage = result !== null ? 'post-decode JS' : 'pre-decode (start/tile-load)';
      console.error(`[decode] exception in runDecode at ${stage}:`, e);
      console.error('[decode] result at throw time:', result);
      // result.ok is a boolean iff WASM returned a proper DecodeResult.  Preserve it — it
      // carries lrps/trace we want to show.  The exception came from post-decode JS processing.
      if (result !== null && (result.ok === true || result.ok === false)) {
        const toast = result.ok ? null : { message: result.error ?? 'Decode failed' };
        set({ decoding: false, decodeResult: result, decodeToast: toast, replaySteps: [], replayStats: null, replayStep: 0 });
      } else {
        // WASM throws plain strings via JsValue::from_str; JS Error objects have .message.
        const errorMsg = e instanceof Error ? e.message : String(e);
        set({ decoding: false, decodeResult: { ok: false, error: errorMsg, segments: [] }, decodeToast: { message: errorMsg } });
      }
    }
  },
 }),
 {
   name: 'openlrlens-settings',
   partialize: (state) => ({
     openlrString: state.openlrString,
     tileUrl: state.tileUrl,
     params: state.params,
     savedParamSets: state.savedParamSets,
   }),
   // Deep-merge params so new fields added to PRESETS.Default survive across
   // localStorage upgrades — persisted values win, but missing fields fall back
   // to the current default rather than becoming undefined.
   merge: (persisted, current) => ({
     ...current,
     ...persisted,
     params: { ...current.params, ...persisted.params },
     savedParamSets: { ...(persisted.savedParamSets ?? {}) },
   }),
 }
));
