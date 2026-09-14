// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import FileProvider
import Foundation

/// Thin glue between the extension and the socket: open a connection to the
/// running agent's `wusel serve`, and turn engine/transport failures into the
/// `NSFileProviderError`s Finder understands.
enum Engine {
    static func connect() throws -> SocketClient {
        let path = SharedPaths.socketPath
        // The engine may be momentarily absent — serve was just restarted (the
        // supervisor respawns it with backoff; an Xcode ⌘R), or the socket is
        // mid-rebind. Serve now *binds before* its slow start-up, so a merely
        // starting engine no longer refuses us — the connect waits in the
        // backlog — but a restart still opens a gap. Retry with exponential
        // backoff up to a deadline so a transient gap is invisible; a genuinely
        // down backend then errors as `.serverUnreachable` (which Finder retries)
        // instead of either giving up on the first refusal or blocking the
        // callback forever.
        let deadline = Date().addingTimeInterval(10)
        var delay: TimeInterval = 0.1
        var lastError: Error?
        while true {
            do {
                return try SocketClient(socketPath: path)
            } catch {
                lastError = error
                let remaining = deadline.timeIntervalSinceNow
                if remaining <= 0 { break }
                Thread.sleep(forTimeInterval: min(delay, remaining))
                delay = min(delay * 2, 1.0)
            }
        }
        log("connect failed, retried to deadline (\(path)): \(lastError.map { "\($0)" } ?? "unknown")")
        throw lastError ?? SocketClient.SocketError.connect(0)
    }

    /// Diagnostic log to a file in the App Group container, readable from a
    /// terminal — the extension's NSLog does not reach the unified log and it has
    /// no console. Temporary scaffolding while bringing the extension up.
    static func log(_ message: String) {
        NSLog("wusel-ext: \(message)")
        guard let dir = FileManager.default
            .containerURL(forSecurityApplicationGroupIdentifier: SharedPaths.appGroup)
        else { return }
        let url = dir.appendingPathComponent("extension.log")
        guard let data = "\(Date()) \(message)\n".data(using: .utf8) else { return }
        if let handle = try? FileHandle(forWritingTo: url) {
            handle.seekToEndOfFile()
            handle.write(data)
            try? handle.close()
        } else {
            try? data.write(to: url)
        }
    }

    /// Map an error onto the File Provider domain. A missing object is
    /// `.noSuchItem`; anything else (a transport failure, the agent not running)
    /// is `.serverUnreachable`, which tells Finder to back off and retry rather
    /// than treat the item as gone.
    static func nsError(_ error: Error) -> NSError {
        let code: NSFileProviderError.Code
        if let wire = error as? WireError, wire == .notFound {
            code = .noSuchItem
        } else {
            code = .serverUnreachable
        }
        return NSError(domain: NSFileProviderError.errorDomain, code: code.rawValue)
    }
}
