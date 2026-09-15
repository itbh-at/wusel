// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import FileProvider
import Foundation

/// Enumerates one container.
///
///   * **Full listing** (`enumerateItems`) — Finder needs a folder's contents.
///     Do a complete, blocking load; Finder shows its loading state until done.
///   * **Live change** (`enumerateChanges`) — a server-side change arrived.
///
/// The change channel is the **working set**: on macOS a per-container signal
/// does *not* trigger that container's change enumeration — only
/// `signalEnumerator(.workingSet)` does. So the agent signals the working set,
/// and this reports the delta there. To keep that from touching folders the user
/// has not opened (which would seed partial replicas — "empty then pop" — and,
/// with folders, a prefetch crawl), changes are reported only for **opened**
/// folders (see `KnownContainers`) and only for **files**.
final class Enumerator: NSObject, NSFileProviderEnumerator {
    private let identifier: NSFileProviderItemIdentifier
    private let path: String
    private let isWorkingSet: Bool
    private let domain: NSFileProviderDomain
    private let queue = DispatchQueue(label: "at.itbh.wusel.enumerator")

    init(identifier: NSFileProviderItemIdentifier, path: String, domain: NSFileProviderDomain) {
        self.identifier = identifier
        self.path = path
        self.isWorkingSet = identifier == .workingSet
        self.domain = domain
        super.init()
    }

    func invalidate() {}

    // MARK: - Full listing

    func enumerateItems(
        for observer: NSFileProviderEnumerationObserver,
        startingAt page: NSFileProviderPage
    ) {
        queue.async { [path, isWorkingSet] in
            // Special containers (working set, trash) have no account path.
            guard !isWorkingSet, path.hasPrefix("/") else {
                observer.finishEnumerating(upTo: nil)
                return
            }
            // The user opened this folder: remember it, so live change updates
            // are limited to opened folders (see enumerateChanges).
            KnownContainers.remember(path)
            // A folder's first listing does a live PROPFIND; on a big account the
            // server can hiccup (a transient io error), and Finder then shows the
            // folder empty with a sticky "items may be outdated" banner. Retry a
            // few times — the listing succeeds on a later attempt — before ever
            // surfacing an error. `not_found` is real and reported at once.
            for attempt in 0..<4 {
                do {
                    let client = try Engine.connect()
                    let (response, _) = try client.call(Request(op: "enumerate", path: path))
                    switch response {
                    case .entries(let entries):
                        observer.didEnumerate(
                            Self.deduped(entries).map { WuselItem(entry: $0, parentPath: path) })
                        observer.finishEnumerating(upTo: nil)
                        return
                    case .error(.notFound):
                        observer.finishEnumeratingWithError(Engine.nsError(WireError.notFound))
                        return
                    case .error(let error) where attempt == 3:
                        Engine.log("enumerate(\(path)) -> error \(error) (gave up)")
                        observer.finishEnumeratingWithError(Engine.nsError(error))
                        return
                    case .error:
                        break  // transient — retry
                    default:
                        observer.finishEnumeratingWithError(
                            Engine.nsError(SocketClient.SocketError.unexpectedResponse))
                        return
                    }
                } catch {
                    if attempt == 3 {
                        Engine.log("enumerateItems(\(path)) threw \(error) (gave up)")
                        observer.finishEnumeratingWithError(Engine.nsError(error))
                        return
                    }
                }
                Thread.sleep(forTimeInterval: 0.5)
            }
        }
    }

    // MARK: - Live change (working set only)

