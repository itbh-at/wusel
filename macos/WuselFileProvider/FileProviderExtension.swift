// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import FileProvider
import Foundation

/// The macOS File Provider frontend: an `NSFileProviderReplicatedExtension` that
/// maps Finder's callbacks onto the engine's socket protocol (`wusel-ipc`). It
/// owns no state — macOS suspends and kills the extension at will — so every
/// method opens a short-lived connection to the agent's `wusel serve` and lets
/// the engine be the single source of truth.
///
/// The mapping mirrors the intent table in documentation/.../frontends.adoc:
///
///   item/enumerator  -> stat / enumerate
///   fetchContents    -> fetch
///   createItem       -> create (+ write + publish for a file's contents)
///   modifyItem       -> move and/or write + publish
///   deleteItem       -> remove
final class FileProviderExtension: NSObject, NSFileProviderReplicatedExtension, NSFileProviderCustomAction {
    private let domain: NSFileProviderDomain
    private let queue = DispatchQueue(label: "at.itbh.wusel.fileprovider", attributes: .concurrent)

    /// The context-menu action identifiers, matched against the declarations in
    /// the extension's Info.plist (`NSExtensionFileProviderActions`).
    private enum Action {
        static let pin = "at.itbh.wusel.action.makeAvailableOffline"
        static let unpin = "at.itbh.wusel.action.removeOfflineAvailability"
    }

    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        super.init()
    }

    func invalidate() {}

    // MARK: - Read

    func item(
        for identifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        queue.async {
            defer { progress.completedUnitCount = 1 }
            if identifier == .rootContainer {
                completionHandler(WuselItem.root(), nil)
                return
            }
            let path = ItemMapping.path(for: identifier)
            do {
                let client = try Engine.connect()
                switch try client.call(Request(op: "stat", path: path)).0 {
                case .node(let node):
                    completionHandler(WuselItem(node: node, path: path), nil)
                case .error(let error):
                    completionHandler(nil, Engine.nsError(error))
                default:
                    completionHandler(nil, Engine.nsError(SocketClient.SocketError.unexpectedResponse))
                }
            } catch {
                completionHandler(nil, Engine.nsError(error))
            }
        }
        return progress
    }

    func fetchContents(
        for itemIdentifier: NSFileProviderItemIdentifier,
        version requestedVersion: NSFileProviderItemVersion?,
        request: NSFileProviderRequest,
        completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        queue.async {
            defer { progress.completedUnitCount = progress.totalUnitCount }
            let path = ItemMapping.path(for: itemIdentifier)
            do {
                let client = try Engine.connect()
                guard case .node(let node) = try client.call(Request(op: "stat", path: path)).0 else {
                    completionHandler(nil, nil, Engine.nsError(WireError.notFound))
                    return
                }
                // Fetch in chunks streamed straight to the file. A single wire
                // frame is capped at 8 MiB, so a whole-file fetch fails on
                // anything larger; request 4 MiB windows and append each.
                let temp = FileManager.default.temporaryDirectory
                    .appendingPathComponent(UUID().uuidString)
                FileManager.default.createFile(atPath: temp.path, contents: nil)
                let handle = try FileHandle(forWritingTo: temp)
                progress.totalUnitCount = Int64(max(node.size, 1))
                let chunk: UInt32 = 4 * 1024 * 1024
                var offset: UInt64 = 0
                while offset < node.size {
                    let want = UInt32(min(UInt64(chunk), node.size - offset))
                    let (response, body) = try client.call(
                        Request(op: "fetch", path: path, offset: offset, len: want))
                    guard case .bytes = response, let bytes = body else {
                        try? handle.close()
                        completionHandler(nil, nil, Engine.nsError(SocketClient.SocketError.unexpectedResponse))
                        return
                    }
                    if bytes.isEmpty { break }  // EOF guard against a short file
                    handle.write(bytes)
                    offset += UInt64(bytes.count)
                    progress.completedUnitCount = Int64(offset)
                }
                try? handle.close()
                completionHandler(temp, WuselItem(node: node, path: path), nil)
            } catch {
                completionHandler(nil, nil, Engine.nsError(error))
            }
        }
        return progress
    }

    // MARK: - Write

    func createItem(
        basedOn itemTemplate: NSFileProviderItem,
        fields: NSFileProviderItemFields,
        contents url: URL?,
        options: NSFileProviderCreateItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        queue.async {
            defer { progress.completedUnitCount = 1 }
            let parent = ItemMapping.path(for: itemTemplate.parentItemIdentifier)
            let path = ItemMapping.childPath(parent: parent, name: itemTemplate.filename)
            let isDir = itemTemplate.contentType == .folder
            do {
                let client = try Engine.connect()
                guard case .node = try client.call(Request(op: "create", path: path, dir: isDir)).0 else {
                    completionHandler(nil, [], false, Engine.nsError(WireError.io))
                    return
                }
                if !isDir, let url = url {
                    try Self.upload(client, path: path, from: url)
                }
                completionHandler(try Self.statItem(client, path: path), [], false, nil)
            } catch {
                completionHandler(nil, [], false, Engine.nsError(error))
            }
        }
        return progress
    }

    func modifyItem(
        _ item: NSFileProviderItem,
        baseVersion version: NSFileProviderItemVersion,
        changedFields: NSFileProviderItemFields,
        contents newContents: URL?,
        options: NSFileProviderModifyItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        queue.async {
            defer { progress.completedUnitCount = 1 }
            var path = ItemMapping.path(for: item.itemIdentifier)
            do {
                let client = try Engine.connect()
                // A rename or a move: both ends are named by the desired item.
                if changedFields.contains(.filename) || changedFields.contains(.parentItemIdentifier) {
                    let newParent = ItemMapping.path(for: item.parentItemIdentifier)
                    let newPath = ItemMapping.childPath(parent: newParent, name: item.filename)
                    if newPath != path {
                        _ = try client.call(Request(op: "move", path: path, to: newPath))
                        path = newPath
                    }
                }
                // A content change: overwrite the buffer and publish it.
                if changedFields.contains(.contents), let url = newContents {
                    try Self.upload(client, path: path, from: url)
                }
                completionHandler(try Self.statItem(client, path: path), [], false, nil)
            } catch {
                completionHandler(nil, [], false, Engine.nsError(error))
            }
        }
        return progress
    }

    func deleteItem(
        identifier: NSFileProviderItemIdentifier,
        baseVersion version: NSFileProviderItemVersion,
        options: NSFileProviderDeleteItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        queue.async {
            defer { progress.completedUnitCount = 1 }
            let path = ItemMapping.path(for: identifier)
            do {
                let client = try Engine.connect()
                switch try client.call(Request(op: "remove", path: path)).0 {
                case .error(.notFound):
                    // Already gone — e.g. a stale replica entry the engine no
                    // longer has. A delete is idempotent, so this is success;
                    // reporting an error here makes Finder retry forever and the
                    // deletion sheet hangs at "0 items".
                    completionHandler(nil)
                case .error(let error):
                    completionHandler(Engine.nsError(error))
                default:
                    completionHandler(nil)
                }
            } catch {
                completionHandler(Engine.nsError(error))
            }
        }
        return progress
    }

    // MARK: - Enumeration

    func enumerator(
        for containerItemIdentifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest
    ) throws -> NSFileProviderEnumerator {
        Enumerator(
            identifier: containerItemIdentifier,
            path: ItemMapping.path(for: containerItemIdentifier),
            domain: domain)
    }

    // MARK: - Custom actions (Finder context menu)

    /// Handle "Make available offline" / "Remove download" from Finder's
    /// right-click menu. Each maps to the engine's pin store over the socket — the
    /// same store `wusel pin` and the Linux frontend use — so the choice is
    /// remembered and, for a pin, the content is downloaded now and kept offline.
    ///
    /// Runs each item in turn (a pin can be slow — it hydrates), reports the first
    /// failure, and signals the working set so Finder re-reads the new state.
    func performAction(
        identifier actionIdentifier: NSFileProviderExtensionActionIdentifier,
        onItemsWithIdentifiers itemIdentifiers: [NSFileProviderItemIdentifier],
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: Int64(max(itemIdentifiers.count, 1)))
        queue.async { [self] in
            let pinning: Bool
            switch actionIdentifier.rawValue {
            case Action.pin: pinning = true
            case Action.unpin: pinning = false
            default:
                completionHandler(Engine.nsError(WireError.badRequest))
                return
            }
            var firstError: Error?
            var acted: [String] = []
            do {
                let client = try Engine.connect()
                for identifier in itemIdentifiers {
                    let path = ItemMapping.path(for: identifier)
                    do {
                        if pinning {
                            try client.pin(path)
                        } else {
                            try client.unpin(path)
                        }
                        // Refresh the acted item and, for a folder, its contents:
                        // pinning/unpinning a folder flips every descendant's
                        // offline state (the engine covers a subtree by the folder
                        // pin), so their emblems must update too, not just the
                        // folder's.
                        acted.append(path)
                        acted.append(contentsOf: Self.descendantPaths(client, of: path))
                    } catch {
                        if firstError == nil { firstError = Engine.nsError(error) }
                    }
                    progress.completedUnitCount += 1
                }
            } catch {
                firstError = Engine.nsError(error)
            }
            // Make Finder re-read the acted items' state (the offline emblem and
            // the keep-downloaded policy). A local pin emits no engine change, so
            // the working-set change enumerator would not otherwise see them:
            // record the paths for it to report, then signal the working set.
            PinRefresh.add(acted)
            NSFileProviderManager(for: domain)?.signalEnumerator(for: .workingSet) { _ in }
            completionHandler(firstError)
        }
        return progress
    }

    // MARK: - Helpers

    /// Every path under `path` (recursively), so a folder pin/unpin can refresh
    /// its contents' emblems. Empty for a file (enumerating one errors). Bounded
    /// by depth and a total cap: the listings are already cached after the pin, so
    /// this is local, but a pathological tree must not stall the action — anything
    /// past the cap simply refreshes on next navigation. Best-effort: a listing
    /// that fails is skipped, not fatal.
    private static func descendantPaths(_ client: SocketClient, of path: String) -> [String] {
        let maxTotal = 2000
        let maxDepth = 24
        var result: [String] = []
        func walk(_ dir: String, _ depth: Int) {
            guard depth < maxDepth, result.count < maxTotal else { return }
            guard case .entries(let entries)? = try? client.call(Request(op: "enumerate", path: dir)).0
            else { return }
            for entry in entries where result.count < maxTotal {
                let childPath = ItemMapping.childPath(parent: dir, name: entry.name)
                result.append(childPath)
                if entry.isDir { walk(childPath, depth + 1) }
            }
        }
        walk(path, 0)
        return result
    }

    /// Upload a local file to `path` in 4 MiB `write` chunks, then publish. A wire
    /// frame is capped at 8 MiB, so writing a whole large file in one frame is
    /// rejected by the server and the upload silently fails; chunking fixes it.
    private static func upload(_ client: SocketClient, path: String, from url: URL) throws {
        // Replace, do not overlay. `modifyItem`/`createItem` always hand over the
        // *complete* new contents, so the destination must be emptied first. The
        // engine seeds a write buffer from the server's current version (a later
        // three-way merge is made of it) and a positional `write` never shrinks
        // it — so without this a shrinking overwrite (`printf x > big.bin`, or a
        // "Replace" save of a smaller document) would publish the new head
        // followed by the previous file's stale tail: a corrupt file of the
        // original length. `setattr(size: 0)` is the File Provider counterpart of
        // the `O_TRUNC` a POSIX editor issues on the FUSE frontend, which the
        // engine turns into a fresh empty buffer instead of hydrating the old
        // bytes it is about to discard.
        //
        // A failed truncate must abort the upload: `call` reports a wire error in
        // the response rather than throwing, and swallowing it here would let the
        // writes overlay a non-emptied buffer — the very corruption this guards
        // against. The system retries `modifyItem` with the full contents later.
        if case .error(let e) = try client.call(Request(op: "setattr", path: path, size: 0)).0 {
            throw e
        }
        let handle = try FileHandle(forReadingFrom: url)
        defer { try? handle.close() }
        let chunk = 4 * 1024 * 1024
        var offset: UInt64 = 0
        while true {
            let data = handle.readData(ofLength: chunk)
            if data.isEmpty { break }
            _ = try client.callWrite(Request(op: "write", path: path, offset: offset), data: data)
            offset += UInt64(data.count)
        }
        // A zero-byte source still needs a buffer for publish to have something.
        if offset == 0 {
            _ = try client.callWrite(Request(op: "write", path: path, offset: 0), data: Data())
        }
        _ = try client.call(Request(op: "publish", path: path))
    }

    /// `stat` a path and turn it into an item, for the create/modify replies.
    private static func statItem(_ client: SocketClient, path: String) throws -> WuselItem {
        if case .node(let node) = try client.call(Request(op: "stat", path: path)).0 {
            return WuselItem(node: node, path: path)
        }
        throw WireError.io
    }
}
