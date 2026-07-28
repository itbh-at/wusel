// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Guards that keep account mountpoints from clobbering each other.
//!
//! Two accounts must never mount at the same directory (Linux would over-mount
//! and hide the first), nor nest one inside the other. The pure overlap logic
//! lives here (platform-independent, tested); the frontend supplies the list of
//! currently active mounts and which of them are ours.

use std::path::{Path, PathBuf};

/// The first active mount that conflicts with mounting at `target`, if any.
///
/// A conflict is:
/// * `target` **equals or contains** any active mount (mounting there would
///   occupy an existing mount, or swallow one nested below it) — checked against
///   `all_mounts`; and
/// * `target` sits **inside one of our own** mounts (nesting one wusel mount
///   in another) — checked against `our_mounts` only, since being nested under a
///   normal system mount (`/`, `/home`, …) is of course fine.
pub fn find_conflict(
    target: &Path,
    all_mounts: &[PathBuf],
    our_mounts: &[PathBuf],
) -> Option<PathBuf> {
    // `m.starts_with(target)` is true when m == target or m is below target.
    if let Some(m) = all_mounts.iter().find(|m| m.starts_with(target)) {
        return Some(m.clone());
    }
    our_mounts
        .iter()
        .find(|m| target != m.as_path() && target.starts_with(m))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn same_mountpoint_conflicts() {
        let all = vec![p("/"), p("/home"), p("/home/u/NC")];
        let ours = vec![p("/home/u/NC")];
        assert_eq!(
            find_conflict(&p("/home/u/NC"), &all, &ours),
            Some(p("/home/u/NC"))
        );
    }

    #[test]
    fn nesting_inside_our_mount_conflicts() {
        let all = vec![p("/"), p("/home"), p("/home/u/NC")];
        let ours = vec![p("/home/u/NC")];
        assert_eq!(
            find_conflict(&p("/home/u/NC/work"), &all, &ours),
            Some(p("/home/u/NC"))
        );
    }

    #[test]
    fn containing_an_existing_mount_conflicts() {
        // We try to mount the outer dir while a mount already sits inside it.
        let all = vec![p("/"), p("/home"), p("/home/u/NC/work")];
        let ours = vec![p("/home/u/NC/work")];
        assert_eq!(
            find_conflict(&p("/home/u/NC"), &all, &ours),
            Some(p("/home/u/NC/work"))
        );
    }

    #[test]
    fn normal_directory_under_system_mounts_is_fine() {
        // `/` and `/home` are active mounts but nesting under them is normal.
        let all = vec![p("/"), p("/home")];
        let ours: Vec<PathBuf> = vec![];
        assert_eq!(find_conflict(&p("/home/u/Wusel"), &all, &ours), None);
    }

    #[test]
    fn sibling_and_prefix_paths_do_not_conflict() {
        let all = vec![p("/"), p("/home/u/NC")];
        let ours = vec![p("/home/u/NC")];
        assert_eq!(find_conflict(&p("/home/u/Work"), &all, &ours), None);
        // A shared string prefix is not a path-component prefix.
        assert_eq!(find_conflict(&p("/home/u/NCthings"), &all, &ours), None);
    }
}
