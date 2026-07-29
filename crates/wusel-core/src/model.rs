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
