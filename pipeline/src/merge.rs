use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write as IoWrite};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use tempfile::NamedTempFile;
use tracing::{debug, info};

// ── Varint helpers ────────────────────────────────────────────────────────────

fn write_uvarint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 { out.push(b); break; }
        out.push(b | 0x80);
    }
}

fn read_uvarint(data: &[u8], pos: &mut usize) -> Result<u64> {
    let mut result = 0u64;
    let mut shift  = 0u32;
    loop {
        let byte = *data.get(*pos).context("unexpected end of PMTiles directory")?;
        *pos += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 { return Ok(result); }
        shift += 7;
        anyhow::ensure!(shift < 64, "varint overflow");
    }
}

// ── Directory encode / decode ─────────────────────────────────────────────────

/// `entries`: `(tile_id, offset, length, run_length)`.
/// `run_length >= 1` → tile; `run_length == 0` → leaf-directory pointer.
fn encode_directory(entries: &[(u64, u64, u32, u32)]) -> Vec<u8> {
    let mut raw = Vec::new();
    write_uvarint(&mut raw, entries.len() as u64);

    let mut last_id = 0u64;
    for &(id, ..) in entries {
        write_uvarint(&mut raw, id - last_id);
        last_id = id;
    }
    for &(_, _, _, rl) in entries { write_uvarint(&mut raw, rl as u64); }
    for &(_, _, len, _) in entries { write_uvarint(&mut raw, len as u64); }
    for (i, &(_, offset, _length, _)) in entries.iter().enumerate() {
        if i > 0 {
            let (_, prev_off, prev_len, _) = entries[i - 1];
            if offset == prev_off + prev_len as u64 {
                write_uvarint(&mut raw, 0);
                continue;
            }
        }
        write_uvarint(&mut raw, offset + 1);
    }
    raw
}

/// Returns `(tile_id, data_offset, data_length, run_length)` for each entry.
fn decode_directory(data: &[u8]) -> Result<Vec<(u64, u64, u32, u32)>> {
    let mut pos = 0usize;
    let n = read_uvarint(data, &mut pos)? as usize;

    let mut ids      = Vec::with_capacity(n);
    let mut rls      = Vec::with_capacity(n);
    let mut lens     = Vec::with_capacity(n);
    let mut offsets  = Vec::with_capacity(n);

    let mut last_id = 0u64;
    for _ in 0..n { last_id += read_uvarint(data, &mut pos)?; ids.push(last_id); }
    for _ in 0..n { rls.push(read_uvarint(data, &mut pos)? as u32); }
    for _ in 0..n { lens.push(read_uvarint(data, &mut pos)? as u32); }

    for i in 0..n {
        let raw = read_uvarint(data, &mut pos)?;
        let off = if raw == 0 {
            if i == 0 { 0 } else { offsets[i - 1] + lens[i - 1] as u64 }
        } else {
            raw - 1
        };
        offsets.push(off);
    }

    Ok((0..n).map(|i| (ids[i], offsets[i], lens[i], rls[i])).collect())
}

// ── Gzip helpers ──────────────────────────────────────────────────────────────

fn gzip_compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(data).context("gzip write")?;
    gz.finish().context("gzip finish")
}

fn gzip_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut gz  = GzDecoder::new(data);
    let mut out = Vec::new();
    gz.read_to_end(&mut out).context("gzip decompress")?;
    Ok(out)
}

// ── Directory builder (mirrors tile.rs build_directory) ───────────────────────

const ENTRIES_PER_LEAF: usize = 16_384;

