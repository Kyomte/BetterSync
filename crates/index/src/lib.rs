//! SQLite index of one folder: the local records (with this device's
//! sequence numbers), where each chunk can be found locally, what each peer
//! has announced, and how far each peer's index has been received.

use std::path::Path;

use bsync_core::{DeviceId, FileRecord, Hash};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("corrupt record: {0}")]
    Decode(#[from] postcard::Error),
    #[error("index schema version {0} is newer than this build supports")]
    TooNew(i64),
}

pub type Result<T, E = IndexError> = std::result::Result<T, E>;

/// Ordered schema migrations; `PRAGMA user_version` counts those applied.
const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    "CREATE TABLE meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);
     CREATE TABLE files (
         path TEXT PRIMARY KEY,
         seq INTEGER NOT NULL UNIQUE,
         record BLOB NOT NULL,
         local_size INTEGER,
         local_mtime_ns INTEGER
     );
     CREATE TABLE chunks (
         hash BLOB NOT NULL,
         path TEXT NOT NULL,
         offset INTEGER NOT NULL,
         len INTEGER NOT NULL
     );
     CREATE INDEX chunks_by_hash ON chunks (hash);
     CREATE INDEX chunks_by_path ON chunks (path);
     CREATE TABLE remote_files (
         peer BLOB NOT NULL,
         path TEXT NOT NULL,
         record BLOB NOT NULL,
         PRIMARY KEY (peer, path)
     );
     CREATE INDEX remote_by_path ON remote_files (path);
     CREATE TABLE peers (
         peer BLOB PRIMARY KEY,
         index_id INTEGER NOT NULL,
         max_seq INTEGER NOT NULL
     );",
];

/// Size and mtime of the file on this device's disk when it was last
/// scanned. If both still match, the file is assumed unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalStat {
    pub size: u64,
    pub mtime_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEntry {
    pub seq: u64,
    pub record: FileRecord,
    /// `None` for tombstones and for entries never seen on this disk.
    pub stat: Option<LocalStat>,
}

/// A place in a local file where a chunk can be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkLocation {
    pub path: String,
    pub offset: u64,
    pub len: u32,
}

/// How much of a peer's index we have received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerProgress {
    /// Identifies one incarnation of the peer's index; if it changes, the
    /// peer's sequence numbers start over.
    pub index_id: u64,
    pub max_seq: u64,
}

#[derive(Debug)]
pub struct Index {
    conn: Connection,
}

fn encode(record: &FileRecord) -> Result<Vec<u8>> {
    Ok(postcard::to_stdvec(record)?)
}

