//! PMTiles v3 reader.
//!
//! Reads a local `.pmtiles` archive and returns raw tile bytes by `TileKey`.
//! Directories are gzip-compressed; individual tile payloads are uncompressed
//! (as written by the openlrlens pipeline).

use std::io::{self, Read, Seek, SeekFrom};
use std::fs::File;
use std::path::Path;

use flate2::read::GzDecoder;
use openlr_graph::TileKey;

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum PmtilesError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("bad PMTiles magic: expected 'PMTiles', got {0:?}")]
    BadMagic(Vec<u8>),
    #[error("unsupported PMTiles version {0} (expected 3)")]
    UnsupportedVersion(u8),
    #[error("tile not found: z={z} x={x} y={y}")]
    TileNotFound { z: u8, x: u32, y: u32 },
    #[error("directory decompression failed: {0}")]
    Decompress(String),
}

// ── Directory entry ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct DirEntry {
    tile_id: u64,
    /// Absolute byte offset within the tile-data section.
    offset: u64,
    /// Byte length of the tile data.
    length: u32,
    /// Number of consecutive tile IDs covered (0 = leaf-directory pointer).
    run_length: u32,
}

// ── Reader ────────────────────────────────────────────────────────────────────

pub struct PmtilesReader {
    file: File,
    tile_data_offset: u64,
    leaf_dirs_offset: u64,
    root_entries: Vec<DirEntry>,
    min_zoom: u8,
}

impl PmtilesReader {
    /// Open a PMTiles v3 archive.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PmtilesError> {
        let mut file = File::open(path)?;

        // ── Header (127 bytes) ───────────────────────────────────────────────
        let mut hdr = [0u8; 127];
        file.read_exact(&mut hdr)?;

        // Magic
        if &hdr[0..7] != b"PMTiles" {
            return Err(PmtilesError::BadMagic(hdr[0..7].to_vec()));
        }
        if hdr[7] != 3 {
            return Err(PmtilesError::UnsupportedVersion(hdr[7]));
        }

        let root_dir_offset = u64_le(&hdr, 8);
        let root_dir_length = u64_le(&hdr, 16);
        let _metadata_offset = u64_le(&hdr, 24);
        let _metadata_length = u64_le(&hdr, 32);
        let leaf_dirs_offset = u64_le(&hdr, 40);
        let _leaf_dirs_length= u64_le(&hdr, 48);
        let tile_data_offset = u64_le(&hdr, 56);
        let min_zoom         = hdr[100];

        // ── Root directory ───────────────────────────────────────────────────
        file.seek(SeekFrom::Start(root_dir_offset))?;
        let mut root_compressed = vec![0u8; root_dir_length as usize];
        file.read_exact(&mut root_compressed)?;
        let root_bytes = decompress_gzip(&root_compressed)
            .map_err(|e| PmtilesError::Decompress(e.to_string()))?;
        let root_entries = parse_directory(&root_bytes);

