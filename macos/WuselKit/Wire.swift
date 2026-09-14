// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

import Foundation

/// The framed request/response protocol, the Swift mirror of the Rust
/// `wusel_ipc::wire` module. Both sides must agree byte-for-byte, so this file
/// is the twin of `crates/wusel-ipc/src/wire.rs`: same op names, same JSON field
/// names (snake_case), same response discriminant (`kind`).
///
/// A request is one JSON frame; a response is one JSON header frame, except that
/// a `bytes` header is followed by a second frame carrying the raw content. A
/// `write` request is the mirror: the request frame, then a content frame.

/// A request to the engine. Only the fields an op needs are meaningful; the rest
/// default on the Rust side (`#[serde(default)]`), so optionals that are `nil`
/// are simply omitted.
struct Request: Encodable {
    /// Read: `stat | enumerate | fetch`. Write: `create | write | publish |
    /// remove | move | setattr`. Change stream: `watch`.
    let op: String
    /// The account-relative path, e.g. `/Notes.txt`.
    let path: String
    /// `fetch`/`write`: the byte offset.
    var offset: UInt64 = 0
    /// `fetch`: how many bytes to read.
    var len: UInt32 = 0
    /// `move`: the destination path.
    var to: String?
    /// `create`: make a directory rather than a file.
    var dir: Bool?
    /// `setattr`: the new size.
    var size: UInt64?
    /// `setattr`: the new modification time (Unix seconds).
    var mtime: Int64?
    /// `changes`: replay the change log from this sequence anchor onward.
    var since: UInt64?
}

/// The per-object sync state, the neutral axis every frontend projects onto its
/// own emblems (the Rust `wusel_ipc::wire::SyncState`, kebab-case on the wire).
/// Decoded leniently: an unknown value from a newer engine reads as `nil` rather
/// than failing the whole response, which is what lets the engine add a state
/// without breaking this client.
enum SyncState: String, Decodable {
    case onlineOnly = "online-only"
    case cached
    case pinned
    case pinnedPending = "pinned-pending"
    case pinnedStale = "pinned-stale"
    case modified
    case uploading
    case syncError = "sync-error"
}

/// Whether an object is the root of a Team/Group folder (the Rust
/// `wusel_ipc::wire::Kind`, snake_case on the wire). Unknown values read as
/// `.plain`, for the same forward-compatibility reason as `SyncState`.
enum FolderKind: String, Decodable {
    case plain
    case groupFolder = "group_folder"
}

/// One directory child, as `enumerate` reports it.
struct Entry: Decodable {
    let name: String
    let isDir: Bool
    let size: UInt64
    let mtime: Int64
    /// The object's ETag — the same value `stat` reports, so a listed item and a
    /// stat'd/fetched one share one content version (see WuselItem.itemVersion).
    /// Empty for an object not yet on the server, and absent from an older serve.
    let etag: String
    /// Nextcloud's stable file id; `nil` until the object is on the server.
    let fileID: UInt64?
    /// Whether the entry is kept offline (pinned). Absent from an older serve, so
    /// decoded with a `false` default.
    let pinned: Bool
    /// Whether the kept copy is out of date (only meaningful with `pinned`).
    let stale: Bool
    /// The full sync state. Absent from an older serve, and from an object that
    /// has none (a plain directory), so it stays optional — `pinned`/`stale`
    /// remain the fallback for everything this client draws.
    let state: SyncState?
    /// Whether the object is a Team/Group folder root.
    let folderKind: FolderKind

    enum CodingKeys: String, CodingKey {
        case name
        case isDir = "is_dir"
        case size
        case mtime
        case etag
        case fileID = "file_id"
        case pinned
        case stale
        case state
        case folderKind = "folder_kind"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        name = try c.decode(String.self, forKey: .name)
        isDir = try c.decode(Bool.self, forKey: .isDir)
        size = try c.decode(UInt64.self, forKey: .size)
        mtime = try c.decode(Int64.self, forKey: .mtime)
        etag = try c.decodeIfPresent(String.self, forKey: .etag) ?? ""
        fileID = try c.decodeIfPresent(UInt64.self, forKey: .fileID)
        pinned = try c.decodeIfPresent(Bool.self, forKey: .pinned) ?? false
        stale = try c.decodeIfPresent(Bool.self, forKey: .stale) ?? false
        // Decoded through the raw string so an unrecognised value degrades to
        // `nil` / `.plain` instead of throwing and failing the whole response.
        let rawState = try c.decodeIfPresent(String.self, forKey: .state)
        state = rawState.flatMap(SyncState.init(rawValue:))
        let rawKind = try c.decodeIfPresent(String.self, forKey: .folderKind)
        folderKind = rawKind.flatMap(FolderKind.init(rawValue:)) ?? .plain
    }
}

