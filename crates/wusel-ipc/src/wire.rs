// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The framed wire protocol — **provisional, a Phase-0 spike**.
//!
//! Every frame is a 4-byte big-endian length prefix followed by that many
//! bytes. A *request* is a single JSON frame; a *response* is a single JSON
//! header frame, except that a `kind:"bytes"` header is immediately followed by
//! a second frame carrying the raw bytes (the header's `len` says how many). A
//! length prefix rather than a delimiter keeps binary content — the whole point
//! of `fetch` — from having to be escaped, and lets a reader size its buffer
//! before it reads.
//!
//! This shape is deliberately minimal and expected to change once the real
//! macOS File Provider extension is written against it; it is not a stable ABI.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// A frame's largest permitted length. Requests are tiny JSON, so a generous
/// cap still refuses a hostile or corrupt prefix that would otherwise ask for a
/// multi-gigabyte allocation. Response byte-frames we write ourselves, so this
/// bounds only what we *read*.
pub const MAX_FRAME: u32 = 8 * 1024 * 1024;

/// The wire's schema version. A client sends nothing to negotiate it yet; the
/// point for now is that the number is *here*, so the moment the protocol grows
/// a breaking change one side can refuse the other by version instead of
/// mis-parsing a field it does not understand. Several frontends (the macOS File
/// Provider, a Nautilus/KDE plugin, a Windows shell handler) will speak this one
/// protocol, and they will not upgrade in lockstep — the version is what lets an
/// old client and a new daemon (or the reverse) meet without corruption.
///
/// Bump only on a breaking change to the shape. Adding a `#[serde(default)]`
/// field is not breaking: an old peer omits it and the new peer reads the
/// default — which is how `state`/`kind` were added without a bump.
pub const SCHEMA: u32 = 1;

/// The per-object sync state — the emblem axis, the neutral superset every
/// frontend projects onto its own decoration vocabulary. It mirrors
/// [`wusel_core::provider::FileState`] one-to-one, kept as a distinct wire type
/// so the engine's enum carries no serialization concern and the wire can evolve
/// on its own.
///
/// This is deliberately the *whole* set, not a lowest common denominator: the
/// wire carries the richest truth the engine knows, and each client decides how
/// much to draw. A file manager that only distinguishes "available" from
/// "online-only" ignores the rest; one that draws a syncing spinner reads
/// `uploading`. Encoding a reduced set here — the way an emblem-poor platform
/// might be tempted to — would strand the richer clients, defeating the point of
/// a shared protocol.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyncState {
    /// No local copy; opening fetches from the server.
    ///
    /// Also the `Default`, which is a *read-path* convenience only — for a peer
    /// that predates the field. Never substitute it when writing: an object with
    /// no state must leave the field out, or every plain directory is reported
    /// as online-only.
    #[default]
    OnlineOnly,
    /// A fresh copy is cached locally but evictable.
    Cached,
    /// Kept offline on purpose — pinned, or under a pinned root — and here.
    Pinned,
    /// Pinned, but nothing is here yet: the promise is made, the bytes are not
    /// down. A directory pin covers files the server grows later, so this is
    /// the ordinary state of one until it is fetched.
    PinnedPending,
    /// Pinned, and the kept copy is out of date.
    PinnedStale,
    /// A local edit not yet flushed.
    Modified,
    /// A committed change on its way to the server.
    Uploading,
    /// A committed change whose upload failed for good.
    SyncError,
}

impl From<wusel_core::provider::FileState> for SyncState {
    fn from(s: wusel_core::provider::FileState) -> Self {
        use wusel_core::provider::FileState as F;
        match s {
            F::OnlineOnly => SyncState::OnlineOnly,
            F::Cached => SyncState::Cached,
            F::Pinned => SyncState::Pinned,
            F::PinnedPending => SyncState::PinnedPending,
            F::PinnedStale => SyncState::PinnedStale,
            F::Modified => SyncState::Modified,
            F::Uploading => SyncState::Uploading,
            F::SyncError => SyncState::SyncError,
        }
    }
}

/// What *kind* of object this is, for the one distinction a file manager draws
/// with a badge rather than a sync emblem: a Team/Group folder's root versus an
/// ordinary folder. A separate axis from [`SyncState`] on purpose — a group
/// folder is still online-only or cached like anything else — so the two never
/// conflate into one value.
///
/// Filled from the engine's `is_group_folder_root`, the same read that answers
/// the sync state — and the same answer the FUSE frontend serves as
/// `user.wusel.kind`. The contract was on the wire before the engine could fill
/// it, which is what let the two lines of work meet without a client changing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// An ordinary file or folder.
    #[default]
    Plain,
    /// The root of a Team/Group folder.
    GroupFolder,
}

