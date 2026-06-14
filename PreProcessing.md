# OpenLRLens — Preprocessing Pipeline

> **Purpose**: Complete handoff notes for the offline build pipeline. Covers architecture,
> every processing stage and its algorithm, all design decisions and their rationale, every
> known bug and caveat, scalability findings from real benchmarks, test coverage gaps, and a
> "what to do next" list. Written after the pipeline was proven on New Zealand (63 MB, 5 s)
> and Germany (1.3 GB, 94 s processing, 13.4 GB peak RAM).

---

## 1. What the pipeline does

Converts a road network source (OSM PBF file or Overture Maps parquet) into a versioned
**PMTiles archive** that the browser decoder reads at runtime via HTTP range requests.
No server involved at runtime — the archive is static and CDN-cacheable.

The pipeline runs once per adopted source release. Output is:
- `openlrlens-{extent}-{release}.pmtiles` — the tile archive
- `manifest.json` — tells the browser which archive filename to use

---

## 2. Two code paths

### 2a. OSM PBF path (primary, actively used)

```
osm_extract  → osm_adapt  → quantize  → tile
```

`build::run_osm()` orchestrates this. Each CPU-bound stage runs via
`tokio::task::spawn_blocking` + rayon parallelism inside.

### 2b. Overture parquet path (exists, less tested)

```
extract  → adapt  → restrictions  → split  → quantize  → tile
```

`build::run()` orchestrates this. Supports multi-partition builds (auto-detects RAM,
bisects bbox, runs each partition, merges results).

The two paths share `quantize` and `tile` verbatim; they differ in the extract and
attribute-derivation stages.

---

## 3. Stage 1 — OSM Extract (`osm_extract.rs`)

### Overview

Reads an OSM PBF file in two passes using `osmpbf::ElementReader::par_map_reduce`
(parallel blob decompression + reduction). Produces `OsmData`:

```rust
pub struct OsmData {
    pub ways:               Vec<OsmWay>,           // vehicular ways with derived attributes
    pub nodes:              HashMap<i64, OsmNodeCoord>, // only nodes referenced by kept ways
    pub intersection_nodes: HashSet<i64>,           // global split-point set (pre-bbox)
    pub restrictions:       Vec<OsmRestriction>,    // no_turn relations
}
```

### Pass 1 — Ways and relations

Processes only `Element::Way` and `Element::Relation` blobs. For each Way:

**Tag parsing** (single pass over tags, all derived inline):
- `highway` → looked up in `OsmSchemaMapping` → `(frc, base_fow, vehicular)`
- `junction=roundabout|mini_roundabout` → `is_roundabout = true`
- `oneway=yes/1/true` → `oneway = 1`; `oneway=-1/reverse` → `oneway = -1`
- `dual_carriageway=yes` → `dual_carriageway = true`
- Any other key checked against `schema.exclusions` (see §5) → `excluded = true`

**Early rejection** (returns immediately, contributes nothing to accumulator):
1. `excluded == true` (tag exclusion matched)
2. No `highway` tag
3. Schema returns `None` (unrecognised highway value)
4. Schema returns `vehicular = false`
5. Fewer than 2 node refs

**Attribute finalisation** for surviving ways:
```
fow = if is_roundabout  → 4 (roundabout)
      elif dual_carriageway → 2 (multiple carriageway)
      else → base_fow from schema

direction = if is_roundabout → Forward (roundabouts are one-way by convention)
            elif oneway == 1  → Forward
            elif oneway == -1 → Backward
            else             → Both
```

**ref_count accumulation** (critical — governs where ways are split):
```
for each node_id in way.node_ids:
    if first or last node: ref_count[node_id] += 2
    else (interior node):  ref_count[node_id] += 1
```
The asymmetric `+2` for endpoints ensures every endpoint reaches the ≥ 2 threshold
regardless of how many ways share it. A node is an intersection iff `ref_count ≥ 2`.

For **Relations**, only `type=restriction` + `restriction=no_*` + `via=single-node` are
kept. Complex via-way restrictions and `only_*` restrictions are silently skipped (v1
limitation).

**Pass 1 accumulator merge** (`P1::merge`): `ways` extended, `ref_count` summed entry-by-entry, `restrictions` extended.

### Pass 2 — Node coordinates

Re-reads the PBF (same file, second sequential read). For each `Node` or `DenseNode`,
if the ID is in `ref_count`, its coordinates are stored. Nodes not referenced by any
kept vehicular way are discarded — Germany retains 52.9M of ~70M+ nodes in the PBF.

### Intersection node set

Built immediately after pass 2, **before** bbox filtering:
```rust
let intersection_nodes: HashSet<i64> = ref_count
    .iter()
    .filter(|(_, &cnt)| cnt >= 2)
    .map(|(&id, _)| id)
    .collect();
```

This must happen before bbox filtering. A junction at the bbox boundary is shared
by two ways globally; if only bbox-interior ways were counted, the boundary node
would have `ref_count = 1` (not an intersection) and the way would not be split there,
silently dropping the boundary junction from the graph.

### Bbox filter (applied last)

If `--extent` resolves to a bbox:
1. Find all node IDs whose coordinates fall within the bbox → `bbox_node_set`
2. Retain only ways that have at least one node in `bbox_node_set`
3. Retain only nodes referenced by the surviving ways (including nodes outside the
   bbox that are needed for boundary-crossing ways)