fn decode(blob: &[u8]) -> Result<FileRecord> {
    Ok(postcard::from_bytes(blob)?)
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        let mut index = Self { conn };
        index.migrate()?;
        Ok(index)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let latest = MIGRATIONS.len() as i64;
        if version > latest {
            return Err(IndexError::TooNew(version));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(to_u64(version) as usize) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", i as i64 + 1)?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    /// Run `f` in one transaction; everything is rolled back if it fails.
    pub fn batch<T, E: From<IndexError>>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E> {
        self.conn
            .execute_batch("BEGIN")
            .map_err(|e| E::from(e.into()))?;
        match f(self) {
            Ok(v) => {
                self.conn
                    .execute_batch("COMMIT")
                    .map_err(|e| E::from(e.into()))?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    // ----- meta -----

    pub fn meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &[u8]) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// This index's incarnation ID, created from `new_id` on first use.
    pub fn index_id_or_init(&self, new_id: impl FnOnce() -> u64) -> Result<u64> {
        if let Some(v) = self.meta("index_id")? {
            if let Ok(bytes) = <[u8; 8]>::try_from(v.as_slice()) {
                return Ok(u64::from_le_bytes(bytes));
            }
        }
        let id = new_id();
        self.set_meta("index_id", &id.to_le_bytes())?;
        Ok(id)
    }

    // ----- local records -----

    pub fn max_seq(&self) -> Result<u64> {
        let v: Option<i64> = self
            .conn
            .query_row("SELECT MAX(seq) FROM files", [], |r| r.get(0))?;
        Ok(v.map_or(0, to_u64))
    }

    pub fn get(&self, path: &str) -> Result<Option<LocalEntry>> {
        self.conn
            .query_row(
                "SELECT seq, record, local_size, local_mtime_ns FROM files WHERE path = ?1",
                [path],
                row_to_parts,
            )
            .optional()?
            .map(parts_to_entry)
            .transpose()
    }

    /// Store a new version of a local record under the next sequence number
    /// and index its chunks. Returns the sequence number.
    pub fn put(&self, record: &FileRecord, stat: Option<LocalStat>) -> Result<u64> {
        let seq = self.max_seq()? + 1;
        self.conn.execute(
            "INSERT INTO files (path, seq, record, local_size, local_mtime_ns)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (path) DO UPDATE SET seq = excluded.seq, record = excluded.record,
                 local_size = excluded.local_size, local_mtime_ns = excluded.local_mtime_ns",
            params![
                record.path,
                to_i64(seq),
                encode(record)?,
                stat.map(|s| to_i64(s.size)),
                stat.map(|s| s.mtime_ns),
            ],
        )?;
        self.conn
            .execute("DELETE FROM chunks WHERE path = ?1", [&record.path])?;
        if record.is_live_file() {
            let mut insert = self.conn.prepare_cached(
                "INSERT INTO chunks (hash, path, offset, len) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (offset, chunk) in record.chunk_offsets() {
                insert.execute(params![
                    &chunk.hash.0[..],
                    record.path,
                    to_i64(offset),
                    chunk.len
                ])?;
            }
        }
        Ok(seq)
    }

    /// Update only the remembered on-disk stat of a record (e.g. after a
    /// touch that did not change content). Does not bump the sequence.
    pub fn set_stat(&self, path: &str, stat: Option<LocalStat>) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET local_size = ?2, local_mtime_ns = ?3 WHERE path = ?1",
            params![path, stat.map(|s| to_i64(s.size)), stat.map(|s| s.mtime_ns)],
        )?;
        Ok(())
    }

    /// All local entries, ordered by path.
    pub fn all(&self) -> Result<Vec<LocalEntry>> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, record, local_size, local_mtime_ns FROM files ORDER BY path")?;
        let rows = stmt.query_map([], row_to_parts)?;
        rows.map(|r| parts_to_entry(r?)).collect()
    }

    /// Local entries at `prefix` and below it (`prefix` itself, `prefix/…`).
    /// An empty `prefix` means everything.
    pub fn under(&self, prefix: &str) -> Result<Vec<LocalEntry>> {
        if prefix.is_empty() {
            return self.all();
        }
        // `prefix/` .. `prefix0` spans exactly the descendants ('0' follows '/').
        let mut stmt = self.conn.prepare_cached(
            "SELECT seq, record, local_size, local_mtime_ns FROM files
             WHERE path = ?1 OR (path >= ?1 || '/' AND path < ?1 || '0') ORDER BY path",
        )?;
        let rows = stmt.query_map([prefix], row_to_parts)?;
        rows.map(|r| parts_to_entry(r?)).collect()
    }

    /// Up to `limit` local entries with a sequence number above `seq`, in
    /// sequence order.
    pub fn since(&self, seq: u64, limit: usize) -> Result<Vec<LocalEntry>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT seq, record, local_size, local_mtime_ns FROM files
             WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![to_i64(seq), to_i64(limit as u64)], row_to_parts)?;
        rows.map(|r| parts_to_entry(r?)).collect()
    }

    /// Where chunk `hash` can be read from local files, according to the index.
    pub fn find_chunk(&self, hash: &Hash) -> Result<Vec<ChunkLocation>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT path, offset, len FROM chunks WHERE hash = ?1 LIMIT 8")?;
        let rows = stmt.query_map([&hash.0[..]], |r| {
            Ok(ChunkLocation {
                path: r.get(0)?,
                offset: to_u64(r.get(1)?),
                len: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ----- remote records -----

    pub fn put_remote(&self, peer: &DeviceId, record: &FileRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO remote_files (peer, path, record) VALUES (?1, ?2, ?3)
             ON CONFLICT (peer, path) DO UPDATE SET record = excluded.record",
            params![&peer.as_bytes()[..], record.path, encode(record)?],
        )?;
        Ok(())
    }

    /// What every peer has announced for `path`, ordered by peer.
    pub fn remote(&self, path: &str) -> Result<Vec<(DeviceId, FileRecord)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT peer, record FROM remote_files WHERE path = ?1 ORDER BY peer",
        )?;
        let rows = stmt.query_map([path], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (peer, blob) = row?;
            let Ok(peer) = <[u8; 32]>::try_from(peer.as_slice()) else {
                continue;
            };
            out.push((DeviceId::from_bytes(peer), decode(&blob)?));
        }
        Ok(out)
    }

    /// Every path any peer has announced, sorted.
    pub fn remote_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT path FROM remote_files ORDER BY path")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Forget everything a peer announced (its index was reset).
    pub fn clear_remote(&self, peer: &DeviceId) -> Result<()> {
        self.conn.execute(
            "DELETE FROM remote_files WHERE peer = ?1",
            [&peer.as_bytes()[..]],
        )?;
        Ok(())
    }

    pub fn peer_progress(&self, peer: &DeviceId) -> Result<Option<PeerProgress>> {
        Ok(self
            .conn
            .query_row(
                "SELECT index_id, max_seq FROM peers WHERE peer = ?1",
                [&peer.as_bytes()[..]],
                |r| {
                    Ok(PeerProgress {
                        index_id: r.get::<_, i64>(0)? as u64,
                        max_seq: to_u64(r.get(1)?),
                    })
                },
            )
            .optional()?)
    }

    pub fn set_peer_progress(&self, peer: &DeviceId, progress: PeerProgress) -> Result<()> {
        self.conn.execute(
            "INSERT INTO peers (peer, index_id, max_seq) VALUES (?1, ?2, ?3)
             ON CONFLICT (peer) DO UPDATE SET index_id = excluded.index_id,
                 max_seq = excluded.max_seq",
            params![
                &peer.as_bytes()[..],
                progress.index_id as i64,
                to_i64(progress.max_seq)
            ],
        )?;
        Ok(())
    }
}

