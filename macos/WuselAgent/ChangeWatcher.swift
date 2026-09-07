// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import FileProvider
import Foundation

/// Turns the engine's change stream into Finder refreshes.
///
/// The socket's `watch` op pushes a `changed` event whenever the server reports
/// a change (via notify_push). This subscribes and calls `signalEnumerator`, so
/// a remote edit shows up without the user reloading. It lives in the agent, not
/// the extension: the extension is ephemeral (macOS suspends and kills it), while
/// the agent is the persistent owner of the domain.
///
/// Reconnects with a short delay if the stream drops (e.g. `wusel serve`
/// restarted), mirroring the serve supervisor's resilience.
final class ChangeWatcher {
    private let domain: NSFileProviderDomain
    private let queue = DispatchQueue(label: "at.itbh.wusel.change-watcher")
    private var stopped = false
    private let reconnectDelay: TimeInterval = 5

    init(domain: NSFileProviderDomain) {
        self.domain = domain
    }

    func start() {
        queue.async { [self] in
            stopped = false
            loop()
        }
    }

    func stop() {
        queue.async { [self] in stopped = true }
    }

    private func loop() {
        guard !stopped else { return }
        do {
            let client = try SocketClient(socketPath: SharedPaths.socketPath)
            try client.watch()
            AgentLog.log("watch: subscribed to the change stream")
            while !stopped, let change = try client.nextPush() {
                if case .changed(_, let path) = change {
                    signal(path: path)
                }
            }
        } catch {
            AgentLog.log("watch: \(error.localizedDescription)")
        }
        // The stream ended or failed; retry unless we are shutting down.
        if !stopped {
            queue.asyncAfter(deadline: .now() + reconnectDelay) { [self] in loop() }
        }
    }

    /// A server-side change happened; nudge Finder's change channel. On macOS only
    /// the working set drives change enumeration — a per-container signal is
    /// ignored — so signal that. The extension's working-set enumerator reports
    /// the delta, limited to folders the user has opened.
    private func signal(path: String) {
        guard let manager = NSFileProviderManager(for: domain) else { return }
        AgentLog.log("watch: change at \(path) -> signal working set")
        manager.signalEnumerator(for: .workingSet) { _ in }
    }
}