`intersection_nodes` is **not** re-filtered — it retains global intersection status.

---

## 4. Stage 2 — OSM Adapt (`osm_adapt.rs`)

### Overview

Converts `OsmData` into the tile pipeline's edge/node/restriction types. For the OSM
path this is simpler than the Overture path because attribute derivation already happened
in `osm_extract`.

### Way splitting

```rust
pub fn adapt(data: OsmData) -> (Vec<SplitEdge>, Vec<NodeRecord>, Vec<RestrictionTriple>)
```

Uses `rayon::par_iter` over `OsmData.ways`. For each `OsmWay`, `split_way()`:

1. Collect "split start indices": index 0 always; any interior node index where
   `intersection_nodes.contains(node_id)`.
2. For each consecutive pair of split-start indices `[start_idx, end_idx]`:
   - Collect geometry vertices from `node_coords`; skip sub-edge and warn if any
     node coordinate is missing
   - Compute `length_m` via `polyline_length_m` (sum of Haversine segments)
   - Encode `start_gers = encode_node_id(start_nid)`, `end_gers = encode_node_id(end_nid)`
   - Emit `SplitEdge` + two `NodeRecord`s

### Stable ID encoding for OSM

The OSM path cannot use Overture GERS UUIDs. Instead it derives stable 16-byte IDs
from the numeric OSM IDs with disjoint namespaces:

```
Node ID:  [0u8; 8] ++ (node_id as u64).to_le_bytes()   → bytes 0–7 = 0
Way ID:   (way_id as u64).to_le_bytes() ++ [0u8; 8]    → bytes 8–15 = 0
```

These spaces are disjoint by construction (a node ID has zeroes in bytes 0–7; a way ID
has zeroes in bytes 8–15). The tile restriction lookup requires
`from_segment_gers == parent_gers_id` (way encoding) and
`via_connector_gers == end/start_node_gers` (node encoding).

### Node deduplication

Node records from parallel workers are merged via a `HashMap<[u8;16], NodeRecord>`;
last writer wins. Coordinates should agree (same OSM node ID → same coordinates);
if they disagree, this silently keeps an arbitrary copy.

### Turn restrictions

Each `OsmRestriction {from_way_id, via_node_id, to_way_id}` maps to:
```rust
RestrictionTriple {
    from_segment_gers:  encode_way_id(from_way_id),
    via_connector_gers: encode_node_id(via_node_id),
    to_segment_gers:    encode_way_id(to_way_id),
    flags: encode_restriction_flags(HEADING_ANY, HEADING_ANY),
}
```
No heading conditions for basic OSM restrictions (conditional restrictions not modelled in v1).

---

## 5. Stage 1 (Overture) — Extract (`extract.rs`)

Range-reads Overture transportation parquet from S3. For each parquet file:
- Issues a DuckDB query with a spatial bbox filter
- Parses `geometry_wkb` (WKB LineString → `Vec<(f64,f64)>`)
- Parses `connectors` JSON array → `Vec<ConnectorRef {connector_id, at}>`
- Parses `prohibited_transitions` JSON → turn restriction data
- Returns `Vec<RawSegment>`

**Least-tested part of the codebase** — see §12.

---

## 6. Stage 2 (Overture) — Adapt (`adapt.rs`)

Maps Overture `class`/`subclass`/`road_flags` → `frc`/`fow`/`direction`/`vehicular`
using `pipeline/schema/overture-default.toml`. Filters non-vehicular segments.

Direction derivation from `access_restrictions`:
- Entry with `access_type=denied` + `when.heading=backward` → `Forward`
- Entry with `access_type=denied` + `when.heading=forward` → `Backward`
- No directional restriction → `Both`

---

## 7. Stage 3 (Overture) — Restrictions (`restrictions.rs`)

Flattens `prohibited_transitions` from adapted segments into `RestrictionTriple` list.

### Overture restriction format

Each `ProhibitedTransition` on a segment has:
- `sequence: Vec<{connector_id, segment_id}>` — the turn path
- `when_condition.heading` — direction on FROM segment that triggers the ban
- `final_heading` — required direction on TO segment

### Processing rules

- **Empty sequence**: logged as warn, skipped
- **Multi-hop (len > 1)**: logged as warn, skipped (v1 limitation)
- **Single-hop**: from = parent segment, via = `sequence[0].connector_id`,
  to = `sequence[0].segment_id`

### Heading flags encoding

```rust
flags = (from_heading & 0x03) | ((to_heading & 0x03) << 2)
// bits [1:0] = from_heading (HEADING_ANY=0, FORWARD=1, BACKWARD=2)
// bits [3:2] = to_heading
// bits [7:4] = reserved (zero)
```

`parse_heading(s)`: `"forward"` → 1, `"backward"` → 2, anything else → 0 (HEADING_ANY).
Case-sensitive string match — an OSM-style `"Forward"` with capital F would silently
become HEADING_ANY.

---

## 8. Stage 4 (Overture) — Split (`split.rs`)

### Overview

Splits each Overture segment at its interior connectors to produce node-to-node
`SplitEdge`s (CLAUDE.md Invariant 1).

```rust
pub fn split(
    segments: Vec<AdaptedSegment>,
    vehicular_endpoints: &HashSet<String>,  // connector IDs that are endpoints of vehicular segs
) -> (Vec<SplitEdge>, Vec<NodeRecord>)
```

