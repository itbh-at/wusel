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
        // The engine may be momentarily absent — serve is still binding at
        // launch, or it was just restarted (a `wusel serve` the watchdog reaped
        // with its agent, an Xcode ⌘R). Giving up on the first refused connect
        // fails the enumeration, and the system then marks every item with a
        // "!". Retry briefly so a transient gap is invisible; a genuinely-down
        // backend still errors after the window.
        var lastError: Error?
        for attempt in 0..<6 {
            do {
                return try SocketClient(socketPath: path)
            } catch {
                lastError = error
                if attempt < 5 { Thread.sleep(forTimeInterval: 0.5) }
            }
        }
        log("connect failed after retries (\(path)): \(lastError.map { "\($0)" } ?? "unknown")")
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