/// Build root + leaf directory blobs from a slice of `(tile_id, offset, length)`.
/// Returns `(root_compressed, leaf_dirs_compressed_concatenated)`.
fn build_directory(tile_entries: &[(u64, u64, u32)]) -> Result<(Vec<u8>, Vec<u8>)> {
    if tile_entries.len() <= ENTRIES_PER_LEAF {
        let entries: Vec<(u64, u64, u32, u32)> = tile_entries
            .iter()
            .map(|&(id, off, len)| (id, off, len, 1))
            .collect();
        let root = gzip_compress(&encode_directory(&entries))?;
        Ok((root, Vec::new()))
    } else {
        let mut leaf_data: Vec<u8> = Vec::new();
        let mut root_entries: Vec<(u64, u64, u32, u32)> = Vec::new();

        for chunk in tile_entries.chunks(ENTRIES_PER_LEAF) {
            let first_id    = chunk[0].0;
            let leaf_offset = leaf_data.len() as u64;
            let leaf_entries: Vec<(u64, u64, u32, u32)> = chunk
                .iter()
                .map(|&(id, off, len)| (id, off, len, 1))
                .collect();
            let compressed = gzip_compress(&encode_directory(&leaf_entries))?;
            let leaf_len   = compressed.len() as u32;
            leaf_data.extend_from_slice(&compressed);
            root_entries.push((first_id, leaf_offset, leaf_len, 0));
        }

        let root = gzip_compress(&encode_directory(&root_entries))?;
        Ok((root, leaf_data))
    }
}

// ── PMTiles reader (sequential tile scan) ────────────────────────────────────

struct PmtilesReader {
    file:             File,
    tile_data_offset: u64,
    /// All actual tile entries (tile_id, data_offset, data_length), sorted ascending.
    entries:          Vec<(u64, u64, u32)>,
    pos:              usize,
}

impl PmtilesReader {
    fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)
            .with_context(|| format!("open {}", path.display()))?;

        let mut hdr = [0u8; 127];
        file.read_exact(&mut hdr).context("read PMTiles header")?;

        anyhow::ensure!(&hdr[0..7] == b"PMTiles",
            "not a PMTiles file: {}", path.display());
        anyhow::ensure!(hdr[7] == 3,
            "unsupported PMTiles version {} in {}", hdr[7], path.display());

        let root_off  = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
        let root_len  = u64::from_le_bytes(hdr[16..24].try_into().unwrap());
        let leaf_off  = u64::from_le_bytes(hdr[40..48].try_into().unwrap());
        let tile_off  = u64::from_le_bytes(hdr[56..64].try_into().unwrap());

        // Read + decompress root directory.
        let root_raw  = read_section(&mut file, root_off, root_len)?;
        let root_entries = decode_directory(&root_raw)?;

        // Flatten leaf-directory pointers into a flat tile entry list.
        let mut entries: Vec<(u64, u64, u32)> = Vec::new();
        for (tile_id, offset, length, rl) in root_entries {
            if rl > 0 {
                entries.push((tile_id, offset, length));
            } else {
                // Leaf pointer: navigate to leaf directory.
                let leaf_raw = read_section(&mut file, leaf_off + offset, length as u64)?;
                for (lid, loff, llen, lrl) in decode_directory(&leaf_raw)? {
                    if lrl > 0 {
                        entries.push((lid, loff, llen));
                    }
                }
            }
        }

        Ok(Self { file, tile_data_offset: tile_off, entries, pos: 0 })
    }

    /// Return the next `(tile_id, compressed_bytes)` or `None` if exhausted.
    fn next_tile(&mut self) -> Result<Option<(u64, Vec<u8>)>> {
        if self.pos >= self.entries.len() {
            return Ok(None);
        }
        let (tile_id, offset, length) = self.entries[self.pos];
        self.pos += 1;

        let abs = self.tile_data_offset + offset;
        let mut buf = vec![0u8; length as usize];
        self.file.seek(SeekFrom::Start(abs))
            .context("seek tile data")?;
        self.file.read_exact(&mut buf)
            .context("read tile data")?;
        Ok(Some((tile_id, buf)))
    }
}

fn read_section(file: &mut File, offset: u64, length: u64) -> Result<Vec<u8>> {
    let mut raw = vec![0u8; length as usize];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut raw)?;
    gzip_decompress(&raw)
}

// ── Streaming PMTiles writer ──────────────────────────────────────────────────

pub(crate) struct StreamingWriter {
    tile_data_tmp:  NamedTempFile,
    entries:        Vec<(u64, u64, u32)>,   // (tile_id, offset, length)
    current_offset: u64,
}