Runs via `rayon::into_par_iter` over segments. Node deduplication after collection:
`HashMap<[u8;16], NodeRecord>`, last writer wins.

### Connector filtering (vehicular-only split points)

For each connector on a segment:
```
keep if: connector.at ≤ 1e-9            (own start endpoint — always keep)
      OR connector.at ≥ 1.0 − 1e-9     (own end endpoint — always keep)
      OR connector_id ∈ vehicular_endpoints   (interior junction with vehicular road)
```

`vehicular_endpoints` is built in `build::run_partition()` by collecting all connector
IDs that appear as endpoints (`at ≤ 1e-9` or `at ≥ 1.0 − 1e-9`) across all vehicular
segments. This ensures interior connectors that only connect to footpaths, cycleways,
or service roads do NOT trigger a split.

### Geometry interpolation

`cumulative_lengths(geometry)` builds prefix-sum arc-lengths using Haversine:
```
cum[0] = 0
cum[i] = cum[i-1] + haversine_m(geom[i-1], geom[i])
```

`sub_geometry(geometry, cum, t_start, t_end)`:
1. Compute `arc_start = t_start × total_length`, `arc_end = t_end × total_length`
2. Interpolate start point: `interp_at_arc(geometry, cum, arc_start)`
   - Finds the segment `[i-1, i]` where `cum[i] ≥ arc_target`
   - Linear interpolation within that segment: `t = (arc_target - cum[i-1]) / seg_len`
   - Returns `p0 + t × (p1 - p0)` (Cartesian interpolation, not great-circle)
3. Collect original vertices `geom[i]` where `arc_start + 1e-6 < cum[i] < arc_end - 1e-6`
4. Interpolate end point
5. Return assembled sub-polyline

`polyline_length_m(geom)`: sum of Haversine distances. **This is the canonical length**
stored in `SplitEdge.length_m` and carried through to the tile. It is never recomputed
from quantized geometry (Invariant 4 in CLAUDE.md).

### Haversine formula

```
R = 6,371,000 m
dlat = (lat2 - lat1) in radians
dlon = (lon2 - lon1) in radians
a = sin²(dlat/2) + cos(lat1) × cos(lat2) × sin²(dlon/2)
d = 2R × arcsin(√a)
```

### GERS ID parsing

`parse_gers_id(s)`: strips hyphens, hex-decodes, asserts exactly 16 bytes.
`parse_gers_id_or_warn(s, parent_id)`: calls above; on failure logs warn and **returns
`[0u8; 16]`**. This is a known bug — zero is a valid GERS ID and collisions are silent.

---

## 9. Stage 5 — Quantize (`quantize.rs`)

### Overview

Converts `SplitEdge`/`NodeRecord` (floating-point WGS84) to `QuantizedEdge`/
`QuantizedNode` (integer 1e-7 degree grid).

```rust
pub fn quantize(
    edges: Vec<SplitEdge>,
    nodes: Vec<NodeRecord>,
) -> (Vec<QuantizedEdge>, Vec<QuantizedNode>)
```

Fully parallel via `rayon::into_par_iter`.

### Coordinate quantization

```rust
fn quantize_coord(deg: f64) -> i32 {
    (deg * 1e7).round() as i32
}
```

Precision: 1e-7° ≈ 1.1 cm at equator (≈ 0.6 cm N/S; varies with latitude for E/W).
This is sub-pixel at any display zoom and sub-centimetre everywhere — satisfies CLAUDE.md
Invariant 4.

**Known gap**: no bounds check. A longitude of 200.0 would produce `200.0 × 1e7 = 2e9`,
which overflows `i32::MAX = 2,147,483,647`. Silently wraps to a negative number.

### Length conversion

```rust
let length_cm = (edge.length_m * 100.0).round() as u32;
```

Max representable: `u32::MAX / 100 = 42,949,672 m ≈ 42,949 km`. No road segment
approaches this. Length is stored from the pre-quantization `SplitEdge.length_m` and
is never recomputed from quantized geometry — consistent with Invariant 4.

### Lossless collinear removal

The only geometry reduction permitted (Invariant 4). A vertex `pts[i]` is removed iff
it is exactly collinear with `pts[i-1]` and `pts[i+1]` in integer coordinates:

```rust
cross = (x1 - x0) as i64 * (y2 - y0) as i64
      - (y1 - y0) as i64 * (x2 - x0) as i64;
if cross == 0 { remove pts[i] }
```

Uses `i64` to prevent overflow in the intermediate products (each factor is at most
`2 × 180 × 1e7 = 3.6e9`, product ≤ `1.3e19`, fits in `i64::MAX = 9.2e18`... barely).
Actually the max `i32` delta is `360 × 1e7 = 3.6e9` which overflows `i32`. The cast
to `i64` before multiplication is essential. Zero cross-product is exact — this is
truly lossless, not approximate.

Endpoints are always preserved. A 2-point line is returned unchanged.

---

## 10. Stage 6 — Tile (`tile.rs`)

The most complex stage. Takes all edges/nodes/restrictions and writes a PMTiles archive.

### Edge binning

Each edge is assigned to the tile whose centre-of-tile-grid-cell contains the edge's
**midpoint vertex** (index `geometry.len() / 2`):