/// Read one length-prefixed frame. `Ok(None)` is a clean end of stream at a
/// frame boundary (the peer hung up), which the connection loop treats as "done"
/// rather than an error.
///
/// # Errors
/// A truncated frame, an I/O failure, or a length beyond [`MAX_FRAME`].
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    // A read of zero bytes right at the start is the ordinary "peer closed"
    // signal; anything read then cut short is a real truncation.
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds the {MAX_FRAME}-byte limit"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Write one length-prefixed frame.
///
/// # Errors
/// If the payload is larger than [`MAX_FRAME`], or the write fails.
pub fn write_frame(w: &mut impl Write, body: &[u8]) -> io::Result<()> {
    let len = u32::try_from(body.len())
        .ok()
        .filter(|n| *n <= MAX_FRAME)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "frame of {} bytes exceeds the {MAX_FRAME}-byte limit",
                    body.len()
                ),
            )
        })?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(body)?;
    Ok(())
}

/// A request from the client. Only the fields an op needs are meaningful; the
/// rest default and are ignored — `offset`/`len` for `fetch`/`write`, `to` for
/// `move`, `dir` for `create`, `size`/`mtime` for `setattr`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Read path: `stat | enumerate | fetch`. Write path: `write | create |
    /// publish | remove | move | setattr`. Change stream: `watch`.
    pub op: String,
    /// The account-relative path, e.g. `/Notes.txt` (a leading `/` is optional).
    pub path: String,
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub len: u32,
    /// `move` only: the destination path. A `write` carries its bytes in the
    /// next frame instead, the mirror of a `bytes` response.
    #[serde(default)]
    pub to: String,
    /// `create` only: make a directory rather than a file.
    #[serde(default)]
    pub dir: bool,
    /// `setattr` only: the new size (truncate/extend the write buffer).
    #[serde(default)]
    pub size: Option<u64>,
    /// `setattr` only: the new modification time, Unix seconds (may be negative
    /// for a pre-epoch timestamp).
    #[serde(default)]
    pub mtime: Option<i64>,
    /// `changes` only: return the change log from this sequence anchor onward.
    /// `0` (the default) asks for the whole retained log.
    #[serde(default)]
    pub since: u64,
}

/// One directory child, as `enumerate` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    /// The object's ETag — the same value `stat` reports. It is the **content
    /// version** a frontend must key on, so a file listed in a directory carries
    /// the identical version it does when stat'd or fetched. Without it a
    /// listing had to fall back to a size/mtime token, which never matched the
    /// ETag a download reported — so the OS judged every downloaded file stale
    /// and left a permanent "not current" (cloud) badge on it. Empty for an
    /// object not yet on the server (a deferred create), which has no ETag.
    #[serde(default)]
    pub etag: String,
    /// Nextcloud's stable file id — survives a rename, so it is the identity a
    /// frontend should persist (a File Provider's `NSFileProviderItemIdentifier`,
    /// say); the inode is stable only within one daemon session. `null` when the
    /// object is not on the server yet (a deferred create), in which case the
    /// frontend must mint a temporary identity until the upload lands.
    pub file_id: Option<u64>,
    /// Whether this entry is kept offline (pinned itself or covered by a pinned
    /// ancestor). Lets a frontend show a "make available offline" toggle and set
    /// the item's keep-downloaded content policy.
    #[serde(default)]
    pub pinned: bool,
    /// Whether the kept copy is out of date — the server has a newer version.
    /// Only meaningful together with `pinned`; lets a frontend flag a
    /// pinned-but-stale file (a distinct emblem).
    #[serde(default)]
    pub stale: bool,
    /// The full sync state — the superset that supersedes `pinned`/`stale`
    /// above (which stay for now so an unmigrated client keeps working).
    ///
    /// `None` when the object has no content state of its own: a plain
    /// directory has no bytes to be online-only or cached, so it gets no
    /// emblem. Absent from the JSON in that case rather than carrying a
    /// stand-in — defaulting it to `online-only` would put a cloud emblem on
    /// every ordinary folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<SyncState>,
    /// Whether this entry is a Team/Group folder root (see [`Kind`]). Named
    /// `folder_kind`, not `kind`, so it never collides with the `kind`
    /// discriminant that tags a [`Response`].
    #[serde(default)]
    pub folder_kind: Kind,
}