impl StreamingWriter {
    pub(crate) fn new() -> Result<Self> {
        let tile_data_tmp = NamedTempFile::new().context("create tile data temp file")?;
        Ok(Self { tile_data_tmp, entries: Vec::new(), current_offset: 0 })
    }

    pub(crate) fn add_tile(&mut self, tile_id: u64, data: &[u8]) -> Result<()> {
        self.tile_data_tmp.write_all(data).context("write tile data")?;
        self.entries.push((tile_id, self.current_offset, data.len() as u32));
        self.current_offset += data.len() as u64;
        Ok(())
    }

    pub(crate) fn finish(mut self, output_path: &Path, tile_zoom: u8) -> Result<()> {
        let n_tiles = self.entries.len() as u64;
        let (root_compressed, leaf_data) = build_directory(&self.entries)?;

        let metadata            = b"{}";
        let root_dir_offset     = 127u64;
        let root_dir_length     = root_compressed.len() as u64;
        let metadata_offset     = root_dir_offset + root_dir_length;
        let metadata_length     = metadata.len() as u64;
        let leaf_dirs_offset    = metadata_offset + metadata_length;
        let leaf_dirs_length    = leaf_data.len() as u64;
        let tile_data_offset    = leaf_dirs_offset + leaf_dirs_length;
        let tile_data_length    = self.current_offset;

        let mut hdr = [0u8; 127];
        hdr[0..7].copy_from_slice(b"PMTiles");
        hdr[7]    = 3;
        hdr[8..16].copy_from_slice(&root_dir_offset.to_le_bytes());
        hdr[16..24].copy_from_slice(&root_dir_length.to_le_bytes());
        hdr[24..32].copy_from_slice(&metadata_offset.to_le_bytes());
        hdr[32..40].copy_from_slice(&metadata_length.to_le_bytes());
        hdr[40..48].copy_from_slice(&leaf_dirs_offset.to_le_bytes());
        hdr[48..56].copy_from_slice(&leaf_dirs_length.to_le_bytes());
        hdr[56..64].copy_from_slice(&tile_data_offset.to_le_bytes());
        hdr[64..72].copy_from_slice(&tile_data_length.to_le_bytes());
        hdr[72..80].copy_from_slice(&n_tiles.to_le_bytes());
        hdr[80..88].copy_from_slice(&n_tiles.to_le_bytes());
        hdr[88..96].copy_from_slice(&n_tiles.to_le_bytes());
        hdr[96] = 1; // clustered
        hdr[97] = 2; // internal_compression = gzip
        hdr[98] = 1; // tile_compression = none (payloads are not re-compressed)
        hdr[99] = 0; // tile_type = unknown/custom
        hdr[100] = tile_zoom; // min_zoom
        hdr[101] = tile_zoom; // max_zoom

        let mut out = File::create(output_path)
            .with_context(|| format!("create {}", output_path.display()))?;
        out.write_all(&hdr).context("write header")?;
        out.write_all(&root_compressed).context("write root dir")?;
        out.write_all(metadata).context("write metadata")?;
        out.write_all(&leaf_data).context("write leaf dirs")?;

        // Stream tile data from temp file.
        self.tile_data_tmp.seek(SeekFrom::Start(0))?;
        let mut buf = vec![0u8; 256 * 1024]; // 256 KB copy buffer
        loop {
            let n = self.tile_data_tmp.read(&mut buf).context("read tile temp")?;
            if n == 0 { break; }
            out.write_all(&buf[..n]).context("write tile data")?;
        }

        Ok(())
    }
}

// ── Public merge entry point ──────────────────────────────────────────────────

