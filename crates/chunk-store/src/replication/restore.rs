use std::{
    fs::{self, File},
    io::Write,
    path::Path,
};

use super::{
    ObjectStorage,
    segment::{self, Object},
};
use crate::{
    Error, Result,
    sqlite::{bootstrap, log::Replica},
};

/// Restores that keep finding storage changed after their claim give up after this many attempts.
const RESTORE_ATTEMPTS: usize = 8;

/// The objects already in storage, listed once when a replicated store opens.
pub(crate) struct Remote {
    objects: Vec<Object>,
}

impl Remote {
    pub fn load(storage: &dyn ObjectStorage) -> Result<Self> {
        let listed = storage.list("epochs")?;
        Ok(Self { objects: listed.iter().filter_map(|(key, size)| Object::parse(key, *size)).collect() })
    }

    pub fn latest_epoch(&self) -> Option<u64> {
        self.objects.iter().map(Object::epoch).max()
    }

    /// The newest epoch with a snapshot and the last sequence stored for it: the
    /// history a restore continues.
    pub fn source(&self) -> Option<(u64, u64)> {
        let snapshots = self.objects.iter().filter(|object| matches!(object, Object::Snapshot { .. }));
        let epoch = snapshots.map(Object::epoch).max()?;
        Some((epoch, self.uploaded(epoch)))
    }

    /// The latest snapshot sequence of `epoch`.
    pub fn base(&self, epoch: u64) -> Option<u64> {
        self.objects
            .iter()
            .filter_map(|object| match object {
                Object::Snapshot { epoch: owner, sequence } if *owner == epoch => Some(*sequence),
                _ => None,
            })
            .max()
    }

    /// The last sequence stored for `epoch`, or zero before its first snapshot.
    pub fn uploaded(&self, epoch: u64) -> u64 {
        self.base(epoch).map_or(0, |base| self.segments(epoch).map(|(_, last, _)| last).fold(base, u64::max))
    }

    /// Segments and their bytes stored after the latest snapshot of `epoch`.
    pub fn since_snapshot(&self, epoch: u64) -> (usize, u64) {
        let base = self.base(epoch).unwrap_or(0);
        self.segments(epoch)
            .filter(|(_, last, _)| *last > base)
            .fold((0, 0), |(count, bytes), (_, _, size)| (count + 1, bytes + size))
    }

    fn segments(&self, epoch: u64) -> impl Iterator<Item = (u64, u64, u64)> {
        self.objects.iter().filter_map(move |object| match object {
            Object::Segment { epoch: owner, first, last, size } if *owner == epoch => Some((*first, *last, *size)),
            _ => None,
        })
    }
}

/// Builds `path` from the newest epoch's latest snapshot and its later segments,
/// or as an empty database when storage has no snapshot, under a newly claimed
/// epoch. Leaves `path` untouched when anything fails.
///
/// The claim is only kept if a listing after it shows the same history the
/// restore replayed and no newer claim; otherwise the restore starts over, so it
/// never continues from a history a writer extended or another restore replaced.
pub(crate) fn restore(path: &Path, environment: &str, storage: &dyn ObjectStorage) -> Result<()> {
    install(path, |temporary| {
        for _ in 0..RESTORE_ATTEMPTS {
            let remote = Remote::load(storage)?;
            let _ = fs::remove_file(temporary);
            let replica = rebuild(temporary, environment, storage, &remote)?;
            let token = replica.token()?;
            let claimed = remote.latest_epoch().unwrap_or(0) + 1;
            if !storage.create(&segment::claim_key(claimed), token.clone().into_bytes())? {
                continue;
            }
            let current = Remote::load(storage)?;
            if current.latest_epoch() == Some(claimed) && current.source() == remote.source() {
                return replica.finish(claimed, &token);
            }
        }
        Err(Error::Replication("object storage kept changing during restore".into()))
    })
}

/// Writes the replayed history of `remote` into the new private file `temporary`.
fn rebuild(temporary: &Path, environment: &str, storage: &dyn ObjectStorage, remote: &Remote) -> Result<Replica> {
    let mut file = super::create_private(temporary)?;
    let Some((epoch, _)) = remote.source() else {
        // A new environment, or one whose database was lost before its first upload.
        drop(file);
        drop(bootstrap::open(temporary, environment)?);
        return Replica::open(temporary, environment, 1, 0);
    };
    let base = remote.base(epoch).unwrap_or(0);
    file.write_all(&storage.get(&segment::snapshot_key(epoch, base))?)?;
    drop(file);
    let mut replica = Replica::open(temporary, environment, epoch, base)?;
    let mut segments: Vec<_> = remote.segments(epoch).collect();
    segments.sort_unstable();
    for (first, last, _) in segments {
        if last <= replica.sequence() {
            continue;
        }
        if first > replica.sequence() + 1 {
            return Err(Error::Corrupt("replicated log has a gap"));
        }
        let entries = segment::decode(epoch, &storage.get(&segment::segment_key(epoch, first, last))?)?;
        let applied = replica.sequence();
        for entry in entries.iter().filter(|entry| entry.sequence > applied) {
            replica.replay(entry)?;
        }
    }
    Ok(replica)
}

/// Replaces `path` with the file `build` writes beside it, keeping `path`'s permissions.
fn install(path: &Path, build: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let temporary = super::sibling(path, "restore");
    let result = (|| {
        build(&temporary)?;
        fs::set_permissions(&temporary, fs::metadata(path)?.permissions())?;
        File::open(&temporary)?.sync_all()?;
        fs::rename(&temporary, path)?;
        if let Some(directory) = path.parent() {
            File::open(directory)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