/// A response header frame. Untagged so the JSON reads as a flat object with a
/// discriminating `kind`, which is easier for a non-Rust client to parse than
/// serde's default externally-tagged form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// A single object's attributes (`stat`).
    Node {
        ok: bool,
        name: String,
        is_dir: bool,
        size: u64,
        mtime: i64,
        etag: String,
        /// Nextcloud's stable file id (see [`Entry::file_id`]); `null` until the
        /// object is on the server.
        file_id: Option<u64>,
        /// Whether the object is kept offline (see [`Entry::pinned`]).
        #[serde(default)]
        pinned: bool,
        /// Whether the kept copy is out of date (see [`Entry::stale`]).
        #[serde(default)]
        stale: bool,
        /// The full sync state, or `None` when the object has none (see
        /// [`Entry::state`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<SyncState>,
        /// Whether the object is a Team/Group folder root (see [`Kind`]). Named
        /// `folder_kind` to avoid colliding with the `kind` response tag.
        #[serde(default)]
        folder_kind: Kind,
    },
    /// A directory's children (`enumerate`).
    Entries { ok: bool, entries: Vec<Entry> },
    /// A read's content follows in the next frame; `len` is that frame's size
    /// (`fetch`).
    Bytes { ok: bool, len: u64 },
    /// A write was accepted into the buffer; `len` is how many bytes it took
    /// (`write`). Nothing reaches the server until `publish`.
    Written { ok: bool, len: u32 },
    /// An operation with nothing to return but its success (`publish`, `remove`,
    /// `move`).
    Done { ok: bool },
    /// The request could not be served.
    Error { ok: bool, error: ErrorKind },
    /// A server-side change, **pushed** to a `watch` subscriber — not a reply to
    /// a request. The frontend turns it into a re-enumeration (a File Provider's
    /// `signalEnumerator`); `path` is the account-relative path that changed.
    Changed {
        ok: bool,
        change: ChangeKind,
        path: String,
    },
    /// A **pulled** batch of changes at or after the requested sequence anchor,
    /// the reply to `changes`. `seq` is the current head sequence — the anchor to
    /// ask from next time. A replicated File Provider drives its working-set
    /// change enumeration from this: replay each entry as an update or a delete,
    /// then store `seq` as the new sync anchor.
    Changes {
        ok: bool,
        seq: u64,
        changes: Vec<ChangeEntry>,
    },
    /// A user-facing notice, **pushed** to a `notices` subscriber — not a reply to
    /// a request. Already localized to the user's language: the engine is the one
    /// place that translates (`wusel_core::desktop::Notice::localize`), so the
    /// title and body cross the wire ready to display and the client never speaks
    /// the user's language itself. The agent turns each into a Notification Center
    /// banner, with `severity` picking the sound/interruption level.
    Notice {
        ok: bool,
        severity: Severity,
        title: String,
        body: String,
    },
    /// The reply to `test-notice`: a sample notice was injected into the fan-out.
    /// `delivered` is how many `notices` subscribers received it — `0` means the
    /// notice went nowhere (no agent listening), which the self-test surfaces so a
    /// missing banner is explained rather than silent.
    Notified { ok: bool, delivered: u32 },
}

/// One entry in a pulled change log (`changes`) — the batch mirror of a pushed
/// [`Response::Changed`] event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeEntry {
    pub change: ChangeKind,
    pub path: String,
}

/// Why a request failed — a small, closed set a client can branch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The path did not resolve, or the object is gone.
    NotFound,
    /// Storage or transport failed.
    Io,
    /// The request itself was malformed (an unknown op, unparseable JSON).
    BadRequest,
}

/// What kind of change a `watch` event reports — the two the engine
/// distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// A directory's membership changed (added/removed/availability): re-list it.
    Entry,
    /// A file's content changed on the server: drop what is cached and re-fetch.
    Content,
}

/// How a [`Response::Notice`] reads, so the frontend shows good vs bad news
/// distinctly — the wire mirror of `wusel_core::desktop::Severity` (which is not
/// itself serializable, keeping the engine free of wire concerns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Good news — typically a prior problem resolved.
    Success,
    /// Needs attention, but nothing is lost.
    Warning,
    /// Something failed or is broken; the user should act.
    Error,
}

impl From<wusel_core::desktop::Severity> for Severity {
    fn from(s: wusel_core::desktop::Severity) -> Self {
        use wusel_core::desktop::Severity as Core;
        match s {
            Core::Success => Severity::Success,
            Core::Warning => Severity::Warning,
            Core::Error => Severity::Error,
        }
    }
}