    func enumerateChanges(
        for observer: NSFileProviderChangeObserver,
        from anchor: NSFileProviderSyncAnchor
    ) {
        // A regular container: declare the anchor stale so the system re-runs a
        // full enumerateItems and refreshes Finder's view. Finishing with the
        // anchor and no changes tells Finder "nothing changed", so the first-entry
        // view stays empty (the replica is populated, but the window is not
        // re-read) until the user re-navigates. Only the working set carries the
        // incremental deltas.
        guard isWorkingSet else {
            observer.finishEnumeratingWithError(
                NSError(domain: NSFileProviderError.errorDomain,
                        code: NSFileProviderError.syncAnchorExpired.rawValue))
            return
        }
        queue.async {
            let since = Self.sequence(from: anchor)
            do {
                let client = try Engine.connect()
                let (head, changes) = try client.changes(since: since)
                // Recover from a change-log reset. The engine's change log is held
                // in memory and starts again at a low sequence whenever `serve`
                // restarts (a crash, a logout, or — during development — Xcode
                // relaunching the app). `since` is the anchor the *system* persists
                // across those restarts, so `head < since` means the log reset and
                // the deltas between the two are gone for good. Any of them could
                // have been a server-side move or delete that happened while serve
                // was down; that removal can never be replayed, so the replica
                // would keep showing files the server no longer has there (exactly
                // the "stuck listing that in/out navigation won't fix" this guards
                // against). Re-import the opened folders so the system reconciles
                // their contents against the current model. Bounded to the folders
                // the user actually opened and never the whole account (a
                // root-wide reimport once flooded items with "!"); the low new head
                // is still adopted below, so this fires once per reset, not a loop.
                if head < since {
                    // The reimport below deletes and re-creates the folder subtree
                    // in the system's replica; the re-create has to reach the
                    // server. Run against an unreachable one it fails with
                    // `.serverUnreachable`, and the folder is left wedged with a
                    // stuck upload error — a "!" that only a domain reset clears
                    // (confirmed by reproduction). So gate on reachability: skip
                    // when the server is not known reachable, and let the agent
                    // re-run the reconcile once the connection is restored. A
                    // failed query reads as "not reachable" — do not risk it.
                    if (try? client.reachable()) == true {
                        Engine.log("change log reset (head=\(head) < since=\(since)) — reconciling opened folders")
                        KnownContainers.reconcileOpenedFolders(domain: self.domain, log: Engine.log)
                    } else {
                        Engine.log(
                            "change log reset (head=\(head) < since=\(since)) — server unreachable, deferring reconcile")
                    }
                }
                // Report a change only for a child of a folder the user has opened
                // (an unopened folder must not be seeded — it would show without a
                // spinner, then pop content in). Both files and directories are
                // reported: a directory that only appears here — a folder created
                // on the server while its parent was already open — never surfaces
                // otherwise, because the parent's `enumerateItems` does not re-run
                // on its own and this is the only change channel macOS serves. This
                // does not crawl. The engine emits an `Entry` change on a folder's
                // own path only when the folder itself is created, removed, renamed,
                // or its availability flips in its parent — never merely because
                // content changed somewhere below it (that lands as a change on the
                // leaf's path, gated to opened parents above). And we only `stat`
                // the item, never `enumerate` it — the runaway that once flooded the
                // account came from enumerating a directory in this channel, not
                // from reporting one.
                let known = KnownContainers.snapshot()
                var seen = Set<String>()
                var updated: [NSFileProviderItem] = []
                var deleted: [NSFileProviderItemIdentifier] = []
                for change in changes {
                    let itemPath = Self.normalized(change.path)
                    guard seen.insert(itemPath).inserted else { continue }
                    guard known.contains(ItemMapping.parentPath(of: itemPath)) else { continue }
                    let id = ItemMapping.identifier(forPath: itemPath)
                    let statResult: Response
                    do {
                        statResult = try client.call(Request(op: "stat", path: itemPath)).0
                    } catch {
                        // A transient failure on one item must not fail the whole
                        // change enumeration (that shows a sticky "outdated" banner).
                        Engine.log("enumerateChanges: stat(\(itemPath)) threw \(error) — skipping")
                        continue
                    }
                    switch statResult {
                    case .node(let node):
                        updated.append(WuselItem(node: node, path: itemPath))
                    case .error(.notFound):
                        deleted.append(id)
                    default:
                        break
                    }
                }
                // Items the user just pinned or unpinned. A local pin/unpin emits
                // no server change, so the loop above never sees the acted item —
                // report it here so its offline emblem updates at once, and clears
                // again on unpin. Unlike the change loop this reports a **folder**
                // too (the file-only rule there guards against a prefetch crawl
                // from server changes; an explicit pin is bounded and, for a
                // folder, download-its-contents is exactly the intent).
                for itemPath in PinRefresh.drain() {
                    guard seen.insert(itemPath).inserted else { continue }
                    do {
                        switch try client.call(Request(op: "stat", path: itemPath)).0 {
                        case .node(let node):
                            updated.append(WuselItem(node: node, path: itemPath))
                        case .error(.notFound):
                            deleted.append(ItemMapping.identifier(forPath: itemPath))
                        default:
                            break
                        }
                    } catch {
                        Engine.log("enumerateChanges: pin-refresh stat(\(itemPath)) threw \(error)")
                    }
                }
                Engine.log(
                    "enumerateChanges(workingSet) since=\(since) -> \(updated.count) updated, "
                        + "\(deleted.count) deleted, head=\(head)")
                if !deleted.isEmpty { observer.didDeleteItems(withIdentifiers: deleted) }
                if !updated.isEmpty { observer.didUpdate(updated) }
                observer.finishEnumeratingChanges(upTo: Self.anchor(from: head), moreComing: false)
            } catch {
                // A transient failure of the change pull must not raise Finder's
                // "items may be outdated" banner. Report no changes and keep the
                // anchor; the next signal retries from the same point.
                Engine.log("enumerateChanges(workingSet) threw \(error) — reporting no changes")
                observer.finishEnumeratingChanges(upTo: anchor, moreComing: false)
            }
        }
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        guard isWorkingSet else {
            completionHandler(NSFileProviderSyncAnchor(Data("wusel".utf8)))
            return
        }
        queue.async {
            // The head sequence now, so the first change enumeration asks from
            // "everything up to now" and reports only what arrives afterwards.
            do {
                let client = try Engine.connect()
                let (head, _) = try client.changes(since: UInt64.max)
                completionHandler(Self.anchor(from: head))
            } catch {
                completionHandler(nil)
            }
        }
    }