/// One object's attributes, as `stat`/`create`/`setattr` report them (the wire
/// `node` response).
struct NodeInfo: Decodable {
    let name: String
    let isDir: Bool
    let size: UInt64
    let mtime: Int64
    let etag: String
    let fileID: UInt64?
    /// Whether the object is kept offline (pinned). Absent from an older serve,
    /// so decoded with a `false` default.
    let pinned: Bool
    /// Whether the kept copy is out of date (only meaningful with `pinned`).
    let stale: Bool
    /// The full sync state. Absent from an older serve, and from an object that
    /// has none (a plain directory), so it stays optional — `pinned`/`stale`
    /// remain the fallback for everything this client draws.
    let state: SyncState?
    /// Whether the object is a Team/Group folder root.
    let folderKind: FolderKind

    enum CodingKeys: String, CodingKey {
        case name
        case isDir = "is_dir"
        case size
        case mtime
        case etag
        case fileID = "file_id"
        case pinned
        case stale
        case state
        case folderKind = "folder_kind"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        name = try c.decode(String.self, forKey: .name)
        isDir = try c.decode(Bool.self, forKey: .isDir)
        size = try c.decode(UInt64.self, forKey: .size)
        mtime = try c.decode(Int64.self, forKey: .mtime)
        etag = try c.decode(String.self, forKey: .etag)
        fileID = try c.decodeIfPresent(UInt64.self, forKey: .fileID)
        pinned = try c.decodeIfPresent(Bool.self, forKey: .pinned) ?? false
        stale = try c.decodeIfPresent(Bool.self, forKey: .stale) ?? false
        // Decoded through the raw string so an unrecognised value degrades to
        // `nil` / `.plain` instead of throwing and failing the whole response.
        let rawState = try c.decodeIfPresent(String.self, forKey: .state)
        state = rawState.flatMap(SyncState.init(rawValue:))
        let rawKind = try c.decodeIfPresent(String.self, forKey: .folderKind)
        folderKind = rawKind.flatMap(FolderKind.init(rawValue:)) ?? .plain
    }
}

/// Why a request failed — the closed set the engine reports.
enum WireError: String, Decodable, Error {
    case notFound = "not_found"
    case io
    case badRequest = "bad_request"
}

/// What kind of change a `watch` event reports.
enum ChangeKind: String, Decodable {
    case entry
    case content
}

/// How a `notice` reads, so the agent picks the banner's sound/interruption
/// level — the mirror of the Rust `wusel_ipc::wire::Severity`.
enum Severity: String, Decodable {
    case success
    case warning
    case error
}

/// One entry in a pulled `changes` batch — the batch mirror of a `changed` push.
struct ChangeItem: Decodable {
    let change: ChangeKind
    let path: String
}

/// A response header. Decoded by its `kind` discriminant, matching serde's
/// internally-tagged form on the Rust side.
enum Response: Decodable {
    case node(NodeInfo)
    case entries([Entry])
    /// A read's content follows in the next frame; the associated value is its
    /// length.
    case bytes(len: UInt64)
    /// A write was accepted into the buffer; the associated value is the count.
    case written(len: UInt32)
    /// An operation succeeded with nothing to return.
    case done
    case error(WireError)
    /// A pushed change on a `watch` connection.
    case changed(kind: ChangeKind, path: String)
    /// A pulled batch of changes at or after the requested anchor, with the head
    /// sequence to anchor from next.
    case changes(seq: UInt64, changes: [ChangeItem])
    /// A pushed, already-localized user notice on a `notices` connection. `kind`
    /// is the notice's stable id (e.g. `"connection-restored"`), which the agent
    /// acts on beyond just showing the banner.
    case notice(kind: String, severity: Severity, title: String, body: String)
    /// The reply to `reachable`: whether the daemon currently has positive
    /// evidence the server is reachable.
    case reachable(Bool)

    private enum CodingKeys: String, CodingKey {
        case kind, len, error, change, path, seq, changes, severity, title, body, reachable
        case noticeKind = "notice_kind"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try c.decode(String.self, forKey: .kind)
        switch kind {
        case "node":
            self = .node(try NodeInfo(from: decoder))
        case "entries":
            self = .entries(try EntriesEnvelope(from: decoder).entries)
        case "bytes":
            self = .bytes(len: try c.decode(UInt64.self, forKey: .len))
        case "written":
            self = .written(len: try c.decode(UInt32.self, forKey: .len))
        case "done":
            self = .done
        case "error":
            self = .error(try c.decode(WireError.self, forKey: .error))
        case "changed":
            self = .changed(
                kind: try c.decode(ChangeKind.self, forKey: .change),
                path: try c.decode(String.self, forKey: .path))
        case "changes":
            self = .changes(
                seq: try c.decode(UInt64.self, forKey: .seq),
                changes: try c.decode([ChangeItem].self, forKey: .changes))
        case "notice":
            self = .notice(
                kind: try c.decode(String.self, forKey: .noticeKind),
                severity: try c.decode(Severity.self, forKey: .severity),
                title: try c.decode(String.self, forKey: .title),
                body: try c.decode(String.self, forKey: .body))
        case "reachable":
            self = .reachable(try c.decode(Bool.self, forKey: .reachable))
        default:
            throw WireError.badRequest
        }
    }
}

/// Helper for decoding the `entries` array out of the flat `entries` response.
private struct EntriesEnvelope: Decodable {
    let entries: [Entry]
}
