// Tool definitions (OpenAI function-calling format) and executor.
// llmClient.js converts these to Anthropic format when needed.

const FOW_LABELS = [
  'undefined', 'motorway', 'dual carriageway', 'single carriageway',
  'roundabout', 'traffic square', 'slip road', 'other',
];
const FRC_LABELS = [
  'FRC0 motorway', 'FRC1 trunk', 'FRC2 secondary', 'FRC3 tertiary',
  'FRC4 unclassified', 'FRC5 residential', 'FRC6 service', 'FRC7 other',
];

const FOW_LABELS_FULL = [
  'Form of Way undefined', 'Motorway', 'Multiple carriageway', 'Single carriageway',
  'Roundabout', 'Traffic square', 'Slip road', 'Other / non-vehicle',
];

export const TOOL_DEFINITIONS = [
  {
    type: 'function',
    function: {
      name: 'get_decode_summary',
      description:
        'Top-level decode outcome: success/failure, segment count, format, offset ranges, current decode parameters, and the full ordered path segment list with per-segment length_m, FRC, FOW, and direction. Also returns path_total_length_m — the sum of all segment lengths in the untrimmed decoded location. Call this first before any other tool.',
      parameters: { type: 'object', properties: {}, required: [] },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_parsed_reference',
      description:
        'Full parsed LRP chain: coordinates, bearing interval, FRC, FOW, LFRCNP, and DNP for each LRP.',
      parameters: { type: 'object', properties: {}, required: [] },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_lrp_candidates',
      description:
        'Ranked candidate segments for one LRP, with projection geometry and 6-term score breakdown. Set include_rejected=true to see rejection reasons.',
      parameters: {
        type: 'object',
        properties: {
          lrp_index: {
            type: 'integer',
            description: 'Zero-based LRP index.',
          },
          include_rejected: {
            type: 'boolean',
            description: 'Include rejected candidates with their rejection verdict. Default false.',
          },
        },
        required: ['lrp_index'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_leg_summary',
      description:
        'Routing outcome for one inter-LRP leg: whether a route was found, its length, A* expansion statistics (nodes expanded, edges skipped by reason), and DNP validation result.',
      parameters: {
        type: 'object',
        properties: {
          leg_index: {
            type: 'integer',
            description: 'Zero-based leg index (leg 0 = LRP 0 → LRP 1).',
          },
        },
        required: ['leg_index'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_route_segments',
      description:
        'Ordered segment list for a successfully routed leg, with per-segment length_m, FRC, FOW, and direction. Also returns segment_sum_m (sum of all segment lengths in this leg) and snap coordinates at each end.',
      parameters: {
        type: 'object',
        properties: {
          leg_index: {
            type: 'integer',
            description: 'Zero-based leg index.',
          },
        },
        required: ['leg_index'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_segment',
      description:
        'Full attributes and geometry for one segment by its internal segment ID. Returns FRC, FOW, direction, length, geometry, tile location, and source_key (the human-readable stable ID such as "372358612-1"). Use this to inspect any segment seen in candidate lists, path breakdowns, or rejection reasons.',
      parameters: {
        type: 'object',
        properties: {
          segment_id: {
            type: 'integer',
            description: 'Internal graph segment ID (as seen in candidate or path data).',
          },
        },
        required: ['segment_id'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_segments_near',
      description:
        'Find all loaded road segments within radius_m of a coordinate. Returns up to 50 segments sorted by distance, each with source_key (stable ID like "372358612-1"), FRC, FOW, direction, and length. Useful for understanding what roads are available near an LRP that produced no or few candidates.',
      parameters: {
        type: 'object',
        properties: {
          lat:      { type: 'number',  description: 'Latitude in decimal degrees.' },
          lon:      { type: 'number',  description: 'Longitude in decimal degrees.' },
          radius_m: { type: 'number',  description: 'Search radius in metres (max 500). Default 100.' },
        },
        required: ['lat', 'lon'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'get_segment_neighbors',
      description:
        'Returns all segments connected at each endpoint of a given segment. '
        + 'Reports two groups — at_start_node and at_end_node — each listing every other '
        + 'segment that shares that node, with can_arrive/can_depart flags and turn-restriction flags. '
        + 'For bidirectional (Both) segments each endpoint is simultaneously entry and exit, '
        + 'so both groups show full connectivity. '
        + 'Each neighbour includes source_key (the human-readable stable ID such as "372358612-1"), '
        + 'internal segment_id, FRC, FOW, direction, and length. '
        + 'Use this to understand junction topology, diagnose why A* took or avoided a turn, '
        + 'or explore the road network around a candidate segment.',
      parameters: {
        type: 'object',
        properties: {
          segment_id: {
            type: 'integer',
            description: 'Internal graph segment ID.',
          },
        },
        required: ['segment_id'],
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'retry_decode',
      description:
        'Re-run the decode with a partial parameter override merged over the current params. Returns ok/fail, segment count, and total path length so you can immediately compare with the original result. Tiles must already be loaded (always true after a normal decode). Example: {"max_bearing_deviation_deg": 30} to test a wider bearing window.',
      parameters: {
        type: 'object',
        properties: {
          params_override: {
            type: 'object',
            description: 'Partial DecodeParams as a JSON object — only the fields you want to change. All other params inherit from the current values.',
            additionalProperties: true,
          },
        },
        required: ['params_override'],
      },
    },
  },
];

// Extract all instances of one event variant from a trace event array.
// Trace events are serde "externally tagged" enums: { VariantName: { ...fields } }
function getTraceEvents(events, variant) {
  return (events ?? [])
    .filter(e => e[variant] !== undefined)
    .map(e => e[variant]);
}

// Execute a tool call.  Returns a JSON string ready to send back as a tool result.
export function executeTool(name, args, { decodeResult, params, decoder }) {
  if (!decodeResult) return JSON.stringify({ error: 'No decode result available.' });

  const events = decodeResult.trace?.events ?? [];

  switch (name) {
    case 'get_decode_summary': {
      const segs = decodeResult.segments ?? [];
      const totalLengthM = segs.reduce((sum, s) => sum + (s.length_m ?? 0), 0);
      return JSON.stringify({
        ok: decodeResult.ok,
        format: decodeResult.format ?? null,
        error: decodeResult.error ?? null,
        segment_count: segs.length,
        lrp_count: decodeResult.lrps?.length ?? 0,
        // Untrimmed decoded path: sum of length_m gives the full route length before offsets
        path_segments: segs.map(s => ({
          segment_id: s.segment_id,
          frc:        s.frc,
          fow:        s.fow,
          direction:  s.direction,
          length_m:   s.length_m,
        })),
        path_total_length_m: Math.round(totalLengthM * 10) / 10,
        pos_offset_m: decodeResult.pos_offset_lb != null
          ? { lb: decodeResult.pos_offset_lb, ub: decodeResult.pos_offset_ub }
          : null,
        neg_offset_m: decodeResult.neg_offset_lb != null
          ? { lb: decodeResult.neg_offset_lb, ub: decodeResult.neg_offset_ub }
          : null,
        params: {
          search_radius_m:        params?.candidate_search_radius_m,
          bearing_tolerance_deg:  params?.max_bearing_deviation_deg,
          dnp_tolerance_pct:      params?.dnp_tolerance_pct,
          lfrcnp_tolerance:       params?.lfrcnp_tolerance,
          max_candidate_score:    params?.max_candidate_score,
          max_candidates_per_lrp: params?.max_candidates_per_lrp,
        },
      });
    }

    case 'get_parsed_reference': {
      const lrps = (decodeResult.lrps ?? []).map((l, i) => {
        const isLast = i === decodeResult.lrps.length - 1;
        return {
          index: i,
          lat: l.lat,
          lon: l.lon,
          bearing: { lb: l.bearing_lb, ub: l.bearing_ub },
          frc: l.frc,
          frc_label: FRC_LABELS[l.frc] ?? null,
          fow: l.fow,
          fow_label: FOW_LABELS[l.fow] ?? null,
          lfrcnp: isLast ? null : l.lfrcnp,
          dnp_m: isLast ? null
            : l.dnp_lb != null ? { lb: l.dnp_lb, ub: l.dnp_ub ?? l.dnp_lb }
            : null,
        };
      });
      return JSON.stringify({ lrps });
    }

    case 'get_lrp_candidates': {
      const { lrp_index, include_rejected = false } = args;
      const ranked = getTraceEvents(events, 'CandidatesRanked');
      const data = ranked.find(e => e.lrp_idx === lrp_index);
      if (!data) return JSON.stringify({ error: `No candidate trace data for LRP ${lrp_index}.` });

      const accepted = (data.accepted ?? []).map(c => ({
        segment_id:   c.segment_id,
        traversal:    c.traversal,
        distance_m:   c.projection?.distance_m,
        bearing_deg:  c.projection?.bearing_deg,
        arc_offset_m: c.projection?.arc_offset_m,
        score:        c.score,
      }));

      const result = {
        lrp_index,
        accepted_count: accepted.length,
        rejected_count: data.rejected_count ?? data.rejected?.length ?? 0,
        accepted,
      };

      if (include_rejected) {
        result.rejected = (data.rejected ?? []).map(r => ({
          segment_id: r.segment_id,
          distance_m: r.projection?.distance_m,
          bearing_deg: r.projection?.bearing_deg,
          verdict:    r.verdict,
        }));
      }

      return JSON.stringify(result);
    }

    case 'get_leg_summary': {
      const { leg_index } = args;
      const routing = {};
      for (const ev of events) {
        const [type, data] = Object.entries(ev)[0];
        if (data.leg !== leg_index) continue;
        switch (type) {
          case 'RouteFound':      routing.result = { found: true,  ...data }; break;
          case 'RouteFailed':     routing.result = { found: false, ...data }; break;
          case 'DnpChecked':      routing.dnp    = data;                      break;
          case 'AStarTerminated': routing.astar  = data;                      break;
          default: break;
        }
      }
      if (!Object.keys(routing).length) {
        return JSON.stringify({ error: `No routing trace data for leg ${leg_index}.` });
      }
      const r = routing.result;
      const d = routing.dnp;
      const a = routing.astar;
      return JSON.stringify({
        leg_index,
        route_found:       r?.found ?? null,
        route_length_m:    r?.found ? r.length_m : null,
        route_fail_reason: r?.found === false ? r.reason : null,
        dnp: d ? {
          actual_m:  d.actual_m,
          window_lb: d.interval?.lb,
          window_ub: d.interval?.ub,
          passed:    d.passed,
        } : null,
        astar: a ? {
          nodes_expanded:       a.nodes_expanded,
          edges_skipped_frc:    a.edges_skipped_frc,
          edges_skipped_direction: a.edges_skipped_direction,
          edges_skipped_turn:   a.edges_skipped_turn,
          edges_skipped_distance: a.edges_skipped_distance,
          reason: a.reason,
        } : null,
      });
    }

    case 'get_route_segments': {
      const { leg_index } = args;
      const found = getTraceEvents(events, 'RouteFound');
      const data = found.find(e => e.leg === leg_index);
      if (!data) return JSON.stringify({ error: `No successful route found for leg ${leg_index}.` });

      // Cross-reference decodeResult.segments (which carries length_m, frc, fow, direction)
      // against the path segment IDs from the trace event.
      const segById = new Map((decodeResult.segments ?? []).map(s => [s.segment_id, s]));
      const path = (data.path ?? []).map(id => {
        const info = segById.get(id);
        return {
          segment_id: id,
          length_m:   info?.length_m  ?? null,
          frc:        info?.frc       ?? null,
          fow:        info?.fow       ?? null,
          direction:  info?.direction ?? null,
        };
      });
      const sumLengthM = path.reduce((s, seg) => s + (seg.length_m ?? 0), 0);

      return JSON.stringify({
        leg_index,
        segment_count:   path.length,
        length_m:        data.length_m,
        segment_sum_m:   Math.round(sumLengthM * 10) / 10,
        from_snap:       data.from_snap,
        to_snap:         data.to_snap,
        path,
      });
    }

    case 'get_segment_neighbors': {
      const { segment_id } = args;
      if (!decoder) return JSON.stringify({ error: 'Decoder not available.' });
      const raw = decoder.get_segment_neighbors(segment_id);
      const data = JSON.parse(raw);
      if (data.error) return raw;
      const annotate = s => ({
        ...s,
        frc_label: FRC_LABELS[s.frc] ?? null,
        fow_label: FOW_LABELS_FULL[s.fow] ?? null,
      });
      data.predecessors = (data.predecessors ?? []).map(annotate);
      data.successors   = (data.successors   ?? []).map(annotate);
      return JSON.stringify(data);
    }

    case 'get_segment': {
      const { segment_id } = args;
      if (!decoder) return JSON.stringify({ error: 'Decoder not available.' });
      const raw = decoder.get_segment(segment_id);
      const data = JSON.parse(raw);
      if (data.error) return raw;
      // Annotate frc/fow with human labels
      data.frc_label = FRC_LABELS[data.frc] ?? null;
      data.fow_label = FOW_LABELS_FULL[data.fow] ?? null;
      return JSON.stringify(data);
    }

    case 'get_segments_near': {
      const { lat, lon, radius_m = 100 } = args;
      if (!decoder) return JSON.stringify({ error: 'Decoder not available.' });
      const raw = decoder.get_segments_near(lat, lon, radius_m);
      const data = JSON.parse(raw);
      if (data.segments) {
        data.segments = data.segments.map(s => ({
          ...s,
          frc_label: FRC_LABELS[s.frc] ?? null,
          fow_label: FOW_LABELS[s.fow] ?? null,
        }));
      }
      return JSON.stringify(data);
    }

    case 'retry_decode': {
      const { params_override } = args;
      if (!decoder) return JSON.stringify({ error: 'Decoder not available.' });
      const overrideStr = typeof params_override === 'string'
        ? params_override
        : JSON.stringify(params_override);
      return decoder.retry_decode(overrideStr);
    }

    default:
      return JSON.stringify({ error: `Tool "${name}" is not yet implemented in this version.` });
  }
}
