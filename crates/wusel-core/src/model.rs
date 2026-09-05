// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Domain types: what the server knows about a file (a PROPFIND entry and its
//! permission letters).

/// An entry from a WebDAV PROPFIND (file or directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    /// Server-side path relative to the user root, e.g. `Documents/foo.txt`.
    pub path: String,
    pub is_dir: bool,
    /// Size in bytes (0 for directories).
    pub size: u64,
    /// The server's ETag — the basis of change detection.
    pub etag: String,
    /// Last modification (Unix seconds), from `getlastmodified`.
    pub mtime: i64,
    /// Nextcloud-internal file ID (`oc:fileid`), stable across renames.
    pub file_id: Option<u64>,
    /// Raw Nextcloud permission letters (`oc:permissions`), e.g. `"RGDNVW"`.
    /// Empty if the server did not report them. See [`is_writable`].
    pub permissions: String,
    /// What kind of mount this entry sits on (`nc:mount-type`): empty for an
    /// ordinary folder in the user's own storage, `group` for a Team/Group
    /// folder, `shared` for a received share, `external`/`external-session`
    /// for external storage, `collective` for the Collectives app. Empty if
    /// the server did not report it. See [`is_group_folder_root`].
    pub mount_type: String,
    /// Whether this entry is the *root* of that mount (`nc:is-mount-root`)
    /// rather than something inside it. Nextcloud sets `mount-type` on every
    /// node within a mount, so this is what separates the folder itself from
    /// its contents. Absent before Nextcloud 28, where it reads as `false`.
    pub is_mount_root: bool,
}

/// The account's storage quota, from a WebDAV `PROPFIND` on the account root
/// (`quota-used-bytes` / `quota-available-bytes`) — the same properties the
/// official client reads for its storage bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Quota {
    /// Bytes already used.
    pub used: u64,
    /// Bytes still free, or `None` if the server did not report a usable
    /// number. Nextcloud encodes "unlimited" and "quota not yet computed" as
    /// negative values rather than omitting the property, and a negative
    /// value fails to parse as a `u64` — which collapses both of those (and
    /// anything else non-numeric) into "we don't actually know", rather than
    /// risking a wrong guess at which sentinel means what.
    pub available: Option<u64>,
}

/// The last path segment of a server-relative path — a child's own name.
///
/// One definition for the whole crate: state and provider both need it, and both
/// used to spell it `rsplit('/').next().unwrap_or(path)` — whose fallback is dead
/// code, since a split always yields at least one item.
pub fn basename(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// Whether Nextcloud's permission letters allow modifying this entry.
///
/// The relevant letters: `W` (update a file's content), `C`/`K` (create files or
/// folders inside a directory). An empty string means the server did not report
/// permissions, so we assume writable (the mount's read-only flag still governs).
pub fn is_writable(permissions: &str, is_dir: bool) -> bool {
    if permissions.is_empty() {
        return true;
    }
    if is_dir {
        permissions.contains('C') || permissions.contains('K') || permissions.contains('W')
    } else {
        permissions.contains('W')
    }
}

/// The `nc:mount-type` value Nextcloud gives a Team/Group folder. Unchanged by
/// the "Group folders" → "Team folders" rename, which touched only UI strings.
pub const MOUNT_TYPE_GROUP: &str = "group";

/// Whether this entry is the folder a Team/Group folder is mounted *at* — the
/// one worth marking in a file manager.
///
/// Both halves matter. `mount-type` alone is true for everything *inside* the
/// folder as well, so marking on that alone would badge every file in it; and
/// `is-mount-root` alone says nothing about what kind of mount it is. An older
/// server (before Nextcloud 28) omits `is-mount-root`, which reads as `false`
/// here — no marking rather than a wrong one, which is the right way to be
/// wrong.
///
/// `oc:permissions` cannot answer this: a group folder carries `M` (mounted)
/// and no `S` (shared) — but so does external storage.
#[must_use]
pub fn is_group_folder_root(mount_type: &str, is_mount_root: bool) -> bool {
    is_mount_root && mount_type == MOUNT_TYPE_GROUP
}

// NOTE: earlier revisions kept `Hydration`/`SyncState` enums here, mirrored into
// `hydration`/`sync` DB columns. Both turned out to be dead modelling: the real
// cache state lives in blob sidecar files (see `crate::content`) and the
// user-visible state is `crate::provider::FileState`. The DB columns remain in
// the schema for compatibility with existing databases, but are no longer
// mapped anywhere.

#[cfg(test)]
mod tests {
    use super::{basename, is_writable};

    #[test]
    fn basename_takes_the_last_segment() {
        assert_eq!(basename("Docs/Sub/notes.txt"), "notes.txt");
        assert_eq!(basename("notes.txt"), "notes.txt", "no separator at all");
        assert_eq!(basename(""), "");
        assert_eq!(basename("Docs/"), "", "a trailing separator ends the path");
    }

    #[test]
    fn permission_letters_map_to_writability() {
        // A writable file (W) vs a read-only share (no W).
        assert!(is_writable("RGDNVW", false));
        assert!(!is_writable("GR", false));
        // Directories: create-file (C) or create-folder (K) count as writable.
        assert!(is_writable("RGDNVCK", true));
        assert!(!is_writable("G", true));
        // Unknown (empty) → assume writable; the RO mount still governs.
        assert!(is_writable("", false));
        assert!(is_writable("", true));
    }
}
