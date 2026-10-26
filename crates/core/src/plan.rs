//! The sync planner: given this device's record for a path and what peers
//! have announced for it, decide what to do. Pure and deterministic — every
//! device reaches the same decision from the same records.
//!
//! Conflict rules:
//! * A version that has seen another (version vectors) replaces it.
//! * Concurrent versions with identical content just merge their vectors.
//! * Otherwise one version wins by a fixed rule that never consults clocks
//!   (live beats deleted — an edit always beats a delete; directories beat
//!   files; then the higher device ID). The device holding the losing
//!   version locally saves it as a conflict copy and then takes the winner
//!   with the merged vector. A device holding the winner waits for that, so
//!   a merged vector only ever exists once the loser has been saved.

use std::cmp::Ordering;

use crate::time::format_utc_compact;
use crate::{Causality, EntryKind, FileRecord};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Make the disk match `record` (write the file, create the directory or
    /// delete the entry) and store it as the local record.
    Apply(FileRecord),
    /// The disk already matches; only the stored version changes.
    AdoptVersion(FileRecord),
    /// The local version lost a conflict: save it as `loser` under a
    /// conflict-copy name, then apply `record`.
    Conflict {
        record: FileRecord,
        loser: FileRecord,
    },
}

impl Action {
    pub fn record(&self) -> &FileRecord {
        match self {
            Action::Apply(r) | Action::AdoptVersion(r) | Action::Conflict { record: r, .. } => r,
        }
    }
}

/// Total order deciding which of two concurrent versions wins. Greater wins.
pub fn conflict_order(a: &FileRecord, b: &FileRecord) -> Ordering {
    let key = |r: &FileRecord| (!r.deleted, r.kind == EntryKind::Dir, r.modified_by, r.hash);
    key(a)
        .cmp(&key(b))
        .then_with(|| a.mtime_ns.cmp(&b.mtime_ns))
        .then_with(|| a.size.cmp(&b.size))
}

/// The version this device should converge to among what peers announced:
/// a version no other one has seen past, choosing by [`conflict_order`] if
/// several are concurrent.
pub fn pick_target(remotes: &[FileRecord]) -> Option<&FileRecord> {
    let maximal = remotes.iter().filter(|r| {
        !remotes
            .iter()
            .any(|o| o.version.compare(&r.version) == Causality::Newer)
    });
    maximal.max_by(|a, b| conflict_order(a, b))
}

/// What to do about one path.
pub fn plan_path(local: Option<&FileRecord>, remotes: &[FileRecord]) -> Option<Action> {
    let target = pick_target(remotes)?;
    let Some(local) = local else {
        return Some(if target.deleted {
            Action::AdoptVersion(target.clone())
        } else {
            Action::Apply(target.clone())
        });
    };
    match local.version.compare(&target.version) {
        Causality::Newer => None,
        Causality::Older if local.same_content(target) => {
            Some(Action::AdoptVersion(target.clone()))
        }
        Causality::Older => Some(Action::Apply(target.clone())),
        Causality::Equal if local.same_content(target) => None,
        // Equal vectors with different content should not happen; resolve it
        // like a conflict so devices still converge.
        Causality::Equal | Causality::Concurrent => {
            // Some announced version has already seen past ours, so ours is
            // not a contender even though it is concurrent with the winner.
            let newer: Vec<&FileRecord> = remotes
                .iter()
                .filter(|r| r.version.compare(&local.version) == Causality::Newer)
                .collect();
            if !newer.is_empty() {
                // A newer record with our content just carries it forward
                // (a merged vector): become that record, and resolve the
                // conflict as one of its holders.
                if let Some(carrier) = newer
                    .iter()
                    .filter(|r| r.same_content(local))
                    .max_by(|a, b| conflict_order(a, b))
                {
                    return Some(Action::AdoptVersion((*carrier).clone()));
                }
                // Otherwise its author replaced our content knowingly. Take
                // the winner as announced; never mint a merged vector here,
                // since only a device that saved the loser may do that.
                return Some(if local.same_content(target) {
                    Action::AdoptVersion(target.clone())
                } else {
                    Action::Apply(target.clone())
                });
            }
            let merged = local.version.merged(&target.version);
            if local.same_content(target) {
                let mut r = if conflict_order(local, target) == Ordering::Less {
                    target.clone()
                } else {
                    local.clone()
                };
                r.version = merged;
                return Some(Action::AdoptVersion(r));
            }
            if conflict_order(local, target) != Ordering::Less {
                // We hold the winner; the loser's holder will resolve.
                return None;
            }
            let mut record = target.clone();
            record.version = merged;
            if local.deleted {
                Some(Action::Apply(record))
            } else {
                Some(Action::Conflict {
                    record,
                    loser: local.clone(),
                })
            }
        }
    }
}

