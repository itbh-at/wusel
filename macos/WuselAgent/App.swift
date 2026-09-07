// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import AppKit
import FileProvider
import ServiceManagement

/// The background agent app. `LSUIElement` (set in Info.plist) keeps it out of
/// the Dock and menu bar.
///
/// Its jobs, per documentation/.../frontends.adoc:
///   1. run the Rust engine and bind `wusel serve` on the shared-container
///      socket the extension connects to;
///   2. register the File Provider domain so Finder shows the account;
///   3. relay server-side changes to Finder (the socket `watch` stream →
///      `signalEnumerator`) so remote edits appear without a manual refresh;
///   4. start itself at login (registered as a launch agent);
///   5. post Notification Center notices — the engine's user notices, pushed over
///      the socket `notices` stream, become banners here (only an app may post).
///
/// Entry point is a plain AppKit bootstrap, *not* a SwiftUI `App`: a window-less
/// SwiftUI `App` (a lone `Settings` scene) self-terminates right after launch
/// unless a debugger holds it open, which is exactly wrong for a daemon. An
/// `NSApplication` set to `.accessory` with a retained delegate runs its loop
/// until told to stop.
@main
enum WuselAgentMain {
    /// Retained for the whole process lifetime; `NSApplication.delegate` is weak.
    private static let delegate = AppDelegate()