type Parts = (i64, Vec<u8>, Option<i64>, Option<i64>);

fn row_to_parts(r: &rusqlite::Row<'_>) -> rusqlite::Result<Parts> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
}

fn parts_to_entry((seq, blob, size, mtime): Parts) -> Result<LocalEntry> {
    Ok(LocalEntry {
        seq: to_u64(seq),
        record: decode(&blob)?,
        stat: match (size, mtime) {
            (Some(size), Some(mtime_ns)) => Some(LocalStat {
                size: to_u64(size),
                mtime_ns,
            }),
            _ => None,
        },
    })
}

#[cfg(test)]
mod tests {
    use bsync_core::{ChunkRef, EntryKind, VersionVector};

    use super::*;

    fn dev(n: u8) -> DeviceId {
        DeviceId::from_bytes([n; 32])
    }

    fn file(path: &str, chunks: &[(u8, u32)]) -> FileRecord {
        FileRecord {
            path: path.into(),
            kind: EntryKind::File,
            deleted: false,
            size: chunks.iter().map(|c| u64::from(c.1)).sum(),
            mtime_ns: 1,
            version: VersionVector::new().incremented(dev(1)),
            modified_by: dev(1),
            hash: Hash([9; 32]),
            chunks: chunks
                .iter()
                .map(|&(h, len)| ChunkRef {
                    hash: Hash([h; 32]),
                    len,
                })
                .collect(),
        }
    }

