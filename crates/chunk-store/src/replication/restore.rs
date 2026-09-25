use std::{
    fs::{self, File},
    path::Path,
};

use super::{
    ObjectStorage,
    segment::{self, Object},
};
use crate::{Error, Result, sqlite::log::Replica};

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

/// Restores the newest epoch's latest snapshot and its later segments into
/// `path`, under the next unused epoch. Leaves `path` untouched when storage has
/// no snapshot or anything fails.
pub(crate) fn restore(path: &Path, environment: &str, storage: &dyn ObjectStorage, remote: &Remote) -> Result<()> {
    let snapshots = remote.objects.iter().filter(|object| matches!(object, Object::Snapshot { .. }));
    let Some(epoch) = snapshots.map(Object::epoch).max() else {
        return Ok(());
    };
    let latest = remote.latest_epoch().unwrap_or(epoch);
    let base = remote.base(epoch).unwrap_or(0);
    let temporary = super::sibling(path, "restore");
    let result = (|| {
        fs::write(&temporary, storage.get(&segment::snapshot_key(epoch, base))?)?;
        let mut replica = Replica::open(&temporary, environment, epoch, base)?;
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
        replica.finish(latest + 1)?;
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