    static func main() {
        let app = NSApplication.shared
        app.delegate = delegate
        app.setActivationPolicy(.accessory)
        app.run()
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    /// One domain per account; a single "default" account for now.
    private let domain = NSFileProviderDomain(
        identifier: NSFileProviderDomainIdentifier(rawValue: "at.itbh.wusel.default"),
        displayName: "Wusel")

    /// Keeps the engine's `wusel serve` alive for as long as the agent runs.
    private let serve = ServeSupervisor(socketPath: SharedPaths.socketPath)

    /// Relays the engine's change stream to Finder.
    private lazy var changes = ChangeWatcher(domain: domain)

    /// Relays the engine's user notices to Notification Center.
    private let notices = NoticeWatcher(socketPath: SharedPaths.socketPath)

    func applicationDidFinishLaunching(_ notification: Notification) {
        AgentLog.log("starting; bundle=\(Bundle.main.bundlePath)")

        // (4) Start at login. Registering the app as a launch agent means the
        // user does not have to keep it (or Xcode) open. Best-effort: a failure
        // (e.g. the app is not in a stable location like /Applications yet) is
        // logged, not fatal.
        registerLoginItem()

        // Make ourselves the only agent, then the only serve. An agent orphaned
        // by an abrupt stop (Xcode ⌘R or a crash) or logout keeps running — the
        // accessory app does not self-terminate — and spawns its own serve, so
        // two engines fight for the one socket: two change logs, lost updates,
        // and every item stuck with a "!". Reaping serves alone does not fix it
        // (a surviving agent immediately respawns its serve), so kill the stray
        // agent first, then the serve it — or the parent-death watchdog, which
        // can miss a serve orphaned the instant after spawning — left behind.
        reapStrayAgents()
        reapStrayServes()

        // (1) Run the engine. The extension drives it over this socket.
        serve.start()

        // Registering the domain triggers immediate enumeration, so wait until
        // serve actually accepts connections first — otherwise the system's first
        // calls race the bind, fail, and leave every item marked with a "!".
        waitForServeReady { [self] in
            // (3) Relay server-side changes to Finder.
            changes.start()
            // (5) Relay engine notices to Notification Center.
            notices.start()
            // (2) Register the File Provider domain so Finder shows the account.
            registerDomain()
        }
    }

    /// Kill any other agent instance before we start (see the call site).
    ///
    /// Matches the agent executable only — never the extension
    /// (`…/PlugIns/…/WuselFileProvider`) or the serve (`…/Resources/wusel`),
    /// whose paths do not contain `Wusel.app/Contents/MacOS/Wusel` — and never
    /// ourselves (filtered by PID). Shells out rather than calling `kill(2)` so
    /// no extra POSIX import is needed; `/bin/kill` sends SIGTERM, letting the
    /// stray shut its serve down and release the socket cleanly.
    private func reapStrayAgents() {
        let ownPid = String(ProcessInfo.processInfo.processIdentifier)
        let pgrep = Process()
        pgrep.executableURL = URL(fileURLWithPath: "/usr/bin/pgrep")
        pgrep.arguments = ["-f", "Wusel.app/Contents/MacOS/Wusel"]
        let out = Pipe()
        pgrep.standardOutput = out
        do {
            try pgrep.run()
            pgrep.waitUntilExit()
        } catch {
            AgentLog.log("reapStrayAgents: \(error.localizedDescription)")
            return
        }
        let strays = String(decoding: out.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
            .split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty && $0 != ownPid }
        guard !strays.isEmpty else { return }
        let kill = Process()
        kill.executableURL = URL(fileURLWithPath: "/bin/kill")
        kill.arguments = strays
        do {
            try kill.run()
            kill.waitUntilExit()
            AgentLog.log("reaped \(strays.count) stray agent(s) before starting: \(strays.joined(separator: ", "))")
            // Let the stray release the socket (and its serve exit) before we bind.
            Thread.sleep(forTimeInterval: 0.3)
        } catch {
            AgentLog.log("reapStrayAgents kill: \(error.localizedDescription)")
        }
    }

    /// Kill any leftover `wusel serve` before we start ours (see the call site).
    private func reapStrayServes() {
        let pkill = Process()
        pkill.executableURL = URL(fileURLWithPath: "/usr/bin/pkill")
        pkill.arguments = ["-f", "Resources/wusel serve --socket"]
        do {
            try pkill.run()
            pkill.waitUntilExit()
            if pkill.terminationStatus == 0 {
                AgentLog.log("reaped a stray serve before starting")
                // Give the OS a moment to release the socket the stray held.
                Thread.sleep(forTimeInterval: 0.3)
            }
        } catch {
            AgentLog.log("reapStrayServes: \(error.localizedDescription)")
        }
    }

    /// Poll the socket until serve accepts a connection (or a timeout), then run
    /// `ready` on the main queue. Serve binds a moment after it is spawned; the
    /// File Provider enumeration that domain registration kicks off must not run
    /// before then.
    private func waitForServeReady(then ready: @escaping () -> Void) {
        DispatchQueue.global(qos: .userInitiated).async {
            let deadline = Date().addingTimeInterval(20)
            while Date() < deadline {
                if (try? SocketClient(socketPath: SharedPaths.socketPath)) != nil {
                    AgentLog.log("serve is accepting connections")
                    DispatchQueue.main.async(execute: ready)
                    return
                }
                Thread.sleep(forTimeInterval: 0.3)
            }
            AgentLog.log("serve not ready within 20s — registering the domain anyway")
            DispatchQueue.main.async(execute: ready)
        }
    }

    /// The one-shot domain-reset marker: `~/.config/wusel/reset-domain`. Dropping
    /// this file makes the next launch rebuild the replica from scratch. It lives
    /// outside the App Group container on purpose — macOS shields that container
    /// from external tools (Terminal, scripts) via TCC, so a marker there could
    /// not be dropped to trigger a recovery. The non-sandboxed agent reaches the
    /// real home directory directly.
    private var resetMarkerURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".config/wusel/reset-domain")
    }

    /// Register the File Provider domain. If a reset marker is present, discard
    /// the current replica first (remove the domain, then re-add it) — a recovery
    /// path for a replica a prior failure left degraded (items stuck with a "!").
    /// Every file is online-only, so a reset loses no local data; it only forces
    /// a clean rebuild. The marker is consumed, so a reset happens once, on demand.
    private func registerDomain() {
        let marker = resetMarkerURL
        if FileManager.default.fileExists(atPath: marker.path) {
            try? FileManager.default.removeItem(at: marker)
            // Finder's replica is rebuilt from scratch; drop the extension's
            // opened-folders memory (KnownContainers) so live updates match.
            if let container = SharedPaths.containerURL {
                try? FileManager.default.removeItem(
                    at: container.appendingPathComponent("known-containers"))
            }
            AgentLog.log("reset marker present — removing the domain before re-adding")
            NSFileProviderManager.remove(domain) { [self] error in
                if let error = error {
                    AgentLog.log("domain remove failed: \(error.localizedDescription)")
                }
                addDomain()
            }
        } else {
            addDomain()
        }
    }

    /// Add the domain so Finder shows the account, and log the outcome.
    private func addDomain() {
        NSFileProviderManager.add(domain) { error in
            if let error = error {
                AgentLog.log("could not register the File Provider domain: \(error.localizedDescription) [\(error)]")
            } else {
                AgentLog.log("File Provider domain registered")
            }
            NSFileProviderManager.getDomainsWithCompletionHandler { domains, listError in
                if let listError = listError {
                    AgentLog.log("getDomains failed: \(listError)")
                } else {
                    AgentLog.log("system holds \(domains.count) domain(s): \(domains.map { $0.identifier.rawValue })")
                }
            }
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        notices.stop()
        changes.stop()
        serve.stop()
    }

    /// Register the agent to launch at login. `SMAppService.mainApp` persists the
    /// registration across reboots; it needs the app in a stable location
    /// (/Applications), so from a DerivedData run it may not stick — that is fine
    /// for development and is why this is best-effort.
    private func registerLoginItem() {
        do {
            if SMAppService.mainApp.status != .enabled {
                try SMAppService.mainApp.register()
                AgentLog.log("registered as a login item")
            }
        } catch {
            AgentLog.log("login-item registration failed: \(error.localizedDescription)")
        }
    }
}
