use std::time::{Duration, SystemTime};

use super::{Listed, segment::Object};

/// Keys of the snapshots and segments older than the newest snapshot uploaded
/// at least `retention` before `now`. Restores never need them again.
pub(super) fn expired(objects: &[Listed], retention: Duration, now: SystemTime) -> Vec<&str> {
    let parsed: Vec<_> = objects
        .iter()
        .filter_map(|listed| Some((listed.key.as_str(), Object::parse(&listed.key, listed.size)?, listed)))
        .collect();
    let aged = |listed: &Listed| now.duration_since(listed.modified).is_ok_and(|age| age >= retention);
    let Some(newest) = parsed
        .iter()
        .filter_map(|(_, object, listed)| match object {
            Object::Snapshot { epoch, sequence } if aged(listed) => Some((*epoch, *sequence)),
            _ => None,
        })
        .max()
    else {
        return Vec::new();
    };
    parsed
        .into_iter()
        .filter(|(_, object, _)| match *object {
            Object::Snapshot { epoch, sequence } => (epoch, sequence) < newest,
            Object::Segment { epoch, last, .. } => (epoch, last) <= newest,
            Object::Claim { .. } => false,
        })
        .map(|(key, _, _)| key)
        .collect()
}