```rust
fn edge_tile_key(edge: &QuantizedEdge, z: u8) -> TileKey {
    let (lon_e7, lat_e7) = edge.geometry[edge.geometry.len() / 2];
    let lon = lon_e7 as f64 * 1e-7;
    let lat = lat_e7 as f64 * 1e-7;
    // Web Mercator lon/lat → slippy tile (x, y) at zoom z
    let n = (1u64 << z) as f64;
    let x = ((lon + 180.0) / 360.0 * n).floor() as u32;
    let merc = ((PI/4 + lat.to_radians()/2).tan()).ln();
    let y = ((1.0 - merc / PI) / 2.0 * n).floor() as u32;
    TileKey { z, x, y }
}
```

Result: `tile_bins: HashMap<TileKey, Vec<usize>>` (global edge indices per tile).

**Design note**: using the midpoint rather than the start node means a long edge
crossing a tile boundary ends up in the tile containing its middle. The decoder must
handle edges that geometrically cross into adjacent tiles — this is expected and the
boundary node mechanism exists for this case.

### Boundary node detection

A node is a boundary node if it appears in **more than one tile** (i.e. it is the
start or end node of edges binned into different tiles). After binning:

```rust
// node_to_tile: first-seen tile for each node
let mut node_to_tile: HashMap<[u8; 16], TileKey> = HashMap::new();
let mut boundary_nodes: HashSet<[u8; 16]> = HashSet::new();
for (key, indices) in &tile_bins {
    for &idx in indices {
        let e = &edges[idx];
        for gers in [e.start_node_gers, e.end_node_gers] {
            match node_to_tile.entry(gers) {
                Occupied(prev_tile) if *prev_tile.get() != *key
                    => { boundary_nodes.insert(gers); }
                Vacant(v) => { v.insert(*key); }
                _ => {}
            }
        }
    }
}
```

Germany: 302,655 boundary nodes out of 9,350,503 total (~3.2%). These are flagged
`flags: bit0 = 1` in the node table so the decoder knows to stitch across tiles.

### Restriction resolution

Two full-graph HashMaps are built over **all** edges (the scalability bottleneck
identified in §14):

```rust
from_edge_map: HashMap<([u8;16], [u8;16]), usize>
// key: (parent_gers_id, end_node_gers) → global edge index
// matches the FROM segment: which edge ends at the via-node?

to_edge_map: HashMap<([u8;16], [u8;16]), usize>
// key: (parent_gers_id, start_node_gers) → global edge index
// matches the TO segment: which edge starts at the via-node?
```

For each `RestrictionTriple`:
1. Look up via-node's tile in `node_to_tile`; skip if not found
2. Find `from_global` via `from_edge_map[(r.from_segment_gers, r.via_connector_gers)]`
3. Find `to_global` via `to_edge_map[(r.to_segment_gers, r.via_connector_gers)]`
4. Look up via-node's local index in that tile's node map
5. If from, via, and to are all in the same tile → write to `IntraTileRestriction`
6. If they span tiles → write to `CrossTileRestriction` (uses global GERS IDs)

Germany results: 59,892 total, 32,153 intra-tile, 27,739 cross-tile (~46%).

**The "skipped" count in logs**: a restriction is counted as skipped if any of steps
1–4 fail (via-node not in any tile, from/to edge not found). This happens when the
restriction references a way that was filtered out (e.g. the from/to way is outside
the extent bbox). The remaining ~27k are genuine cross-tile restrictions, not lost ones.
**However: whether the decoder currently reads `xrestriction_count` cross-tile entries
has not been verified.** This must be confirmed.

### Tile payload binary format

Version 2 (current). All integers little-endian.

```
Header (40 bytes):
  magic:               [u8; 4]  = b"OLRL"
  version:             u8       = 2
  flags:               u8       = 0
  _pad:                [u8; 2]
  segment_count:       u32
  node_count:          u32
  restriction_count:   u32      (intra-tile restrictions)
  geom_vertex_count:   u32
  xrestriction_count:  u32      (cross-tile restrictions)
  _reserved:           [u8; 12]

Segment array: segment_count × 32 bytes
  start_node:    u32  (tile-local node index)
  end_node:      u32
  geom_offset:   u32  (vertex index into geometry pool, not byte offset)
  geom_len:      u16  (vertex count)
  length_cm:     u32
  attrs:         u8   frc[2:0] | fow[5:3] | direction[7:6]
                        direction: 0=Both 1=Forward 2=Backward
  flags:         u8   = 0 (reserved)
  _reserved:     [u8; 12]

Segment GERS-id table: segment_count × 16 bytes
  parent_gers_id for each segment (indexed by local segment index)
  Used for cross-tile restriction matching.

Geometry pool: geom_vertex_count × 8 bytes
  lon_e7:   i32  (WGS84 × 1e7, absolute — not delta-coded)
  lat_e7:   i32

Node table: node_count × 28 bytes
  lon_e7:   i32
  lat_e7:   i32
  gers_id:  [u8; 16]
  flags:    u8   bit0 = boundary node
  _pad:     [u8; 3]

Intra-tile restriction table: restriction_count × 16 bytes
  from_seg:  u32  (local segment index)
  via_node:  u32  (local node index)
  to_seg:    u32
  flags:     u8   bits[1:0]=from_heading, bits[3:2]=to_heading
  _pad:      [u8; 3]

Cross-tile restriction table: xrestriction_count × 40 bytes
  from_gers:       [u8; 16]  (GERS id of from-segment)
  via_node_local:  u32       (local node index in this tile)
  to_gers:         [u8; 16]  (GERS id of to-segment)
  flags:           u8
  _pad:            [u8; 3]
```

