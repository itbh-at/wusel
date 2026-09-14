// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import FileProvider
import Foundation

/// The set of container paths the user has actually opened (Finder called
/// `enumerateItems` on them), and the reconcile that re-imports them.
///
/// It lives in the shared WuselKit sources (compiled into both targets), not the
/// extension alone, because two processes need it: the **extension** records
/// opens and reports live changes only for
/// these, and reconciles them after a change-log reset; the **agent** reconciles
/// them again once a lost connection is restored (the reset-time reconcile is
/// deferred while the server is unreachable, so a reimport never wedges a folder
/// with a stuck upload error). Both read the same file in the App Group
/// container (the extension is short-lived); the agent clears it on a domain
/// reset, matching Finder's freshly-emptied replica.
enum KnownContainers {
    static let filename = "known-containers"
    private static let lock = NSLock()

    private static var url: URL? {
        SharedPaths.containerURL?.appendingPathComponent(filename)
    }

    static func remember(_ path: String) {
        guard path.hasPrefix("/"), let url = url else { return }
        lock.lock()
        defer { lock.unlock() }
        var set = read(url)
        guard set.insert(path).inserted else { return }
        try? Data((set.joined(separator: "\n") + "\n").utf8).write(to: url)
    }

    static func snapshot() -> Set<String> {
        guard let url = url else { return [] }
        lock.lock()
        defer { lock.unlock() }
        return read(url)
    }

    private static func read(_ url: URL) -> Set<String> {
        guard let data = try? Data(contentsOf: url),
            let text = String(data: data, encoding: .utf8)
        else { return [] }
        return Set(text.split(separator: "\n").map(String.init))
    }

    /// Ask the system to re-import the opened folders so it reconciles their
    /// contents against the current model. Bounded to the folders the user
    /// actually opened and never the account root (a root-wide reimport once
    /// flooded items with "!"); a folder nested under another opened one is
    /// dropped so each opened subtree is re-scanned once, not repeatedly.
    ///
    /// `reimportItems(below:)` is Void-returning and non-throwing; it reports each
    /// outcome through the completion handler, which is logged via `log`.
    static func reconcileOpenedFolders(
        domain: NSFileProviderDomain, log: @escaping (String) -> Void
    ) {
        guard let manager = NSFileProviderManager(for: domain) else { return }
        let opened = snapshot().filter { $0 != "/" }
        let topmost = opened.filter { path in
            !opened.contains { other in other != path && path.hasPrefix(other + "/") }
        }
        for path in topmost {
            manager.reimportItems(below: NSFileProviderItemIdentifier(rawValue: path)) { error in
                if let error = error {
                    log("reconcile: reimport(\(path)) failed: \(error.localizedDescription)")
                }
            }
        }
    }
}
