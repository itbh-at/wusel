// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import Foundation

/// Runs and supervises the engine's `wusel serve` as a child process, binding
/// the Unix socket the File Provider extension talks to. This is the agent's
/// core job: the extension owns no state and drives the engine over this socket,
/// so the engine must be alive whenever the extension might be asked to work.
///
/// The `wusel` binary is bundled in the app (Contents/Resources/wusel); building
/// and copying it is `mise run bundle-wusel`. If it exits, the supervisor
/// restarts it after a short backoff; on agent quit it is terminated cleanly.
final class ServeSupervisor {
    private let socketPath: String
    private let queue = DispatchQueue(label: "at.itbh.wusel.serve-supervisor")
    private var process: Process?
    private var stopped = false

    /// Restart delay. It grows exponentially on fast failures and resets after a
    /// healthy run, so a process that cannot start (e.g. no account configured
    /// yet — `wusel login` not run) settles to one retry per minute instead of a
    /// tight loop that burns energy. A later login is still picked up within the
    /// cap.
    private var backoff: TimeInterval = minBackoff
    private static let minBackoff: TimeInterval = 2
    private static let maxBackoff: TimeInterval = 60
    /// A run at least this long counts as healthy; a crash after it is treated as
    /// transient and resets the backoff.
    private static let healthyRun: TimeInterval = 10

    init(socketPath: String) {
        self.socketPath = socketPath
    }

    /// Start serving. Idempotent-ish: intended to be called once, at launch.
    func start() {
        queue.async { [self] in
            stopped = false
            spawn()
        }
    }

    /// Stop serving and do not restart. Called on agent termination.
    func stop() {
        queue.sync { [self] in
            stopped = true
            process?.terminate()
            process = nil
        }
    }

    private func spawn() {
        guard !stopped else { return }
        guard let binary = Self.binaryURL() else {
            AgentLog.log("serve: bundled 'wusel' binary not found (run `mise run bundle-wusel`)")
            return
        }
        // Record exactly which engine build this is, every spawn. The bundled
        // binary and the running process are otherwise impossible to tell apart,
        // and a stale bundle has looked identical to a fresh one more than once.
        AgentLog.log("serve: engine \(Self.engineVersion(binary))")

        let proc = Process()
        proc.executableURL = binary
        proc.arguments = ["serve", "--socket", socketPath]
        // The engine localizes user notices from its own environment locale
        // (`ui_locale()` reads LC_ALL/LC_MESSAGES/LANG). A launchd-spawned agent
        // often has no LANG, so it would default to English; pass the logged-in
        // user's UI language down so notices reach non-technical users in their
        // own language — the one place we translate.
        var environment = ProcessInfo.processInfo.environment
        if let locale = Self.preferredPosixLocale() {
            environment["LANG"] = locale
        }
        proc.environment = environment
        let startedAt = Date()
        proc.terminationHandler = { [weak self] _ in
            guard let self else { return }
            self.queue.async {
                self.process = nil
                guard !self.stopped else { return }
                let ranFor = Date().timeIntervalSince(startedAt)
                if ranFor >= Self.healthyRun {
                    self.backoff = Self.minBackoff  // it was serving; a crash is transient
                } else {
                    self.backoff = min(self.backoff * 2, Self.maxBackoff)  // cannot start; ease off
                }
                AgentLog.log("serve: exited after \(Int(ranFor))s; retrying in \(Int(self.backoff))s")
                self.queue.asyncAfter(deadline: .now() + self.backoff) { self.spawn() }
            }
        }

        do {
            try proc.run()
            process = proc
            AgentLog.log("serve: started \(binary.path) --socket \(socketPath)")
        } catch {
            AgentLog.log("serve: could not start: \(error)")
        }
    }

    /// The bundled engine's `--version` line (package version + build stamp), so
    /// the agent log names the exact build it is about to run. Best-effort — a
    /// query failure never blocks the spawn.
    private static func engineVersion(_ binary: URL) -> String {
        let proc = Process()
        proc.executableURL = binary
        proc.arguments = ["--version"]
        let out = Pipe()
        proc.standardOutput = out
        proc.standardError = Pipe()
        do {
            try proc.run()
            proc.waitUntilExit()
            let data = out.fileHandleForReading.readDataToEndOfFile()
            let text = String(decoding: data, as: UTF8.self)
                .trimmingCharacters(in: .whitespacesAndNewlines)
            return text.isEmpty ? "(no version output)" : text
        } catch {
            return "(version query failed: \(error.localizedDescription))"
        }
    }

    /// The bundled engine binary, looked up in the app bundle.
    private static func binaryURL() -> URL? {
        Bundle.main.url(forResource: "wusel", withExtension: nil)
            ?? Bundle.main.url(forAuxiliaryExecutable: "wusel")
    }

    /// The user's preferred UI language as a POSIX locale for `LANG`, e.g.
    /// `de-DE` → `de_DE.UTF-8`, `en` → `en.UTF-8`. `nil` when none is set, so the
    /// engine keeps its English default. Only the language subtag matters to the
    /// engine's `ui_locale()`, but a full, well-formed value is the least
    /// surprising thing to hand down.
    private static func preferredPosixLocale() -> String? {
        guard let tag = Locale.preferredLanguages.first, !tag.isEmpty else { return nil }
        // Strip any script subtag (`zh-Hans-CN` → `zh-CN`), which POSIX has no
        // slot for, then map BCP-47's `-` to POSIX's `_`.
        let parts = tag.split(separator: "-")
        let posix: String
        switch parts.count {
        case 0:
            return nil
        case 1:
            posix = String(parts[0])
        default:
            // language + region (drop a middle script subtag if present).
            posix = "\(parts[0])_\(parts[parts.count - 1])"
        }
        return "\(posix).UTF-8"
    }
}