    #[test]
    fn local_records_get_increasing_seqs_and_chunk_index() {
        let idx = Index::open_in_memory().unwrap();
        assert_eq!(idx.schema_version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(idx.max_seq().unwrap(), 0);

        let stat = Some(LocalStat {
            size: 30,
            mtime_ns: 5,
        });
        assert_eq!(idx.put(&file("a", &[(1, 10), (2, 20)]), stat).unwrap(), 1);
        assert_eq!(idx.put(&file("b", &[(2, 20)]), None).unwrap(), 2);
        assert_eq!(idx.get("a").unwrap().unwrap().stat, stat);

        let mut locs = idx.find_chunk(&Hash([2; 32])).unwrap();
        locs.sort_by(|x, y| x.path.cmp(&y.path));
        assert_eq!(
            locs,
            vec![
                ChunkLocation {
                    path: "a".into(),
                    offset: 10,
                    len: 20
                },
                ChunkLocation {
                    path: "b".into(),
                    offset: 0,
                    len: 20
                },
            ]
        );

        // Re-putting `a` moves it to the end and replaces its chunks.
        let mut tomb = file("a", &[]);
        tomb.deleted = true;
        assert_eq!(idx.put(&tomb, None).unwrap(), 3);
        assert!(idx.find_chunk(&Hash([1; 32])).unwrap().is_empty());
        let since: Vec<_> = idx
            .since(1, 10)
            .unwrap()
            .into_iter()
            .map(|e| e.record.path)
            .collect();
        assert_eq!(since, vec!["b", "a"]);
        assert_eq!(idx.all().unwrap().len(), 2);
    }

    #[test]
    fn under_matches_whole_components() {
        let idx = Index::open_in_memory().unwrap();
        for p in ["d", "d/x", "d/y/z", "dd", "e"] {
            idx.put(&file(p, &[]), None).unwrap();
        }
        let paths: Vec<_> = idx
            .under("d")
            .unwrap()
            .into_iter()
            .map(|e| e.record.path)
            .collect();
        assert_eq!(paths, vec!["d", "d/x", "d/y/z"]);
        assert_eq!(idx.under("").unwrap().len(), 5);
    }

    #[test]
    fn remote_records_and_peer_progress() {
        let idx = Index::open_in_memory().unwrap();
        idx.put_remote(&dev(2), &file("a", &[])).unwrap();
        idx.put_remote(&dev(3), &file("a", &[(1, 1)])).unwrap();
        idx.put_remote(&dev(3), &file("b", &[])).unwrap();
        assert_eq!(idx.remote("a").unwrap().len(), 2);
        assert_eq!(idx.remote_paths().unwrap(), vec!["a", "b"]);
        idx.clear_remote(&dev(3)).unwrap();
        assert_eq!(idx.remote_paths().unwrap(), vec!["a"]);

        assert_eq!(idx.peer_progress(&dev(2)).unwrap(), None);
        let p = PeerProgress {
            index_id: u64::MAX,
            max_seq: 7,
        };
        idx.set_peer_progress(&dev(2), p).unwrap();
        assert_eq!(idx.peer_progress(&dev(2)).unwrap(), Some(p));
    }

    #[test]
    fn index_id_is_created_once_and_survives_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("index.db");
        {
            let idx = Index::open(&dir).unwrap();
            assert_eq!(idx.index_id_or_init(|| 42).unwrap(), 42);
            idx.put(&file("a", &[]), None).unwrap();
        }
        let idx = Index::open(&dir).unwrap();
        assert_eq!(idx.index_id_or_init(|| 7).unwrap(), 42);
        assert_eq!(idx.max_seq().unwrap(), 1);
    }

    #[test]
    fn batch_rolls_back_on_error() {
        let mut idx = Index::open_in_memory().unwrap();
        let r: Result<()> = idx.batch(|i| {
            i.put(&file("a", &[]), None)?;
            Err(IndexError::TooNew(99))
        });
        assert!(r.is_err());
        assert_eq!(idx.max_seq().unwrap(), 0);
    }
}
