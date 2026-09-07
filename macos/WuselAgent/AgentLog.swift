// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import Foundation

/// Agent-wide logging. Mirrors to NSLog and to a fixed file, because an
/// `open`/launchd-launched agent's NSLog does not reach the unified log in a way
/// the CLI can read, and `open` captures no stderr. The file makes every run's
/// outcome readable from a terminal — the backbone of CLI-driven testing.
///
/// Path: ~/Library/Logs/wusel-agent.log (the agent is not sandboxed, so this is
/// the real user Library, reachable from outside).
enum AgentLog {
    private static let fileURL: URL? = {
        guard let library = FileManager.default
            .urls(for: .libraryDirectory, in: .userDomainMask).first
        else { return nil }
        let logs = library.appendingPathComponent("Logs", isDirectory: true)
        try? FileManager.default.createDirectory(at: logs, withIntermediateDirectories: true)
        return logs.appendingPathComponent("wusel-agent.log")
    }()

    static func log(_ message: String) {
        NSLog("wusel-agent: \(message)")
        guard let url = fileURL, let data = "wusel-agent: \(message)\n".data(using: .utf8) else { return }
        if let handle = try? FileHandle(forWritingTo: url) {
            handle.seekToEndOfFile()
            handle.write(data)
            try? handle.close()
        } else {
            try? data.write(to: url)
        }
    }
}
