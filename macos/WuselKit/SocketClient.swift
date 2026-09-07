// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import Foundation
#if canImport(Darwin)
import Darwin
#endif

/// A synchronous client for a `wusel serve` Unix-domain socket — the Swift twin
/// of `wusel_ipc::Client`. It speaks the same framing (a 4-byte big-endian
/// length prefix then that many bytes) so the two cannot drift.
///
/// Blocking on purpose: the File Provider callbacks already run on their own
/// queues, and a short request/response over a local socket has nothing to gain
/// from async plumbing. One connection per call keeps it simple; the engine
/// handles each on its own thread.
final class SocketClient {
    enum SocketError: Error {
        case connect(Int32)
        case pathTooLong
        case truncated
        case closed
        case oversizedFrame(UInt32)
        case unexpectedResponse
    }

    /// The largest frame we will read, matching the Rust `MAX_FRAME` — a guard
    /// against a corrupt length prefix asking for a huge allocation.
    private static let maxFrame: UInt32 = 8 * 1024 * 1024

    private let fd: Int32

    /// Connect to the socket at `socketPath`.
    init(socketPath: String) throws {
        let f = socket(AF_UNIX, SOCK_STREAM, 0)
        guard f >= 0 else { throw SocketError.connect(errno) }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let capacity = MemoryLayout.size(ofValue: addr.sun_path)
        let pathBytes = Array(socketPath.utf8)
        guard pathBytes.count < capacity else {
            close(f)
            throw SocketError.pathTooLong
        }
        withUnsafeMutablePointer(to: &addr.sun_path) { raw in
            raw.withMemoryRebound(to: UInt8.self, capacity: capacity) { dst in
                for (i, byte) in pathBytes.enumerated() { dst[i] = byte }
                dst[pathBytes.count] = 0
            }
        }
        let size = socklen_t(MemoryLayout<sockaddr_un>.size)
        let rc = withUnsafePointer(to: &addr) { p in
            p.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                Darwin.connect(f, sa, size)
            }
        }
        guard rc == 0 else {
            let e = errno
            close(f)
            throw SocketError.connect(e)
        }
        self.fd = f
    }

    deinit { close(fd) }

    /// Send one request and read its answer. A `fetch` answer carries its content
    /// in the returned `Data`; every other answer returns `nil` there.
    func call(_ request: Request) throws -> (Response, Data?) {
        try writeFrame(try JSONEncoder().encode(request))
        let response = try readResponse()
        if case .bytes = response {
            return (response, try readFrameExpecting())
        }
        return (response, nil)
    }

    /// Send a `write` request whose bytes follow in the next frame, and read the
    /// `written` (or `error`) answer.
    func callWrite(_ request: Request, data: Data) throws -> Response {
        try writeFrame(try JSONEncoder().encode(request))
        try writeFrame(data)
        return try readResponse()
    }

    /// Keep `path` offline: pin it and (for a directory, recursively) download it
    /// now. Can be slow — it hydrates — so call it off the main thread. Throws on
    /// a wire error (e.g. the path does not resolve).
    func pin(_ path: String) throws {
        try sendAction(op: "pin", path: path)
    }

    /// Drop `path`'s keep-offline protection. The bytes stay cached until evicted.
    func unpin(_ path: String) throws {
        try sendAction(op: "unpin", path: path)
    }

    /// Send an op that answers with `done` (or an error), and surface a failure.
    private func sendAction(op: String, path: String) throws {
        let (response, _) = try call(Request(op: op, path: path))
        switch response {
        case .done:
            return
        case .error(let error):
            throw error
        default:
            throw SocketError.unexpectedResponse
        }
    }

    /// Turn this connection into a change stream: send `watch`, then call
    /// `nextPush()` repeatedly.
    func watch() throws {
        try writeFrame(try JSONEncoder().encode(Request(op: "watch", path: "")))
    }

    /// Turn this connection into a notice stream: send `notices`, then call
    /// `nextPush()` repeatedly. The twin of `watch()` for user notifications.
    func notices() throws {
        try writeFrame(try JSONEncoder().encode(Request(op: "notices", path: "")))
    }

    /// Pull the change log at or after `since`; returns the head sequence (the
    /// next anchor) and the batch. The replicated working-set enumerator drives
    /// its sync anchors from this instead of holding a live `watch` open.
    func changes(since: UInt64) throws -> (seq: UInt64, changes: [ChangeItem]) {
        let (response, _) = try call(Request(op: "changes", path: "", since: since))
        guard case .changes(let seq, let items) = response else {
            throw SocketError.unexpectedResponse
        }
        return (seq, items)
    }

    /// Block for the next pushed frame on a `watch` or `notices` connection; `nil`
    /// at a clean end of stream (the peer closed, e.g. `wusel serve` restarted).
    func nextPush() throws -> Response? {
        guard let frame = try readFrameOrNil() else { return nil }
        return try JSONDecoder().decode(Response.self, from: frame)
    }

    // MARK: - Framing

    /// Like `readFrameExpecting`, but returns `nil` on a clean EOF at the frame
    /// boundary rather than throwing — the loop terminator for a push reader
    /// (`watch` / `notices`).
    private func readFrameOrNil() throws -> Data? {
        guard let header = try readExactOrNil(4) else { return nil }
        let len = header.withUnsafeBytes { $0.load(as: UInt32.self).bigEndian }
        guard len <= Self.maxFrame else { throw SocketError.oversizedFrame(len) }
        return try readExact(Int(len))
    }

    private func readExactOrNil(_ count: Int) throws -> Data? {
        var buffer = Data(count: count)
        var got = 0
        let eof = try buffer.withUnsafeMutableBytes { (raw: UnsafeMutableRawBufferPointer) -> Bool in
            while got < count {
                let n = Darwin.read(fd, raw.baseAddress!.advanced(by: got), count - got)
                if n > 0 {
                    got += n
                } else if n == 0 {
                    if got == 0 { return true }  // clean EOF at the boundary
                    throw SocketError.truncated
                } else if errno == EINTR {
                    continue
                } else {
                    throw SocketError.closed
                }
            }
            return false
        }
        return eof ? nil : buffer
    }

    private func readResponse() throws -> Response {
        let frame = try readFrameExpecting()
        return try JSONDecoder().decode(Response.self, from: frame)
    }

    private func writeFrame(_ body: Data) throws {
        var prefix = UInt32(body.count).bigEndian
        try withUnsafeBytes(of: &prefix) { try writeAll(Data($0)) }
        try writeAll(body)
    }

    /// Read one frame; a clean EOF at the boundary is an error here because the
    /// caller always expects a frame (the loop-terminating `nil` case belongs to
    /// a `watch` reader, which this client does not implement).
    private func readFrameExpecting() throws -> Data {
        let header = try readExact(4)
        let len = header.withUnsafeBytes { $0.load(as: UInt32.self).bigEndian }
        guard len <= Self.maxFrame else { throw SocketError.oversizedFrame(len) }
        return try readExact(Int(len))
    }

    // MARK: - Raw I/O

    private func writeAll(_ data: Data) throws {
        try data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            var offset = 0
            while offset < raw.count {
                let n = Darwin.write(fd, raw.baseAddress!.advanced(by: offset), raw.count - offset)
                if n > 0 {
                    offset += n
                } else if n < 0 && errno == EINTR {
                    continue
                } else {
                    throw SocketError.closed
                }
            }
        }
    }

    private func readExact(_ count: Int) throws -> Data {
        var buffer = Data(count: count)
        try buffer.withUnsafeMutableBytes { (raw: UnsafeMutableRawBufferPointer) in
            var offset = 0
            while offset < count {
                let n = Darwin.read(fd, raw.baseAddress!.advanced(by: offset), count - offset)
                if n > 0 {
                    offset += n
                } else if n == 0 {
                    throw SocketError.truncated
                } else if errno == EINTR {
                    continue
                } else {
                    throw SocketError.closed
                }
            }
        }
        return buffer
    }
}