        Ok(Self { file, tile_data_offset, leaf_dirs_offset, root_entries, min_zoom })
    }

    /// Read the raw tile bytes for `key`. Returns `None` if the tile is absent.
    pub fn get_tile(&mut self, key: TileKey) -> Result<Option<Vec<u8>>, PmtilesError> {
        let tile_id = xyz_to_tile_id(key.z, key.x, key.y);
        match self.find_entry(&self.root_entries.clone(), tile_id)? {
            None => Ok(None),
            Some(entry) => {
                let abs_offset = self.tile_data_offset + entry.offset;
                self.file.seek(SeekFrom::Start(abs_offset))?;
                let mut data = vec![0u8; entry.length as usize];
                self.file.read_exact(&mut data)?;
                Ok(Some(data))
            }
        }
    }

    fn find_entry(
        &mut self,
        entries: &[DirEntry],
        tile_id: u64,
    ) -> Result<Option<DirEntry>, PmtilesError> {
        // Binary search: find last entry whose tile_id ≤ requested.
        let idx = entries.partition_point(|e| e.tile_id <= tile_id);
        if idx == 0 {
            return Ok(None);
        }
        let entry = &entries[idx - 1];

        if entry.run_length == 0 {
            // Leaf directory pointer: entry.offset = byte offset within leaf_dirs section.
            let abs = self.leaf_dirs_offset + entry.offset;
            self.file.seek(SeekFrom::Start(abs))?;
            let mut compressed = vec![0u8; entry.length as usize];
            self.file.read_exact(&mut compressed)?;
            let leaf_bytes = decompress_gzip(&compressed)
                .map_err(|e| PmtilesError::Decompress(e.to_string()))?;
            let leaf_entries = parse_directory(&leaf_bytes);
            return self.find_entry(&leaf_entries, tile_id);
        }

        // Tile entry: covers [tile_id, tile_id + run_length).
        if tile_id < entry.tile_id + entry.run_length as u64 {
            let run_idx = tile_id - entry.tile_id;
            Ok(Some(DirEntry {
                tile_id,
                offset: entry.offset + run_idx * entry.length as u64,
                length: entry.length,
                run_length: 1,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn min_zoom(&self) -> u8 { self.min_zoom }
}

// ── Directory parsing ─────────────────────────────────────────────────────────

fn parse_directory(b: &[u8]) -> Vec<DirEntry> {
    let mut pos = 0;
    let n = read_uvarint(b, &mut pos) as usize;

    // Read four parallel arrays.
    let mut tile_ids  = Vec::with_capacity(n);
    let mut run_lengths = Vec::with_capacity(n);
    let mut lengths   = Vec::with_capacity(n);
    let mut offsets   = Vec::with_capacity(n);

    let mut acc_id: u64 = 0;
    for _ in 0..n {
        let delta = read_uvarint(b, &mut pos);
        acc_id += delta;
        tile_ids.push(acc_id);
    }
    for _ in 0..n { run_lengths.push(read_uvarint(b, &mut pos) as u32); }
    for _ in 0..n { lengths.push(read_uvarint(b, &mut pos) as u32); }
    for _ in 0..n { offsets.push(read_uvarint(b, &mut pos)); }

    // Decode sequential offset encoding: 0 means previous.offset + previous.length.
    let mut entries = Vec::with_capacity(n);
    let mut prev_offset: u64 = 0;
    let mut prev_length: u32 = 0;
    for i in 0..n {
        let offset = if offsets[i] == 0 && i > 0 {
            prev_offset + prev_length as u64
        } else if offsets[i] == 0 {
            0
        } else {
            offsets[i] - 1
        };
        entries.push(DirEntry {
            tile_id: tile_ids[i],
            offset,
            length: lengths[i],
            run_length: run_lengths[i],
        });
        prev_offset = offset;
        prev_length = lengths[i];
    }
    entries
}

// ── Hilbert tile ID ───────────────────────────────────────────────────────────

fn xyz_to_tile_id(z: u8, x: u32, y: u32) -> u64 {
    if z == 0 { return 0; }
    let acc = ((1u64 << (2 * z as u64)) - 1) / 3;
    let n = 1u64 << z;
    acc + hilbert_d(n, x as u64, y as u64)
}

fn hilbert_d(n: u64, mut x: u64, mut y: u64) -> u64 {
    let mut d: u64 = 0;
    let mut s = n / 2;
    while s > 0 {
        let rx = if x & s > 0 { 1u64 } else { 0 };
        let ry = if y & s > 0 { 1u64 } else { 0 };
        d += s * s * ((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                x = s.wrapping_sub(1).wrapping_sub(x);
                y = s.wrapping_sub(1).wrapping_sub(y);
            }
            std::mem::swap(&mut x, &mut y);
        }
        s /= 2;
    }
    d
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn u64_le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o+8].try_into().unwrap())
}

fn read_uvarint(b: &[u8], pos: &mut usize) -> u64 {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let byte = b[*pos];
        *pos += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        shift += 7;
        if byte & 0x80 == 0 { break; }
    }
    result
}

fn decompress_gzip(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut dec = GzDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)?;
    Ok(out)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_id_zoom0() {
        assert_eq!(xyz_to_tile_id(0, 0, 0), 0);
    }

    #[test]
    fn tile_id_zoom1() {
        // Zoom 1: acc = (4-1)/3 = 1.
        // Hilbert curve for 2×2 grid traverses (0,0)→(0,1)→(1,1)→(1,0).
        assert_eq!(xyz_to_tile_id(1, 0, 0), 1);
        assert_eq!(xyz_to_tile_id(1, 0, 1), 2);
        assert_eq!(xyz_to_tile_id(1, 1, 1), 3);
        assert_eq!(xyz_to_tile_id(1, 1, 0), 4);
    }

    #[test]
    fn uvarint_single_byte() {
        let b = [0x2A];
        let mut pos = 0;
        assert_eq!(read_uvarint(&b, &mut pos), 42);
        assert_eq!(pos, 1);
    }

    #[test]
    fn uvarint_multi_byte() {
        // 300 in uvarint: 0xAC 0x02
        let b = [0xAC, 0x02];
        let mut pos = 0;
        assert_eq!(read_uvarint(&b, &mut pos), 300);
        assert_eq!(pos, 2);
    }

    #[test]
    fn parse_empty_directory() {
        // 0 entries = single uvarint(0)
        let b = [0x00];
        let entries = parse_directory(&b);
        assert!(entries.is_empty());
    }
}
