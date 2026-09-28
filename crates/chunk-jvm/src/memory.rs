//! The memory this machine lets the runner use.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// The memory visible under `proc` (normally `/proc`), in MiB: the lower of the lowest cgroup v2 `memory.max` from this
/// process's cgroup up to the hierarchy's mount, and `MemTotal`, whichever are known.
pub(crate) fn visible_mib(proc: &Path) -> Option<u64> {
    let total = fs::read_to_string(proc.join("meminfo")).ok().and_then(|meminfo| {
        let line = meminfo.lines().find_map(|line| line.strip_prefix("MemTotal:"))?;
        line.trim().strip_suffix("kB")?.trim().parse::<u64>().ok().map(|kib| kib * 1024)
    });
    [cgroup_limit(proc), total].into_iter().flatten().min().map(|bytes| bytes / (1024 * 1024))
}

fn cgroup_limit(proc: &Path) -> Option<u64> {
    let (mount, directory) = cgroup(proc)?;
    directory
        .ancestors()
        .take_while(|directory| directory.starts_with(&mount))
        .filter_map(|directory| fs::read_to_string(directory.join("memory.max")).ok()?.trim().parse::<u64>().ok())
        .min()
}

/// Where the cgroup v2 hierarchy is mounted, and this process's cgroup directory under it.
fn cgroup(proc: &Path) -> Option<(PathBuf, PathBuf)> {
    let membership = fs::read_to_string(proc.join("self/cgroup")).ok()?;
    let path = Path::new(membership.lines().find_map(|line| line.strip_prefix("0::"))?);
    let mounts = fs::read_to_string(proc.join("self/mountinfo")).ok()?;
    mounts.lines().find_map(|line| {
        let (mount, filesystem) = line.split_once(" - ")?;
        if filesystem.split(' ').next()? != "cgroup2" {
            return None;
        }
        let mut fields = mount.split(' ').skip(3);
        let (root, point) = (fields.next()?, PathBuf::from(fields.next()?));
        let directory = point.join(path.strip_prefix(root).ok()?);
        Some((point, directory))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn the_lowest_limit_from_the_processs_cgroup_up_to_its_mount_bounds_memory() {
        let directory = tempfile::tempdir().unwrap();
        let (proc, mount) = (directory.path().join("proc"), directory.path().join("cgroup"));
        let service = mount.join("chunk.service");
        fs::create_dir_all(proc.join("self")).unwrap();
        fs::create_dir_all(&service).unwrap();
        assert_eq!(visible_mib(&proc), None);
        fs::write(proc.join("meminfo"), "MemFree:  100 kB\nMemTotal:       4194304 kB\n").unwrap();
        assert_eq!(visible_mib(&proc), Some(4096));

        fs::write(proc.join("self/cgroup"), "0::/system.slice/chunk.service\n").unwrap();
        let mounts = format!(
            "22 1 0:21 / /proc rw - proc proc rw\n35 24 0:30 /system.slice {} rw - cgroup2 cgroup2 rw\n",
            mount.display()
        );
        fs::write(proc.join("self/mountinfo"), mounts).unwrap();
        fs::write(service.join("memory.max"), "max\n").unwrap();
        assert_eq!(visible_mib(&proc), Some(4096));
        fs::write(mount.join("memory.max"), format!("{}\n", 2 * GIB)).unwrap();
        fs::write(directory.path().join("memory.max"), format!("{}\n", GIB / 4)).unwrap();
        assert_eq!(visible_mib(&proc), Some(2048));
        fs::write(service.join("memory.max"), format!("{GIB}\n")).unwrap();
        assert_eq!(visible_mib(&proc), Some(1024));
    }
}