/// Merge multiple PMTiles archives (produced by separate partition runs) into a single
/// Hilbert-sorted PMTiles archive.
///
/// All input archives must cover non-overlapping tile sets (guaranteed when the build
/// uses non-overlapping bbox partitions).  Tiles are merged in tile-ID order so the
/// output is clustered.
///
/// Memory usage: O(N) where N = number of input archives (one buffered tile per reader).
pub fn merge_pmtiles(inputs: &[PathBuf], output: &Path, tile_zoom: u8) -> Result<()> {
    anyhow::ensure!(!inputs.is_empty(), "merge_pmtiles: no input archives");

    if inputs.len() == 1 {
        std::fs::copy(&inputs[0], output)
            .with_context(|| format!("copy {} → {}", inputs[0].display(), output.display()))?;
        return Ok(());
    }

    // Open one reader per input archive.
    let mut readers: Vec<PmtilesReader> = inputs
        .iter()
        .map(|p| PmtilesReader::open(p))
        .collect::<Result<_>>()?;

    // Prime each reader and seed the heap with the first tile from each.
    // Heap item: (Reverse(tile_id), reader_index) — min-heap by tile_id.
    let mut current: Vec<Option<(u64, Vec<u8>)>> = readers
        .iter_mut()
        .map(|r| r.next_tile())
        .collect::<Result<_>>()?;

    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = BinaryHeap::new();
    for (i, slot) in current.iter().enumerate() {
        if let Some((tile_id, _)) = slot {
            heap.push(Reverse((*tile_id, i)));
        }
    }

    let mut writer = StreamingWriter::new()?;
    let mut n_merged = 0u64;

    while let Some(Reverse((tile_id, idx))) = heap.pop() {
        let (_, data) = current[idx].take().unwrap();
        writer.add_tile(tile_id, &data)?;
        n_merged += 1;

        // Advance this reader and push its next tile.
        current[idx] = readers[idx].next_tile()?;
        if let Some((next_id, _)) = &current[idx] {
            heap.push(Reverse((*next_id, idx)));
        }

        if n_merged % 10_000 == 0 {
            debug!(tiles = n_merged, "merging…");
        }
    }

    info!(tiles = n_merged, archives = inputs.len(), output = %output.display(), "merge complete");
    writer.finish(output, tile_zoom)?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uvarint_roundtrip() {
        for &v in &[0u64, 1, 127, 128, 16383, 16384, u32::MAX as u64, u64::MAX / 2] {
            let mut buf = Vec::new();
            write_uvarint(&mut buf, v);
            let mut pos = 0;
            assert_eq!(read_uvarint(&buf, &mut pos).unwrap(), v);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn directory_roundtrip() {
        let entries = vec![
            (1u64, 0u64, 100u32, 1u32),
            (2,    100,  200,    1),
            (5,    300,  150,    1),
        ];
        let encoded  = encode_directory(&entries);
        let decoded  = decode_directory(&encoded).unwrap();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0], (1, 0, 100, 1));
        assert_eq!(decoded[1], (2, 100, 200, 1));
        assert_eq!(decoded[2], (5, 300, 150, 1));
    }

    #[test]
    fn merge_two_archives_roundtrip() {
        // Build two small PMTiles archives and merge them; verify tile count.
        use crate::tile::write_pmtiles_file_pub;

        let tmp1 = tempfile::NamedTempFile::new().unwrap();
        let tmp2 = tempfile::NamedTempFile::new().unwrap();
        let out  = tempfile::NamedTempFile::new().unwrap();

        // Archive 1: tiles 0 and 2
        write_pmtiles_file_pub(&[(0, vec![0xAAu8; 10]), (2, vec![0xBBu8; 20])], tmp1.path(), 12).unwrap();
        // Archive 2: tiles 1 and 3
        write_pmtiles_file_pub(&[(1, vec![0xCCu8; 15]), (3, vec![0xDDu8; 25])], tmp2.path(), 12).unwrap();

        merge_pmtiles(&[tmp1.path().to_owned(), tmp2.path().to_owned()], out.path()).unwrap();

        // Verify output has 4 tiles in order.
        let mut reader = PmtilesReader::open(out.path()).unwrap();
        let mut ids = Vec::new();
        while let Some((id, _)) = reader.next_tile().unwrap() {
            ids.push(id);
        }
        assert_eq!(ids, vec![0, 1, 2, 3]);
    }
}