/// `dir/name.sync-conflict-YYYYMMDD-HHMMSS-DEVICE.ext` for the losing
/// version `loser` of `path`. Derived only from the loser so every device
/// that saves it picks the same name.
pub fn conflict_copy_path(loser: &FileRecord) -> String {
    let (dir, name) = match loser.path.rfind('/') {
        Some(i) => (&loser.path[..=i], &loser.path[i + 1..]),
        None => ("", loser.path.as_str()),
    };
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    format!(
        "{dir}{stem}.sync-conflict-{}-{}{ext}",
        format_utc_compact(loser.mtime_ns),
        loser.modified_by.short()
    )
}

/// Order in which planned actions should be applied: directories top-down,
/// then files, then deletions bottom-up (so a directory is emptied before
/// it is removed, and renamed content is fetched before its old copy goes).
pub fn apply_order(a: &Action, b: &Action) -> Ordering {
    fn key(action: &Action) -> (u8, i64, &str) {
        let r = action.record();
        let depth = r.path.matches('/').count() as i64;
        match (action, r.deleted, r.kind) {
            (Action::AdoptVersion(_), _, _) => (0, depth, &r.path),
            (_, false, EntryKind::Dir) => (1, depth, &r.path),
            (_, false, EntryKind::File) => (2, depth, &r.path),
            (_, true, _) => (3, -depth, &r.path),
        }
    }
    key(a).cmp(&key(b))
}

#[cfg(test)]
#[allow(clippy::cloned_ref_to_slice_refs)] // `&[x.clone()]` reads better here
mod tests {
    use super::*;
    use crate::{DeviceId, Hash, VersionVector};

    fn d(n: u8) -> DeviceId {
        DeviceId::from_bytes([n; 32])
    }

    fn file(content: &[u8], by: u8, version: &[(u8, u64)]) -> FileRecord {
        FileRecord {
            path: "dir/notes.txt".into(),
            kind: EntryKind::File,
            deleted: false,
            size: content.len() as u64,
            mtime_ns: 1_700_000_000_000_000_000,
            version: version.iter().fold(VersionVector::new(), |v, &(dev, n)| {
                (0..n).fold(v, |v, _| v.incremented(d(dev)))
            }),
            modified_by: d(by),
            hash: Hash::of(content),
            chunks: Vec::new(),
        }
    }

    fn deleted(mut r: FileRecord) -> FileRecord {
        r.deleted = true;
        r.hash = Hash::default();
        r
    }

    #[test]
    fn newer_remote_is_applied_older_is_ignored() {
        let local = file(b"a", 1, &[(1, 1)]);
        let remote = file(b"b", 2, &[(1, 1), (2, 1)]);
        assert_eq!(
            plan_path(Some(&local), &[remote.clone()]),
            Some(Action::Apply(remote.clone()))
        );
        assert_eq!(plan_path(Some(&remote), &[local.clone()]), None);
        assert_eq!(plan_path(Some(&local), &[local.clone()]), None);
        assert_eq!(
            plan_path(None, &[remote.clone()]),
            Some(Action::Apply(remote))
        );
        assert_eq!(plan_path(Some(&local), &[]), None);
    }

    #[test]
    fn remote_tombstone_for_unknown_path_is_only_recorded() {
        let t = deleted(file(b"", 2, &[(2, 2)]));
        assert_eq!(plan_path(None, &[t.clone()]), Some(Action::AdoptVersion(t)));
    }

    #[test]
    fn same_content_only_adopts_version() {
        let local = file(b"same", 1, &[(1, 1)]);
        let newer = file(b"same", 2, &[(1, 1), (2, 1)]);
        assert_eq!(
            plan_path(Some(&local), &[newer.clone()]),
            Some(Action::AdoptVersion(newer))
        );

        let concurrent = file(b"same", 2, &[(2, 1)]);
        let Some(Action::AdoptVersion(r)) = plan_path(Some(&local), &[concurrent.clone()]) else {
            panic!("expected AdoptVersion");
        };
        assert_eq!(r.version, local.version.merged(&concurrent.version));
        // Both sides compute the identical record.
        let Some(Action::AdoptVersion(r2)) = plan_path(Some(&concurrent), &[local.clone()]) else {
            panic!("expected AdoptVersion");
        };
        assert_eq!(r, r2);
    }

    #[test]
    fn concurrent_edits_loser_holder_saves_copy() {
        let mine = file(b"mine", 1, &[(1, 1)]);
        let theirs = file(b"theirs", 2, &[(2, 1)]); // device 2 > device 1: wins
        let Some(Action::Conflict { record, loser }) = plan_path(Some(&mine), &[theirs.clone()])
        else {
            panic!("expected conflict");
        };
        assert_eq!(loser, mine);
        assert_eq!(record.hash, theirs.hash);
        assert_eq!(record.version, mine.version.merged(&theirs.version));
        // The winner's holder waits.
        assert_eq!(plan_path(Some(&theirs), &[mine.clone()]), None);
        // Once the merged record arrives, the winner's holder adopts it.
        assert_eq!(
            plan_path(Some(&theirs), &[record.clone()]),
            Some(Action::AdoptVersion(record))
        );
    }

