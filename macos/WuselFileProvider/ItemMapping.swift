// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import FileProvider

/// The bridge between the File Provider's opaque item identifiers and the
/// engine's account-relative paths. The socket protocol addresses objects by
/// path; Finder addresses them by `NSFileProviderItemIdentifier`. For this
/// skeleton the identifier *is* the path (with the root as the well-known
/// `.rootContainer`).
///
/// A later slice should switch to Nextcloud's stable `file_id` as the identity —
/// a rename keeps the id but changes the path, and a File Provider identifier is
/// expected to survive a rename. The path-as-identity used here is enough to
/// prove the callback wiring end to end.
enum ItemMapping {
    static func path(for id: NSFileProviderItemIdentifier) -> String {
        id == .rootContainer ? "/" : id.rawValue
    }

    static func identifier(forPath path: String) -> NSFileProviderItemIdentifier {
        (path == "/" || path.isEmpty) ? .rootContainer : NSFileProviderItemIdentifier(path)
    }

    /// Join a parent path and a child name into an account-relative path.
    ///
    /// The name is first normalised to Unicode **NFC** (precomposed). macOS hands
    /// filenames to the File Provider in **NFD** (decomposed — e.g. "Ö" as `O` +
    /// U+0308 combining diaeresis), whereas Nextcloud stores and returns them in
    /// NFC. A locally created name that reached the engine as NFD would never
    /// match the server's NFC name on the `(parent, name)` key: the PROPFIND
    /// reconcile inserts the server's node as a *second* row, and the NFD
    /// deferred row (file id still null) is exempt from the "vanished from the
    /// server" cleanup — so every accented filename created here grew a permanent
    /// ghost twin. Normalising at this single choke point — the one place a
    /// macOS-supplied name becomes an engine path — keeps the frontend speaking
    /// the same NFC the server does. It is idempotent on names the engine hands
    /// back (already NFC), so it is safe in both directions of the mapping.
    static func childPath(parent: String, name: String) -> String {
        let name = name.precomposedStringWithCanonicalMapping
        return parent == "/" ? "/" + name : parent + "/" + name
    }

    /// The parent of a path (`/a/b/c` → `/a/b`; a top-level name → `/`).
    static func parentPath(of path: String) -> String {
        guard let slash = path.lastIndex(of: "/"), slash != path.startIndex else { return "/" }
        return String(path[path.startIndex..<slash])
    }
}