    // MARK: - Sync-anchor <-> sequence

    private static func sequence(from anchor: NSFileProviderSyncAnchor) -> UInt64 {
        UInt64(String(decoding: anchor.rawValue, as: UTF8.self)) ?? 0
    }

    private static func anchor(from sequence: UInt64) -> NSFileProviderSyncAnchor {
        NSFileProviderSyncAnchor(Data(String(sequence).utf8))
    }

    /// The engine emits change paths without a leading slash (`Angebote/x`); item
    /// identifiers carry one (`/Angebote/x`). Normalise before mapping.
    private static func normalized(_ path: String) -> String {
        path.hasPrefix("/") ? path : "/" + path
    }

    /// Collapse entries whose names are equal once normalised to Unicode NFC,
    /// keeping the one that is really on the server (a `file_id` is present).
    ///
    /// This hides a *legacy* ghost — an NFD-named deferred row a pre-fix build
    /// left beside its NFC server twin (see `ItemMapping.childPath`) — so Finder
    /// no longer shows the file twice. The `childPath` fix stops new ghosts from
    /// forming; this covers the ones already in the model. Two genuinely distinct
    /// names are untouched (NFC of an already-NFC name is itself), and order is
    /// preserved so the listing stays stable.
    private static func deduped(_ entries: [Entry]) -> [Entry] {
        var kept: [String: Entry] = [:]
        var order: [String] = []
        for entry in entries {
            let key = entry.name.precomposedStringWithCanonicalMapping
            if let existing = kept[key] {
                // Prefer the server-backed row; the null-file-id one is the ghost.
                if existing.fileID == nil, entry.fileID != nil { kept[key] = entry }
            } else {
                kept[key] = entry
                order.append(key)
            }
        }
        return order.compactMap { kept[$0] }
    }

}

/// Paths the user just pinned or unpinned via the context menu, awaiting a Finder
/// refresh. The custom action records them and signals the working set; the
/// working-set change enumerator drains this and reports each as updated, so a
/// locally-initiated pin (which emits no server change, and so nothing the change
/// log would carry) still updates the item's offline emblem immediately — for a
/// folder as well as a file. File-backed in the App Group container, like
/// `KnownContainers`, because the extension is short-lived and the action and the
/// enumeration may run in different extension instances.
enum PinRefresh {
    static let filename = "pending-pin-refresh"
    private static let lock = NSLock()

    private static var url: URL? {
        SharedPaths.containerURL?.appendingPathComponent(filename)
    }

    /// Record acted-on paths (account-relative, with a leading slash).
    static func add(_ paths: [String]) {
        guard let url = url, !paths.isEmpty else { return }
        lock.lock()
        defer { lock.unlock() }
        var set = read(url)
        set.formUnion(paths)
        try? Data((set.sorted().joined(separator: "\n") + "\n").utf8).write(to: url)
    }

    /// Take and clear the pending paths.
    static func drain() -> [String] {
        guard let url = url else { return [] }
        lock.lock()
        defer { lock.unlock() }
        let set = read(url)
        if !set.isEmpty { try? FileManager.default.removeItem(at: url) }
        return Array(set)
    }

    private static func read(_ url: URL) -> Set<String> {
        guard let data = try? Data(contentsOf: url),
            let text = String(data: data, encoding: .utf8)
        else { return [] }
        return Set(text.split(separator: "\n").map(String.init))
    }
}