    #[test]
    fn edit_beats_delete_in_both_directions() {
        let edit = file(b"edit", 1, &[(1, 1), (2, 1)]);
        let del = deleted(file(b"", 2, &[(2, 2)]));
        assert_eq!(edit.version.compare(&del.version), Causality::Concurrent);
        // Device with the delete takes the edit (nothing to save).
        let Some(Action::Apply(r)) = plan_path(Some(&del), &[edit.clone()]) else {
            panic!("expected apply");
        };
        assert!(!r.deleted);
        assert_eq!(r.version, edit.version.merged(&del.version));
        // Device with the edit keeps it.
        assert_eq!(plan_path(Some(&edit), &[del]), None);
    }

    #[test]
    fn directory_beats_file() {
        let f = file(b"x", 9, &[(9, 1)]);
        let dir = FileRecord {
            kind: EntryKind::Dir,
            hash: Hash::default(),
            ..file(b"", 1, &[(1, 1)])
        };
        assert!(matches!(
            plan_path(Some(&f), &[dir.clone()]),
            Some(Action::Conflict { .. })
        ));
        assert_eq!(plan_path(Some(&dir), &[f]), None);
    }

    /// Regression (fuzz seed 3): two concurrent deletions, and a local live
    /// version that one of them already supersedes. The local version must
    /// not "win" against the other deletion and stay forever.
    #[test]
    fn locally_obsolete_version_does_not_contend() {
        let mine = file(b"old", 3, &[(3, 1)]);
        let del_a = deleted(file(b"", 4, &[(2, 2), (4, 2)]));
        let del_b = deleted(file(b"", 3, &[(3, 5)]));
        let remotes = [del_a.clone(), del_b];
        assert_eq!(pick_target(&remotes), Some(&del_a));
        assert_eq!(plan_path(Some(&mine), &remotes), Some(Action::Apply(del_a)));
    }

    /// Regression (fuzz seed 46): a newer record that merely carries our
    /// content forward must not make us drop that content.
    #[test]
    fn newer_record_with_our_content_is_adopted_not_discarded() {
        let mine = file(b"ours", 1, &[(1, 80), (4, 1)]);
        let carrier = FileRecord {
            version: mine.version.merged(&VersionVector::from([(d(4), 18)])),
            ..mine.clone()
        };
        let winner = file(b"theirs", 3, &[(1, 18), (3, 80), (4, 18)]);
        let remotes = [carrier.clone(), winner];
        assert_eq!(
            plan_path(Some(&mine), &remotes),
            Some(Action::AdoptVersion(carrier.clone()))
        );
        // Next round, as the carrier's holder, we lose and save a copy.
        assert!(matches!(
            plan_path(Some(&carrier), &remotes),
            Some(Action::Conflict { .. })
        ));
    }

    #[test]
    fn target_is_maximal_and_deterministic() {
        let base = file(b"0", 1, &[(1, 1)]);
        let a = file(b"a", 2, &[(1, 1), (2, 1)]);
        let b = file(b"b", 3, &[(1, 1), (3, 1)]);
        let rs = [base.clone(), a.clone(), b.clone()];
        assert_eq!(pick_target(&rs), Some(&b));
        let rs2 = [b.clone(), base, a];
        assert_eq!(pick_target(&rs2), Some(&b));
    }

    #[test]
    fn conflict_copy_names() {
        let mut r = file(b"x", 1, &[(1, 1)]);
        r.mtime_ns = 1_709_212_455 * 1_000_000_000;
        let dev = d(1).short();
        assert_eq!(
            conflict_copy_path(&r),
            format!("dir/notes.sync-conflict-20240229-131415-{dev}.txt")
        );
        r.path = ".bashrc".into();
        assert_eq!(
            conflict_copy_path(&r),
            format!(".bashrc.sync-conflict-20240229-131415-{dev}")
        );
        r.path = "a.b/archive.tar.gz".into();
        assert_eq!(
            conflict_copy_path(&r),
            format!("a.b/archive.tar.sync-conflict-20240229-131415-{dev}.gz")
        );
    }

    #[test]
    fn apply_order_dirs_then_files_then_deletes_bottom_up() {
        let mk = |path: &str, kind, del| FileRecord {
            path: path.into(),
            kind,
            deleted: del,
            ..file(b"", 1, &[(1, 1)])
        };
        let mut actions = [
            Action::Apply(mk("a/b", EntryKind::Dir, true)),
            Action::Apply(mk("a/b/c", EntryKind::File, true)),
            Action::Apply(mk("z", EntryKind::File, false)),
            Action::Apply(mk("x/y", EntryKind::Dir, false)),
            Action::Apply(mk("x", EntryKind::Dir, false)),
        ];
        actions.sort_by(apply_order);
        let order: Vec<_> = actions.iter().map(|a| a.record().path.as_str()).collect();
        assert_eq!(order, vec!["x", "x/y", "z", "a/b/c", "a/b"]);
    }
}