impl Response {
    /// A `node` response from the parts of a row a frontend needs.
    // A flat constructor mirroring the wire struct's fields; grouping them would
    // only add an indirection the wire does not have.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn node(
        name: String,
        is_dir: bool,
        size: u64,
        mtime: i64,
        etag: String,
        file_id: Option<u64>,
        pinned: bool,
        stale: bool,
        state: Option<SyncState>,
        folder_kind: Kind,
    ) -> Self {
        Response::Node {
            ok: true,
            name,
            is_dir,
            size,
            mtime,
            etag,
            file_id,
            pinned,
            stale,
            state,
            folder_kind,
        }
    }

    /// An `entries` response.
    #[must_use]
    pub fn entries(entries: Vec<Entry>) -> Self {
        Response::Entries { ok: true, entries }
    }

    /// A `bytes` header; the caller writes the `len`-byte content frame next.
    #[must_use]
    pub fn bytes(len: u64) -> Self {
        Response::Bytes { ok: true, len }
    }

    /// A `written` response: the buffer took `len` bytes.
    #[must_use]
    pub fn written(len: u32) -> Self {
        Response::Written { ok: true, len }
    }

    /// A `done` response: the operation succeeded with nothing to return.
    #[must_use]
    pub fn done() -> Self {
        Response::Done { ok: true }
    }

    /// An `error` response.
    #[must_use]
    pub fn error(error: ErrorKind) -> Self {
        Response::Error { ok: false, error }
    }

    /// A `changed` event, pushed to a `watch` subscriber.
    #[must_use]
    pub fn changed(change: ChangeKind, path: String) -> Self {
        Response::Changed {
            ok: true,
            change,
            path,
        }
    }

    /// A `changes` response: the batch at or after the anchor, and the head `seq`.
    #[must_use]
    pub fn changes(seq: u64, changes: Vec<ChangeEntry>) -> Self {
        Response::Changes {
            ok: true,
            seq,
            changes,
        }
    }

    /// A `notice` event, pushed to a `notices` subscriber. `title` and `body` are
    /// already localized.
    #[must_use]
    pub fn notice(severity: Severity, title: String, body: String) -> Self {
        Response::Notice {
            ok: true,
            severity,
            title,
            body,
        }
    }

    /// A `notified` reply: a `test-notice` reached `delivered` subscribers.
    #[must_use]
    pub fn notified(delivered: u32) -> Self {
        Response::Notified {
            ok: true,
            delivered,
        }
    }

    /// Serialise this header to its frame bytes.
    ///
    /// # Errors
    /// If serialisation fails, which for these plain types it does not in
    /// practice.
    pub fn to_frame(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_round_trips_including_empty_and_binary() {
        for payload in [
            b"".to_vec(),
            b"hello".to_vec(),
            vec![0u8, 255, 1, 254, b'\n', b'"'],
        ] {
            let mut buf = Vec::new();
            write_frame(&mut buf, &payload).unwrap();
            // The 4-byte prefix must equal the body length, big-endian.
            assert_eq!(&buf[..4], &(payload.len() as u32).to_be_bytes());
            let mut cursor = std::io::Cursor::new(buf);
            let back = read_frame(&mut cursor).unwrap().unwrap();
            assert_eq!(back, payload);
        }
    }

    #[test]
    fn two_frames_read_back_in_order() {
        // A bytes response is two frames; the reader must get the header, then
        // the content, in that order out of one stream.
        let mut buf = Vec::new();
        write_frame(&mut buf, b"header").unwrap();
        write_frame(&mut buf, &[1, 2, 3, 4]).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor).unwrap().unwrap(), b"header");
        assert_eq!(read_frame(&mut cursor).unwrap().unwrap(), vec![1, 2, 3, 4]);
        // A clean end of stream at the boundary is `None`, not an error.
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn an_oversized_length_prefix_is_refused_without_allocating() {
        let mut framed = (MAX_FRAME + 1).to_be_bytes().to_vec();
        framed.extend_from_slice(b"...");
        let err = read_frame(&mut std::io::Cursor::new(framed)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn request_json_parses_with_and_without_fetch_fields() {
        // stat/enumerate omit offset/len; they must default rather than fail.
        let stat: Request = serde_json::from_str(r#"{"op":"stat","path":"/a/b"}"#).unwrap();
        assert_eq!(stat.op, "stat");
        assert_eq!(stat.path, "/a/b");
        assert_eq!((stat.offset, stat.len), (0, 0));

        let fetch: Request =
            serde_json::from_str(r#"{"op":"fetch","path":"/f","offset":3,"len":5}"#).unwrap();
        assert_eq!((fetch.offset, fetch.len), (3, 5));
    }

    #[test]
    fn response_shapes_carry_their_discriminant() {
        let node = Response::node(
            "Notes.txt".into(),
            false,
            5,
            42,
            "etag".into(),
            Some(42),
            true,
            true,
            Some(SyncState::PinnedStale),
            Kind::Plain,
        );
        let v: serde_json::Value = serde_json::from_slice(&node.to_frame().unwrap()).unwrap();
        // `kind: "node"` is the response-shape discriminant; the object's folder
        // kind rides in the separate `folder_kind` field, so the two never clash.
        assert_eq!(v["kind"], "node");
        assert_eq!(v["ok"], true);
        assert_eq!(v["name"], "Notes.txt");
        assert_eq!(v["size"], 5);
        assert_eq!(v["is_dir"], false);
        assert_eq!(v["file_id"], 42);
        assert_eq!(v["pinned"], true);
        assert_eq!(v["stale"], true);
        assert_eq!(v["state"], "pinned-stale");
        assert_eq!(v["folder_kind"], "plain");
        // The spelling a client's emblem table matches on: hyphenated, and
        // distinct from `pinned`, which is the whole point of the value.
        assert_eq!(
            serde_json::to_value(SyncState::PinnedPending).unwrap(),
            "pinned-pending"
        );

        // A never-uploaded child carries `file_id: null`, not a fake id.
        let entries = Response::entries(vec![Entry {
            name: "Sub Folder".into(),
            is_dir: true,
            size: 0,
            mtime: 1,
            etag: String::new(),
            file_id: None,
            pinned: false,
            stale: false,
            state: None,
            folder_kind: Kind::GroupFolder,
        }]);
        let v: serde_json::Value = serde_json::from_slice(&entries.to_frame().unwrap()).unwrap();
        assert_eq!(v["kind"], "entries");
        assert_eq!(v["entries"][0]["name"], "Sub Folder");
        assert_eq!(v["entries"][0]["is_dir"], true);
        assert!(v["entries"][0]["file_id"].is_null());
        assert_eq!(v["entries"][0]["pinned"], false);
        assert_eq!(v["entries"][0]["stale"], false);
        // A directory carries no content state, so the field is absent
        // rather than defaulted — the difference between "no emblem" and a
        // cloud emblem on every folder.
        assert!(v["entries"][0]["state"].is_null());
        assert_eq!(v["entries"][0]["folder_kind"], "group_folder");

        let bytes = Response::bytes(5);
        let v: serde_json::Value = serde_json::from_slice(&bytes.to_frame().unwrap()).unwrap();
        assert_eq!(v["kind"], "bytes");
        assert_eq!(v["len"], 5);

        let err = Response::error(ErrorKind::NotFound);
        let v: serde_json::Value = serde_json::from_slice(&err.to_frame().unwrap()).unwrap();
        assert_eq!(v["kind"], "error");
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "not_found");

        let changes = Response::changes(
            7,
            vec![
                ChangeEntry {
                    change: ChangeKind::Entry,
                    path: "Angebote/servus.md".into(),
                },
                ChangeEntry {
                    change: ChangeKind::Content,
                    path: "Angebote/README.md".into(),
                },
            ],
        );
        let v: serde_json::Value = serde_json::from_slice(&changes.to_frame().unwrap()).unwrap();
        assert_eq!(v["kind"], "changes");
        assert_eq!(v["seq"], 7);
        assert_eq!(v["changes"][0]["change"], "entry");
        assert_eq!(v["changes"][0]["path"], "Angebote/servus.md");
        assert_eq!(v["changes"][1]["change"], "content");
    }

    #[test]
    fn notice_carries_severity_title_and_body() {
        let n = Response::notice(
            Severity::Error,
            "Upload failed".into(),
            "'big.iso' could not be uploaded.".into(),
        );
        let v: serde_json::Value = serde_json::from_slice(&n.to_frame().unwrap()).unwrap();
        assert_eq!(v["kind"], "notice");
        assert_eq!(v["ok"], true);
        assert_eq!(v["severity"], "error");
        assert_eq!(v["title"], "Upload failed");
        assert_eq!(v["body"], "'big.iso' could not be uploaded.");
    }

    #[test]
    fn request_changes_since_defaults_to_zero() {
        let full: Request = serde_json::from_str(r#"{"op":"changes","path":""}"#).unwrap();
        assert_eq!(full.since, 0);
        let from: Request =
            serde_json::from_str(r#"{"op":"changes","path":"","since":42}"#).unwrap();
        assert_eq!(from.since, 42);
    }
}