`attrs` byte packing:
```
bits [2:0] = frc   (0–7)
bits [5:3] = fow   (0–7)
bits [7:6] = direction  (0=Both, 1=Forward, 2=Backward)
```

### PMTiles v3 archive format

Section layout:
```
[header 127 bytes][root_directory][metadata JSON][leaf_directories][tile_data]
```

**Header** (127 bytes, all u64 LE):
- bytes 0–6: `b"PMTiles"`
- byte 7: version = 3
- bytes 8–15: root_dir_offset
- bytes 16–23: root_dir_length
- bytes 24–31: metadata_offset
- bytes 32–39: metadata_length
- bytes 40–47: leaf_dirs_offset
- bytes 48–55: leaf_dirs_length
- bytes 56–63: tile_data_offset
- bytes 64–71: tile_data_length
- bytes 72–79: addressed_tiles_count
- bytes 80–87: tile_entries_count
- bytes 88–95: tile_contents_count
- byte 96: clustered = 1
- byte 97: internal_compression = 2 (gzip)
- byte 98: tile_compression = 1 (none — tile payloads are raw binary, not re-compressed)
- byte 99: tile_type = 0 (unknown/custom)
- byte 100: min_zoom
- byte 101: max_zoom

**Directory encoding** (gzip-compressed varint stream):
- N (varint) — number of entries
- N delta-coded tile IDs (varint each)
- N run-lengths (varint each; 0 = leaf directory pointer)
- N data lengths (varint each)
- N offsets (varint each; 0 = "immediately follows previous entry")

**Two-level directory** when tile count > 16,384:
- Root holds leaf-directory pointers (run_length = 0)
- Each leaf holds up to 16,384 tile entries
- 2 levels suffices for planet-scale at any zoom: z12 world ~500k tiles → ~31 leaves

Tiles are written in **Hilbert curve order**:
```
tile_id = (4^z − 1)/3 + hilbert_index(z, x, y)
```
This clusters spatially adjacent tiles contiguously in the archive, minimising the
range-request footprint for a spatial query.

---

## 11. Stage 7 — Merge (`merge.rs`)

Merges N independently-built PMTiles archives into one. Used both for
multi-partition Overture builds (automatic) and for combining regional OSM archives
(via the `merge` CLI subcommand).

### Algorithm

K-way merge using a `BinaryHeap<Reverse<(tile_id, reader_index)>>` (min-heap):

1. Open one `PmtilesReader` per input archive
2. Prime each reader: read first tile and push `(tile_id, reader_idx)` to heap
3. While heap non-empty:
   - Pop minimum `(tile_id, idx)`
   - Write tile data to `StreamingWriter`
   - Advance reader `idx`; push its next tile to heap

Memory: O(N) where N = number of input archives — only one buffered tile per reader
at any time. Tile data itself is not buffered (written immediately to a temp file).

### PmtilesReader

Sequential tile scanner:
1. Reads 127-byte header → root_dir_offset, root_dir_length, leaf_dirs_offset, tile_data_offset
2. Decompresses root directory
3. Flattens leaf-directory pointers: for each leaf pointer in root, reads + decompresses
   that leaf directory, collects its tile entries
4. Holds flat `Vec<(tile_id, data_offset, data_length)>` sorted ascending
5. `next_tile()`: seeks to `tile_data_offset + offset`, reads `length` bytes

### StreamingWriter

Streams tile data to a `NamedTempFile` to avoid holding all tile data in RAM.
On `finish()`:
1. Builds directory from accumulated `(tile_id, offset, length)` entries
2. Writes final archive: header → root_dir → metadata → leaf_dirs → tile_data (copied
   from temp file in 256 KB chunks)

**Limitation**: directory is built entirely in RAM before write. Fine at current scale;
becomes a concern at z15+ with millions of tiles.

---

## 12. Schema configuration

### `pipeline/schema/osm-default.toml`

Highway → FRC/FOW/vehicular mapping for the OSM PBF path.

**Rules** (`[[rules]]`): matched in order, first match wins.
- `highway`: OSM highway tag value; `""` = catch-all
- `frc`: 0–7
- `fow`: 0=undefined 1=motorway 2=dual 3=single 4=roundabout 5=traffic_sq 6=slip 7=other
- `vehicular`: bool (default true). `false` = excluded entirely (nodes not in ref_count)

Current mapping summary:

| highway value | FRC | FOW | vehicular |
|---|---|---|---|
| motorway | 0 | 1 (motorway) | yes |
| motorway_link | 1 | 6 (slip road) | yes |
| trunk / trunk_link | 1 | 3 / 6 | yes |
| primary / primary_link | 2 | 3 / 6 | yes |
| secondary / secondary_link | 3 | 3 / 6 | yes |
| tertiary / tertiary_link | 4 | 3 / 6 | yes |
| yes | 5 | 3 | yes |
| unclassified | 6 | 3 | yes |
| residential | 7 | 3 | yes |
| living_street | 7 | 3 | yes |
| road | 7 | 3 | yes |
| track | 7 | 3 | yes |
| **service** | 7 | 7 | **no** |
| pedestrian / footway / cycleway / path / steps / bridleway | 7 | 7 | no |

