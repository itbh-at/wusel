// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import FileProvider
import Foundation
import UniformTypeIdentifiers

/// An `NSFileProviderItem` built from a wire `node`/`entry`. The replicated
/// model reads these to populate its own store, so the fields here are exactly
/// what Finder shows: name, kind, size, modification time, and a version that
/// changes when the object does.
final class WuselItem: NSObject, NSFileProviderItem, NSFileProviderItemDecorating {
    /// The Finder emblem shown on offline-available (pinned) items. Must match the
    /// `Identifier` under `NSFileProviderDecorations` in the extension's Info.plist.
    static let offlineDecoration = NSFileProviderItemDecorationIdentifier(
        rawValue: "at.itbh.wusel.decoration.offline")

    /// The emblem shown on a pinned item whose kept copy is out of date (the
    /// server has a newer version). Replaces the plain offline check, so an
    /// outdated offline copy is visible at a glance.
    static let staleDecoration = NSFileProviderItemDecorationIdentifier(
        rawValue: "at.itbh.wusel.decoration.stale")

    // No Team/Group-folder emblem: a File Provider decoration badge needs a UTI
    // conforming to `com.apple.icon-decoration.badge`, Apple ships no team glyph,
    // and a custom-UTI badge does not render as a clean overlay and degrades
    // items to a "!" state. `folderKind` is still tracked (below) for when a
    // viable badge exists; it just drives no decoration today.

    let itemIdentifier: NSFileProviderItemIdentifier
    let parentItemIdentifier: NSFileProviderItemIdentifier
    let filename: String

    private let isDirectory: Bool
    private let size: UInt64
    private let mtime: Int64
    private let versionToken: Data
    /// Whether the item is kept offline (pinned). Drives the keep-downloaded
    /// content policy and the offline emblem.
    private let pinned: Bool
    /// Whether the kept copy is out of date (only meaningful with `pinned`).
    /// Switches the emblem from the offline check to the "outdated" warning.
    private let stale: Bool
    /// The engine's full sync state, when it reports one. `pinned` alone cannot
    /// say whether the promised copy is actually here — `pinned-pending` is the
    /// state that can — so the emblem reads this where it exists.
    private let state: SyncState?
    /// Whether this item is a Team/Group folder root. Drives its own emblem and
    /// is folded into the metadata version, so becoming (or ceasing to be) one
    /// re-renders.
    private let folderKind: FolderKind

    init(
        itemIdentifier: NSFileProviderItemIdentifier,
        parentItemIdentifier: NSFileProviderItemIdentifier,
        filename: String,
        isDirectory: Bool,
        size: UInt64,
        mtime: Int64,
        version: String,
        pinned: Bool = false,
        stale: Bool = false,
        state: SyncState? = nil,
        folderKind: FolderKind = .plain
    ) {
        self.itemIdentifier = itemIdentifier
        self.parentItemIdentifier = parentItemIdentifier
        self.filename = filename
        self.isDirectory = isDirectory
        self.size = size
        self.mtime = mtime
        // A version token must never be empty. It is the object's ETag — for a
        // listing as much as a stat, so the two agree — and `"0"` only for an
        // object with no ETag yet (a deferred create, not on the server).
        self.versionToken = Data((version.isEmpty ? "0" : version).utf8)
        self.pinned = pinned
        self.stale = stale
        self.state = state
        self.folderKind = folderKind
        super.init()
    }

    convenience init(node: NodeInfo, path: String) {
        self.init(
            itemIdentifier: ItemMapping.identifier(forPath: path),
            parentItemIdentifier: ItemMapping.identifier(forPath: ItemMapping.parentPath(of: path)),
            filename: node.name,
            isDirectory: node.isDir,
            size: node.size,
            mtime: node.mtime,
            version: node.etag,
            pinned: node.pinned,
            stale: node.stale,
            state: node.state,
            folderKind: node.folderKind)
    }

