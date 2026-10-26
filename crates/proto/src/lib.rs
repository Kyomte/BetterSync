//! Wire messages (serde + postcard), framing and protocol version.
//!
//! Every message travels as a frame: a 4-byte little-endian length followed
//! by the postcard encoding of a [`Message`].

use bsync_core::{DeviceId, FileRecord, FolderId, Hash};
use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to [`Message`].
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest frame accepted (a chunk is at most 256 KiB; index batches are
/// limited by count).
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Most records sent in one [`Message::IndexUpdate`].
pub const INDEX_BATCH: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// First message on every connection, in both directions.
    Hello {
        protocol_version: u32,
        device_id: DeviceId,
        device_name: String,
        /// Folders the sender syncs with the receiver.
        folders: Vec<FolderId>,
    },
    /// "Send me your index from `since_seq` on." `known_index_id` is the
    /// index incarnation the sequence number refers to; if the sender's
    /// index has a different ID, it starts over from zero.
    IndexRequest {
        folder: FolderId,
        known_index_id: u64,
        since_seq: u64,
    },
    /// Records with sequence numbers up to `last_seq`. `reset` means the
    /// receiver must forget what it had from this peer for the folder.
    IndexUpdate {
        folder: FolderId,
        index_id: u64,
        reset: bool,
        records: Vec<FileRecord>,
        last_seq: u64,
    },
    RequestChunk {
        folder: FolderId,
        id: u64,
        hash: Hash,
    },
    /// Reply to [`Message::RequestChunk`]; `data` is `None` if the sender no
    /// longer has the chunk.
    ChunkData {
        folder: FolderId,
        id: u64,
        hash: Hash,
        data: Option<Vec<u8>>,
    },
    Ping,
    Pong,
}

impl Message {
    pub fn folder(&self) -> Option<&FolderId> {
        match self {
            Message::IndexRequest { folder, .. }
            | Message::IndexUpdate { folder, .. }
            | Message::RequestChunk { folder, .. }
            | Message::ChunkData { folder, .. } => Some(folder),
            Message::Hello { .. } | Message::Ping | Message::Pong => None,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        // Serializing to a Vec only fails for types serde cannot express;
        // `Message` has none.
        postcard::to_stdvec(self).unwrap_or_default()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("malformed message: {0}")]
    Decode(#[from] postcard::Error),
    #[error("frame of {0} bytes exceeds the limit")]
    TooLarge(usize),
}

/// A complete frame (length prefix + body) for `msg`.
pub fn encode_frame(msg: &Message) -> Vec<u8> {
    let body = msg.encode();
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Length of the body announced by a frame header.
pub fn frame_len(header: [u8; 4]) -> Result<usize, ProtoError> {
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(ProtoError::TooLarge(len));
    }
    Ok(len)
}

/// Incrementally splits a byte stream into messages.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete message, if one has arrived.
    pub fn next_message(&mut self) -> Result<Option<Message>, ProtoError> {
        let Some(header) = self.buf.get(..4) else {
            return Ok(None);
        };
        let len = frame_len([header[0], header[1], header[2], header[3]])?;
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let msg = Message::decode(&self.buf[4..4 + len]);
        self.buf.drain(..4 + len);
        msg.map(Some)
    }
}

#[cfg(test)]
mod tests {
    use bsync_core::{EntryKind, VersionVector};

    use super::*;

    fn samples() -> Vec<Message> {
        let dev = DeviceId::from_bytes([3; 32]);
        let folder = FolderId("photos".into());
        vec![
            Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_id: dev,
                device_name: "laptop".into(),
                folders: vec![folder.clone()],
            },
            Message::IndexRequest {
                folder: folder.clone(),
                known_index_id: 9,
                since_seq: 41,
            },
            Message::IndexUpdate {
                folder: folder.clone(),
                index_id: 9,
                reset: false,
                records: vec![FileRecord {
                    path: "a/b.txt".into(),
                    kind: EntryKind::File,
                    deleted: false,
                    size: 3,
                    mtime_ns: -5,
                    version: VersionVector::new().incremented(dev),
                    modified_by: dev,
                    hash: Hash::of(b"abc"),
                    chunks: vec![],
                }],
                last_seq: 42,
            },
            Message::RequestChunk {
                folder: folder.clone(),
                id: 1,
                hash: Hash::of(b"x"),
            },
            Message::ChunkData {
                folder,
                id: 1,
                hash: Hash::of(b"x"),
                data: Some(b"x".to_vec()),
            },
            Message::Ping,
            Message::Pong,
        ]
    }

    #[test]
    fn messages_round_trip_through_frames_split_anywhere() {
        let mut stream = Vec::new();
        for m in samples() {
            stream.extend(encode_frame(&m));
        }
        for step in [1, 3, 7, stream.len()] {
            let mut dec = FrameDecoder::default();
            let mut got = Vec::new();
            for piece in stream.chunks(step) {
                dec.push(piece);
                while let Some(m) = dec.next_message().unwrap() {
                    got.push(m);
                }
            }
            assert_eq!(got, samples());
        }
    }

    #[test]
    fn oversized_and_garbage_frames_are_rejected() {
        let mut dec = FrameDecoder::default();
        dec.push(&(MAX_FRAME as u32 + 1).to_le_bytes());
        assert!(matches!(dec.next_message(), Err(ProtoError::TooLarge(_))));
        let mut dec = FrameDecoder::default();
        dec.push(&[2, 0, 0, 0, 0xFF, 0xFF]);
        assert!(dec.next_message().is_err());
    }
}