Note: `junction=roundabout` → FOW=4 and `dual_carriageway=yes` → FOW=2 are applied
at extract time regardless of the FOW value in this table.

**Exclusions** (`[exclusions]`): tag key → list of values that cause a way to be
dropped in pass 1, before the schema lookup.

```toml
area   = ["yes", "true", "1"]    # mapped areas (parking lots etc), not drivable lines
access = ["private", "no"]       # inaccessible roads
```

**`highway=service` is `vehicular=false`** (deliberate). This excludes all service
roads — parking aisles, driveways, drive-throughs, forecourts, but also legitimate
access roads to industrial/commercial sites. The exclusion also means service road
nodes never enter `ref_count`, so service road junctions don't spuriously split main
roads. This trade-off was made after observing parking lots and drive-throughs
appearing in the visualiser. May need refinement once the decoder exercises service
road references.

**`access=no` limitation**: `access=no; motor_vehicle=yes` (pedestrian zone with
vehicle access) is also excluded. The current implementation has no per-tag weighting.

### `pipeline/schema/overture-default.toml`

Overture class/subclass → FRC/FOW/vehicular mapping + flag overrides. Not covered in
detail here as the OSM path is the primary active path.

---

## 13. CLI reference

```
openlrlens-build [GLOBAL FLAGS] <SUBCOMMAND>

SUBCOMMANDS:
  list-releases
  build --extent <spec> [--pbf <path|URL>] [--release <ver>] [options]
  merge --output <path> <INPUTS>...

GLOBAL FLAGS:
  -v / -vv                log verbosity (info/debug/trace); RUST_LOG overrides
  --retry-max <n>         HTTP retry attempts [5]
  --retry-base-ms <ms>    initial backoff [200]
  --retry-max-ms <ms>     backoff cap [30000]
  --retry-factor <f>      backoff multiplier [2.0]

BUILD FLAGS:
  --pbf <path|URL>        OSM PBF (local file or https:// URL, auto-downloaded)
  --osm-schema <path>     OSM schema TOML [pipeline/schema/osm-default.toml]
  --release <ver>         Overture release e.g. 2026-05-20.0
  --schema <path>         Overture schema TOML [pipeline/schema/overture-default.toml]
  --extent <spec>         ISO code (NZ/DE/GB), continent (europe), 'world', or W,S,E,N
  --output <dir>          output directory [./out]
  -j / --jobs <n>         rayon thread count [logical CPU count]
  --fetch-concurrency <n> concurrent S3 parquet downloads [8] (Overture only)
  --tile-zoom <8–15>      PMTiles zoom level [12]
  --ram-gb <f>            override RAM detection (Overture multi-partition only)
  --bytes-per-segment <n> RAM estimate per segment (Overture only)

MERGE FLAGS:
  <INPUTS>...             .pmtiles files or directories (first .pmtiles found in each dir)
  --output <path>         merged archive path e.g. out/world/world.pmtiles
  --extent <label>        extent label for output manifest.json [world]
```

**URL download**: streams to `{filename}` in the current working directory.
No resume support — interrupted downloads must restart. File is always overwritten
(no ETag/mtime check). Progress logged every 50 MB.

**Parallel invocations**: multiple instances are safe as long as each uses a different
`--output` directory. Divide rayon threads with `-j <total_cores / N>`.

---

## 14. Benchmark results

### New Zealand (NZ)

| Metric | Value |
|---|---|
| Source PBF | `new-zealand-latest.osm.pbf`, 379 MB |
| Processing time | 4.9 s |
| Peak RAM | ~100 MB (estimated) |
| Output archive | 63 MB |
| Tiles at z12 | 4,235 |
| Edges | 382,508 |
| Nodes | 325,719 |
| Restrictions (total/resolved/cross-tile) | 3,393 / 1,696 / 1,697 |

### Germany (DE)

| Metric | Value |
|---|---|
| Source PBF | `germany-latest.osm.pbf`, 4.5 GB |
| Download time | ~57 min (connection-limited) |
| Processing time | 94.5 s |
| Peak RSS | **13.4 GB** (`/usr/bin/time -l`) |
| Output archive | 1.3 GB |
| Tiles at z12 | 10,019 |
| Edges | 11,435,767 |
| Nodes | 9,350,503 |
| Restrictions (total/resolved/cross-tile) | 59,892 / 32,153 / 27,739 |

**Processing phase breakdown (Germany)**:

| Phase | Duration |
|---|---|
| PBF pass 1 (7.4M ways + ref_count) | 20 s |
| PBF pass 2 (52.9M node coords) | 31 s |
| Bbox filter | 18 s |
| Adapt + split (parallel) | 7 s |
| Quantize (parallel) | <1 s |
| Tile — bin edges | 2 s |
| Tile — boundary nodes | 2 s |
| Tile — restriction resolution | 8 s |
| Tile — write PMTiles | 6 s |
| **Total** | **94.5 s** |

**NZ → Germany scaling**:

