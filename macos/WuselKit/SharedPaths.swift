// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import Foundation

/// Where the agent and the extension agree the socket lives.
enum SharedPaths {
    /// The App Group whose container is the agent↔extension meeting point.
    ///
    /// On macOS an App Group is `<TeamID>.<name>`, so the concrete value depends
    /// on the signing team and must not be hard-coded. Both targets carry it in
    /// their Info.plist as `WUSELAppGroup` (composed from `$(WUSEL_APP_GROUP)` at
    /// build time), so agent and extension resolve the *same* group and share the
    /// same container. The fallback only matters for an unsigned/no-group build.
    static var appGroup: String {
        (Bundle.main.object(forInfoDictionaryKey: "WUSELAppGroup") as? String)
            .flatMap { $0.isEmpty ? nil : $0 }
            ?? "group.at.itbh.wusel"
    }

    /// The App Group container, when the build has one (a paid-team build). The
    /// agent keeps recovery markers here — e.g. a one-shot domain-reset flag the
    /// extension and any tooling can drop to force a clean replica rebuild.
    static var containerURL: URL? {
        FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup)
    }

    /// The Unix socket path.
    ///
    /// With a **paid team**, both agent and extension share the App Group
    /// container, so both resolve to the same path — the production layout.
    ///
    /// Without it (a personal-team build), there is no shared container: the
    /// non-sandboxed agent falls back to a fixed path under the real
    /// Application Support, where a CLI `wusel ipc --socket …` can reach it to
    /// test the agent+engine half. The sandboxed extension cannot reach that
    /// path — that is the part the App Group unlocks.
    static var socketPath: String {
        if let container = FileManager.default
            .containerURL(forSecurityApplicationGroupIdentifier: appGroup)
        {
            return container.appendingPathComponent("ipc.sock").path
        }
        if let support = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask).first
        {
            let dir = support.appendingPathComponent("at.itbh.wusel", isDirectory: true)
            try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            return dir.appendingPathComponent("ipc.sock").path
        }
        return (NSTemporaryDirectory() as NSString).appendingPathComponent("wusel-ipc.sock")
    }
}
