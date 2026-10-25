//! Content-defined chunking (FastCDC) with BLAKE3 chunk and file hashes.
//!
//! Chunk boundaries depend on content, so an edit in the middle of a file
//! only changes the chunks around it.

use std::io::{self, Read};

use crate::{ChunkRef, Hash};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkParams {
    pub min: u32,
    pub avg: u32,
    pub max: u32,
}

impl ChunkParams {
    /// 16 KiB / 64 KiB / 256 KiB.
    pub const DEFAULT: Self = Self {
        min: 16 * 1024,
        avg: 64 * 1024,
        max: 256 * 1024,
    };

    /// The smallest sizes FastCDC allows; used by the simulator so small
    /// files still have several chunks.
    pub const TINY: Self = Self {
        min: 64,
        avg: 256,
        max: 1024,
    };
}

impl Default for ChunkParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Chunk list and whole-file hash of some content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkedFile {
    pub size: u64,
    pub hash: Hash,
    pub chunks: Vec<ChunkRef>,
}

/// Chunk everything `reader` yields.
pub fn chunk_reader(reader: impl Read, params: ChunkParams) -> io::Result<ChunkedFile> {
    let mut file_hasher = blake3::Hasher::new();
    let mut chunks = Vec::new();
    let mut size = 0u64;
    let cdc = fastcdc::v2020::StreamCDC::new(
        reader,
        params.min as usize,
        params.avg as usize,
        params.max as usize,
    );
    for chunk in cdc {
        let chunk = chunk.map_err(|e| match e {
            fastcdc::v2020::Error::IoError(e) => e,
            other => io::Error::other(other.to_string()),
        })?;
        file_hasher.update(&chunk.data);
        size += chunk.data.len() as u64;
        chunks.push(ChunkRef {
            hash: Hash::of(&chunk.data),
            len: u32::try_from(chunk.length).map_err(io::Error::other)?,
        });
    }
    Ok(ChunkedFile {
        size,
        hash: file_hasher.finalize().into(),
        chunks,
    })
}

/// Chunk an in-memory buffer.
pub fn chunk_bytes(data: &[u8], params: ChunkParams) -> ChunkedFile {
    match chunk_reader(data, params) {
        Ok(c) => c,
        // Reading from a slice cannot fail.
        Err(_) => unreachable!("reading a byte slice failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random(len: usize, mut seed: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect()
    }

    #[test]
    fn empty_file_has_no_chunks() {
        let c = chunk_bytes(b"", ChunkParams::DEFAULT);
        assert_eq!(c.size, 0);
        assert!(c.chunks.is_empty());
        assert_eq!(c.hash, Hash::of(b""));
    }

    #[test]
    fn chunks_cover_file_and_respect_bounds() {
        let data = pseudo_random(3 * 1024 * 1024, 5);
        let c = chunk_bytes(&data, ChunkParams::DEFAULT);
        assert_eq!(c.hash, Hash::of(&data));
        assert_eq!(
            c.chunks.iter().map(|c| u64::from(c.len)).sum::<u64>(),
            data.len() as u64
        );
        for ch in &c.chunks[..c.chunks.len() - 1] {
            assert!((16 * 1024..=256 * 1024).contains(&ch.len));
        }
        let mut offset = 0usize;
        for ch in &c.chunks {
            assert_eq!(ch.hash, Hash::of(&data[offset..offset + ch.len as usize]));
            offset += ch.len as usize;
        }
    }

    #[test]
    fn middle_edit_of_10mb_file_changes_few_chunks() {
        let mut data = pseudo_random(10 * 1024 * 1024, 9);
        let before = chunk_bytes(&data, ChunkParams::DEFAULT);
        // Overwrite 100 bytes and insert 10 bytes in the middle.
        let mid = data.len() / 2;
        data[mid..mid + 100].fill(0xAB);
        data.splice(mid..mid, [1u8; 10]);
        let after = chunk_bytes(&data, ChunkParams::DEFAULT);

        let old: std::collections::HashSet<_> = before.chunks.iter().map(|c| c.hash).collect();
        let changed = after
            .chunks
            .iter()
            .filter(|c| !old.contains(&c.hash))
            .count();
        assert!(before.chunks.len() > 100, "{} chunks", before.chunks.len());
        assert!((1..=3).contains(&changed), "{changed} chunks changed");
    }
}