| Metric | NZ | Germany | Ratio |
|---|---|---|---|
| Needed nodes | 4.3M | 52.9M | 12× |
| Edges | 382k | 11.4M | 30× |
| Archive | 63 MB | 1.3 GB | 21× |
| Processing time | 4.9 s | 94.5 s | 19× |
| Peak RAM | ~100 MB | **13.4 GB** | **135×** |

**Critical finding**: RAM scales super-linearly (135× for 12× the data). HashMap
per-entry bookkeeping (~48 bytes/entry) dominates over the payload at 50M+ entries.
Germany fits on 16 GB but a machine needs ~14 GB free. Planet (~10× Germany) needs
partitioning.

---

## 15. Scalability issues (priority order)

### Blockers at planet scale

**1. HashMap overhead in node maps (`osm_extract.rs`)**
`ref_count` and `all_node_coords` both grow to cover all vehicular-way nodes globally.
Germany: 52.9M entries × ~48 bytes each × 2 maps ≈ **5 GB** for node data alone.
Planet: ~300M nodes → **~29 GB** just for these two maps, held simultaneously.
Fix: sort-and-scan on disk, or accept partitioning as mandatory for continent+ extents.

**2. Full-graph edge maps in tile writer (`tile.rs:559–563`)**
`from_edge_map` and `to_edge_map` each hold all edges (key = 32 bytes, value = 8 bytes,
HashMap overhead ~24 bytes each → ~64 bytes/entry × 2 maps × 300M planet edges ≈ **38 GB**).
Fix: process restrictions tile-by-tile with a sorted edge list instead of global HashMaps.

**3. Partition density assumption (`partition.rs`)**
Estimates segment count as `GLOBAL_SEGMENT_ESTIMATE × bbox_area_fraction`. Assumes
uniform road density. Dense cities (Berlin, Tokyo) have 10–100× the density of the
global average. A partition sized correctly by area will OOM when it hits an
unexpectedly dense region.
Fix: read actual segment counts from parquet metadata before partitioning.

**4. No checkpoint/resume**
Failed builds (network blip during download, OOM mid-tile-write) restart from zero.
Fix: serialise `OsmData` to a sidecar `.cache` file after pass 2; skip both PBF passes
on subsequent runs when PBF mtime is unchanged. Germany rebuild would drop from 94 s
to ~25 s for schema-only changes.

### Significant but manageable

**5. Single-threaded merge**
The k-way merge heap loop is serial. Merging 50 regional archives (500k tiles) takes
minutes; 500 archives would take hours.
Fix: parallel reader threads feeding a concurrent writer.

**6. No inter-stage streaming**
All N edges sit in memory at both input and output of each stage. Peak = two
consecutive stages simultaneously. Fix: iterator-based streaming pipeline.

**7. PBF read twice**
Two full sequential reads per build: 20 s + 31 s = 51 s out of 94 s for Germany.
Fix: extract cache (sidecar file, invalidated by PBF mtime).

**8. URL download never skips existing file**
Re-running a build re-downloads the PBF even if it already exists locally.
Fix: HEAD request for Content-Length; skip download if local file matches.

---

## 16. Test coverage gaps (priority order)

### Critical — could produce silent wrong output

**`tile::write_tiles()` — zero test coverage**
The most complex function. No test verifies edge binning, boundary node flagging,
restriction table placement, or payload binary layout roundtrip.

**`merge_pmtiles()` tested only with 2 archives**
The k-way heap merge is untested with 3+ inputs. Overlapping tile IDs across inputs
are not detected (silent corrupt output — no `ensure!` check exists).

**`parse_gers_id_or_warn()` returns `[0u8;16]` on failure**
Zero is a valid GERS ID. Two segments with unparseable IDs silently share identity,
corrupting turn restriction lookups and cross-tile stitching. Should hard-error.

**`quantize_coord()` has no overflow guard**
`lat/lon × 1e7` cast to `i32` overflows for coordinates outside ±180/±90 (rare but
possible with corrupt PBF data). Silent wrong output.

**Overture `extract.rs` — almost entirely untested**
WKB parsing, DuckDB queries, S3 XML parsing all have no tests. JSON deserialization
failures return `unwrap_or_default()` (empty vec) silently.

### Important correctness gaps

**`extent::resolve()` accepts inverted bbox** — `west > east` produces empty or
nonsensical output without error.

**`restrictions.rs` heading string is case-sensitive** — `"Forward"` (capital F)
silently becomes HEADING_ANY instead of HEADING_FORWARD.

**`osm_extract::extract()` untested** — pass 1/2 logic, restriction extraction,
and intersection counting all have zero direct test coverage.

**Cross-tile restriction consumption unverified** — ~46% of restrictions are written
to the cross-tile table. Whether the decoder reads these has not been confirmed.

---

## 17. Known correctness issues not yet fixed

1. **`parse_gers_id_or_warn` zero-collision** (`split.rs:258`): returns `[0u8;16]`
   on parse failure. Should be a hard error propagated up.

2. **Duplicate tile ID check missing in merge**: overlapping regional archives produce
   a PMTiles file with ambiguous tile entries (later entry silently wins in a reader).

3. **`--pbf` always re-downloads**: no ETag or file-size check before downloading.

4. **`highway=service` blanket exclusion**: removes legitimate industrial access roads.
   No service road OpenLR references have been tested against the decoder yet.

5. **`access=no` too broad**: excludes pedestrian zones with `motor_vehicle=yes`.