    convenience init(entry: Entry, parentPath: String) {
        let path = ItemMapping.childPath(parent: parentPath, name: entry.name)
        self.init(
            itemIdentifier: ItemMapping.identifier(forPath: path),
            parentItemIdentifier: ItemMapping.identifier(forPath: parentPath),
            filename: entry.name,
            isDirectory: entry.isDir,
            size: entry.size,
            mtime: entry.mtime,
            // The ETag, exactly as `init(node:)` uses it, so a listed item and a
            // stat'd/fetched one report one content version — otherwise a
            // downloaded file re-listed here looked stale and kept a cloud badge.
            version: entry.etag,
            pinned: entry.pinned,
            stale: entry.stale,
            state: entry.state,
            folderKind: entry.folderKind)
    }

    /// The account root, which the File Provider addresses as `.rootContainer`.
    static func root() -> WuselItem {
        WuselItem(
            itemIdentifier: .rootContainer,
            parentItemIdentifier: .rootContainer,
            filename: "Wusel",
            isDirectory: true,
            size: 0,
            mtime: 0,
            version: "root")
    }

    var contentType: UTType { isDirectory ? .folder : .data }

    var capabilities: NSFileProviderItemCapabilities {
        isDirectory
            ? [.allowsReading, .allowsAddingSubItems, .allowsContentEnumerating,
               .allowsRenaming, .allowsReparenting, .allowsDeleting]
            : [.allowsReading, .allowsWriting, .allowsRenaming, .allowsReparenting,
               .allowsDeleting]
    }

    var documentSize: NSNumber? { isDirectory ? nil : NSNumber(value: size) }

    var contentModificationDate: Date? { Date(timeIntervalSince1970: TimeInterval(mtime)) }

    var itemVersion: NSFileProviderItemVersion {
        // Fold the pinned and stale state into the metadata version: they change
        // no etag, so without this the version is identical before and after and
        // the system treats a reported pin/unpin (or a freshly-stale file) as
        // "nothing changed" and does not re-render the emblem (most visibly on a
        // folder, whose unpin otherwise looked like a no-op). The content version
        // stays the etag — the bytes did not change, so nothing re-downloads.
        var metadata = versionToken
        metadata.append(pinned ? 0x01 : 0x00)
        metadata.append(stale ? 0x01 : 0x00)
        // The promise and the copy are different facts, and only the state tells
        // them apart: without this byte, the moment a pending pin finally has its
        // bytes is not a metadata change and the emblem does not appear.
        metadata.append(offlineCopyIsHere ? 0x01 : 0x00)
        metadata.append(folderKind == .groupFolder ? 0x01 : 0x00)
        return NSFileProviderItemVersion(contentVersion: versionToken, metadataVersion: metadata)
    }

    /// Whether the offline promise is actually redeemed — pinned *and* the bytes
    /// are here. `pinned-pending` is the engine's word for a pin whose content is
    /// still to come, which a directory pin produces routinely: it covers files
    /// the server grows afterwards, so the promise runs ahead of the copy.
    ///
    /// Where the engine reports no state (an older serve, or an object that has
    /// none) the flag alone decides, as before.
    private var offlineCopyIsHere: Bool {
        guard let state else { return pinned }
        return state != .pinnedPending
    }

    /// Show the outdated emblem on a stale pinned item, otherwise the plain
    /// offline check on a pinned item whose copy is *here*; nothing on an
    /// on-demand item, and nothing on a pin still waiting for its bytes —
    /// "Available Offline" on a file that is not down is the very claim
    /// `pinned-pending` exists to stop. macOS's own not-downloaded indicator
    /// already covers that case, and the item still declares
    /// `downloadEagerlyAndKeepDownloaded`, so the check appears once the copy
    /// lands. `stale` already implies `pinned` (see the wire), so one badge is
    /// shown, not both.
    var decorations: [NSFileProviderItemDecorationIdentifier]? {
        if stale { return [Self.staleDecoration] }
        if pinned && offlineCopyIsHere { return [Self.offlineDecoration] }
        return nil
    }

    /// A pinned item is downloaded eagerly and kept downloaded; everything else
    /// inherits the domain default (download on demand). This is the macOS side of
    /// "make available offline" — the File Provider keeps a local copy — alongside
    /// the engine-side pin the action records.
    var contentPolicy: NSFileProviderContentPolicy {
        pinned ? .downloadEagerlyAndKeepDownloaded : .inherited
    }

}