6. **Multi-hop Overture restrictions silently skipped**: `prohibited_transitions` with
   `sequence.len() > 1` are dropped with a warn. These represent via-way restrictions
   which can be significant in complex intersections.

---

## 18. File inventory

```
pipeline/
  src/
    build.rs          Orchestrators: run_osm(), run(), run_partition(), write_top_manifest (pub crate)
    cli.rs            Cli / BuildArgs / MergeArgs (clap)
    main.rs           Entry point; run_merge(); find_pmtiles_in_dir()
    osm_extract.rs    Two-pass PBF reader → OsmData
    osm_adapt.rs      OsmData → SplitEdge/NodeRecord/RestrictionTriple; OSM stable ID encoding
    osm_schema.rs     OsmSchemaMapping TOML; lookup(); DEFAULT_SCHEMA_STR (include_str embed)
    split.rs          Overture connector-based splitter; haversine; sub_geometry; GERS ID parse
    quantize.rs       1e-7 degree quantization; collinear removal
    tile.rs           Edge binner; tile payload builder; PMTiles v3 writer; Hilbert curve; manifest
    merge.rs          PmtilesReader; StreamingWriter; merge_pmtiles() k-way merge
    restrictions.rs   Overture prohibited_transitions → RestrictionTriple; heading flags
    adapt.rs          Overture segment adapter (class/subclass → frc/fow/direction/vehicular)
    extract.rs        Overture S3 parquet downloader + DuckDB query + WKB parser
    extent.rs         --extent resolver (ISO / continent / world / explicit bbox)
    partition.rs      RAM-aware bbox bisection for Overture multi-partition builds
    http.rs           Reqwest client with retry config; download_to_file()
    releases.rs       Overture S3 release listing + XML parse
    schema.rs         Overture TOML schema loader
    parquet_meta.rs   Overture parquet metadata helpers
  schema/
    osm-default.toml       OSM highway → FRC/FOW/vehicular/exclusions
    overture-default.toml  Overture class/subclass → FRC/FOW + flag overrides

web/
  src/
    main.js           MapLibre map; tile loader (?tiles= param); segment click/highlight; legend
    decoder.js        Binary tile payload → GeoJSON features (custom OLRL format decoder)
  vite.config.js      Dev server; serve-tiles plugin (HTTP 206 range support for PMTiles)

out/
  nz-osm/             63 MB archive, 4,235 tiles, 382k edges — fast rebuild reference
  de-osm/             1.3 GB archive, 10,019 tiles, 11.4M edges — scalability reference
  world/              Merge landing zone (currently just NZ test merge)

new-zealand-latest.osm.pbf   379 MB — kept for fast iterative schema testing (~5 s rebuild)
germany-latest.osm.pbf       4.5 GB — kept to avoid 57-min re-download
```

---

## 19. How to rebuild

```bash
cd /Users/dave/projects/rust/openlr_lens

# Recompile after source changes
cargo build --release -p openlrlens-build

# NZ — fast schema iteration (~5 s, uses cached PBF)
./target/release/openlrlens-build build \
  --pbf new-zealand-latest.osm.pbf --extent NZ --output out/nz-osm

# Germany — scalability reference (~94 s processing, PBF already present)
./target/release/openlrlens-build build \
  --pbf germany-latest.osm.pbf --extent DE --output out/de-osm

# Germany from URL (re-downloads 4.5 GB)
./target/release/openlrlens-build build \
  --pbf https://download.geofabrik.de/europe/germany-latest.osm.pbf \
  --extent DE --output out/de-osm

# Merge regional archives into one
./target/release/openlrlens-build merge \
  --output out/world/openlrlens-world.pmtiles \
  out/nz-osm out/de-osm

# View in browser
cd web && npm run dev
# http://localhost:5173?tiles=nz-osm
# http://localhost:5173?tiles=de-osm
```

---

## 20. What to do next

### Correctness (fix before trusting decode results)

- [ ] Fix `parse_gers_id_or_warn` → propagate `Result`, hard error on bad IDs
- [ ] Verify decoder reads and applies `xrestriction_count` cross-tile entries
- [ ] Add `quantize_coord` bounds check: `assert!(deg.abs() <= 180.0)` (lon) / `90.0` (lat)
- [ ] Add inverted-bbox check in `extent::resolve()`
- [ ] Add duplicate tile ID detection in `merge_pmtiles()`

### Test coverage

- [ ] E2E test for `tile::write_tiles()`: synthetic edge set → write tile → parse binary
  → assert segment/node/restriction counts and spot-check field values
- [ ] `merge_pmtiles()` with 3+ archives; test for tile ID ordering preservation
- [ ] `osm_extract::extract()` integration test (small synthetic PBF or test fixture)
- [ ] Restriction heading case-sensitivity test

### Performance (in order of expected impact)

- [ ] Extract cache: serialise `OsmData` to `{pbf_stem}.cache` (bincode/rmp-serde);
  skip PBF passes if PBF mtime unchanged → Germany schema rebuild: 94 s → ~25 s
- [ ] Download skip: HEAD request before downloading; compare Content-Length to local size
- [ ] Replace `from_edge_map`/`to_edge_map` globals with per-tile sorted edge lists
  → removes the planet-scale blocker in `tile.rs`
- [ ] `--dry-run` flag: print resolved bbox, estimated segment count, partition plan, exit
