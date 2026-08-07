// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The decision core: what one object is doing, which step it is on, and who is
//! waiting behind it.
//!
//! This crate is a **mechanism**. It holds no connection, opens no file and
//! knows nothing about Nextcloud, FUSE or errno. Everything here is a decision
//! over plain data, which is what lets the whole design rest on one rule — the
//! deciding thread performs no I/O — and what lets a second frontend be a port
//! rather than a rewrite. See the `concurrency` and `frontends` pages in
//! `documentation/`.
//!
//! ## The two levels
//!
//! Keeping them apart is what stops the machine turning into an unreadable
//! graph:
//!
//! * **Occupancy, per object** ([`machine::Machine`]): idle, or busy with one
//!   flow plus a FIFO queue. Collisions are decided here, and the FIFO is where
//!   `write` → `flush` ordering comes from — without a lock.
//! * **The script of the running operation** ([`script`]): a straight sequence
//!   of steps with decisions between them, never a graph.
//!
//! ## Rust notes
//!
//! The enums below carry data (`Job::FetchRange { .. }`), which is Rust's
//! answer to "an enum plus a union" in C: the variant *is* the tag, and the
//! compiler will not let a payload be read for the wrong one. `match` without a
//! `_ =>` arm then turns "we thought of every case" from a claim into a build
//! error — which is why there is no catch-all arm anywhere in this crate, and
//! why adding a variant to [`Intent`] is deliberately noisy.

pub mod collision;
pub mod machine;
pub mod registry;
pub mod script;

pub use collision::{collision, Collision};
pub use machine::{Action, BusyObject, Machine, MachineSnapshot, Outcome};
pub use registry::{Buffer, Registry};
pub use script::{advance, Flow, Next, Step};

/// The identity of one filesystem object, as far as the machine is concerned.
///
/// Deliberately opaque. Under FUSE this is the inode; a Windows frontend would
/// map a file id onto it and a macOS one an `NSFileProviderItemIdentifier`.
/// The machine never does arithmetic on it, never assumes it is small, and
/// never assumes it means anything on the wire — the only operations it needs
/// are equality and hashing, which is exactly what the derives below grant.
///
/// A newtype rather than a bare `u64` on purpose: `ObjectId(7)` and a length of
/// `7` are different things, and the compiler should say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub u64);

impl Default for ObjectId {
    /// Not a valid object: `NodeFacts::default()` means "not found", and its id
    /// must not look like one that could be addressed.
    fn default() -> Self {
        Self(0)
    }
}

/// One caller waiting for an answer.
///
/// The machine never holds a reply object — that is a frontend type, and
/// `reply.data()` writes to a device, which is I/O. It holds this ticket
/// instead, and the frontend keeps the map from ticket to reply. That
/// indirection is what lets a worker thread answer, out of order, without the
/// machine ever touching the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(pub u64);

/// What a frontend asks the machine to accomplish.
///
/// The alphabet is **engine intents, not callbacks**. `flush`, `fsync` and
/// `release` are one intent because they are one operation; `getattr` and
/// `lookup` are both [`Intent::Enumerate`]-adjacent metadata reads. A frontend
/// maps its platform's callbacks onto these, and the machine never learns their
/// names — which is the difference between porting and rewriting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Read a range of an object's content.
    Fetch { offset: u64, len: u32 },
    /// Put bytes into the object's write buffer, creating it if needed. Nothing
    /// reaches the server until [`Intent::Publish`].
    Write { offset: u64, len: u32 },
    /// Read one object's metadata.
    Stat,
    /// Resolve one name inside a directory.
    Lookup { name: String },
    /// The per-object state an OS integration draws an emblem from.
    State,
    /// Change attributes: resize the buffer, set the modification time, or
    /// both. One intent rather than two because the kernel sends them in one
    /// callback, and splitting them would mean chaining two flows to answer it.
    SetAttr {
        size: Option<u64>,
        mtime: Option<i64>,
    },
    /// List a directory, filling it from the server if it has never been listed.
    Enumerate,
    /// Create a local object; nothing reaches the server until it is published.
    Materialise { name: String, dir: bool },
    /// Send the write buffer to the server (`flush`, `fsync`, `release`).
    Publish,
    /// Delete an object and its subtree.
    Remove { name: String },
    /// Rename or move a child of this object.
    ///
    /// The kernel names both ends, so the flow is keyed on the source parent —
    /// whose membership is what changes — and the child is resolved by the
    /// script's first step.
    Move {
        from_name: String,
        to_parent: ObjectId,
        to_name: String,
    },
    /// Re-fetch an object because the server said it changed.
    Refresh,
    /// Re-list a directory in the background, because what we served was past
    /// its revalidation interval. Nobody waits for this: the caller already has
    /// an answer, which is the whole point of serving stale rather than
    /// blocking on the server.
    Relist,
}

impl Intent {
    /// A short, stable, name-free label for diagnostics — the variant only,
    /// never the fields. Some fields (a file or directory name) are the user's
    /// private data, and a support bundle must not carry them.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Intent::Fetch { .. } => "fetch",
            Intent::Write { .. } => "write",
            Intent::Stat => "stat",
            Intent::Lookup { .. } => "lookup",
            Intent::State => "state",
            Intent::SetAttr { .. } => "setattr",
            Intent::Enumerate => "enumerate",
            Intent::Materialise { .. } => "materialise",
            Intent::Publish => "publish",
            Intent::Remove { .. } => "remove",
            Intent::Move { .. } => "move",
            Intent::Refresh => "refresh",
            Intent::Relist => "relist",
        }
    }
}

/// One request as it arrives from a dispatch thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub id: RequestId,
    pub object: ObjectId,
    pub intent: Intent,
}

/// Which pool runs a job.
///
/// The split is not decoration. Readers are separate from the writer because
/// WAL allows many readers alongside one writer, and the common operations are
/// reads: a metadata lookup must not wait behind a write that a virus scanner
/// is holding up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Executor {
    /// One of N database readers.
    DbRead,
    /// The single database writer.
    DbWrite,
    /// Network I/O (WebDAV).
    Net,
    /// Local file I/O (write buffers, cache blobs).
    FileIo,
}

/// A unit of work the machine hands out. Plain data: the executor decides *how*,
/// the machine only says *what*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// Fetch an object's row. The reply also reports whether a cached copy is
    /// current, because deciding that means stating a file — I/O, and therefore
    /// the worker's job, not ours. Answering it here is what keeps the
    /// three-way choice in [`Step::FetchDecide`] a pure decision.
    ReadNode { object: ObjectId },
    /// List a directory's children from the state database.
    ReadChildren { object: ObjectId },
    /// Resolve one child by name.
    ReadChild { parent: ObjectId, name: String },
    /// Work out the object's emblem state. `buffered` is what the machine
    /// already knows and the worker must not go looking for — an unsaved edit
    /// outranks everything the disk could say.
    ReadState { object: ObjectId, buffered: bool },
    /// Resize the write buffer.
    TruncateBuffer { object: ObjectId, size: u64 },
    /// Record a modification time in the state.
    RecordMtime { object: ObjectId, mtime: i64 },
    /// Read a range out of the object's write buffer.
    ReadBuffer {
        object: ObjectId,
        offset: u64,
        len: u32,
    },
    /// Read a range out of the cached blob.
    ReadBlob {
        object: ObjectId,
        offset: u64,
        len: u32,
    },
    /// Fetch a range from the server.
    FetchRange {
        object: ObjectId,
        offset: u64,
        len: u32,
    },
    /// List a directory on the server and reconcile it into the state.
    ListRemote { object: ObjectId },
    /// How large the write buffer currently is.
    BufferSize { object: ObjectId },
    /// Send the buffer to the server under the given precondition.
    Upload {
        object: ObjectId,
        size: u64,
        precondition: Precondition,
        /// Travels with the upload rather than being applied afterwards: a
        /// second round-trip to set a timestamp is one the server need not be
        /// asked for.
        mtime: Option<i64>,
    },
    /// Resolve a rejected upload: merge if possible, otherwise a second copy.
    ResolveConflict { object: ObjectId },
    /// Record a new version in the state database.
    RecordVersion {
        object: ObjectId,
        etag: String,
        size: u64,
    },
    /// Copy the buffer into the read cache, so the next read stays local.
    StoreBlob { object: ObjectId },
    /// Durably record that this object's buffer is owed to the server, and make
    /// the buffer itself durable — the commit that lets `flush` answer before the
    /// upload runs, without risking the bytes on a crash. Carries the
    /// precondition the eventual upload asserts.
    MarkPending {
        object: ObjectId,
        base_etag: String,
        mtime: Option<i64>,
    },
    /// The upload landed: forget the pending record.
    ClearPending { object: ObjectId },
    /// A detached upload failed after the change was already committed and
    /// `flush` answered. Mark the pending record so the failure is visible and
    /// the bytes are known to still need sending, rather than let the flow
    /// vanish and the record look as if it were still queued.
    SetUploadError { object: ObjectId, message: String },
    /// Drop the write buffer's file.
    DiscardBuffer { object: ObjectId },
    /// Create an empty write buffer.
    CreateBuffer { object: ObjectId },
    /// Put bytes into the write buffer.
    WriteBuffer {
        object: ObjectId,
        offset: u64,
        len: u32,
    },
    /// Stream the object's current content into a fresh write buffer, so an
    /// edit starts from the whole file and a later merge has a base.
    HydrateBuffer { object: ObjectId },
    /// Stream the object's current content into the read cache.
    HydrateCache { object: ObjectId },
    /// Add a local row for a newly created object.
    InsertNode {
        parent: ObjectId,
        name: String,
        dir: bool,
    },
    /// Create a directory on the server.
    CreateRemoteDir { parent: ObjectId, name: String },
    /// Delete an object on the server.
    DeleteRemote { object: ObjectId },
    /// Move an object on the server.
    MoveRemote {
        object: ObjectId,
        to_parent: ObjectId,
        to_name: String,
    },
    /// Remove an object's rows, its pins and its eviction markers.
    RemoveRows { object: ObjectId },
    /// Move an object's rows, keeping the identity alive so open handles and a
    /// pending buffer survive.
    MoveRows {
        object: ObjectId,
        to_parent: ObjectId,
        to_name: String,
    },
}

impl Job {
    /// Which pool runs this job. A `match` with no catch-all: a new job cannot
    /// be added without deciding where it runs.
    #[must_use]
    pub fn executor(&self) -> Executor {
        match self {
            Job::ReadNode { .. }
            | Job::ReadChildren { .. }
            | Job::ReadChild { .. }
            | Job::ReadState { .. } => Executor::DbRead,
            Job::RecordVersion { .. }
            | Job::InsertNode { .. }
            | Job::RemoveRows { .. }
            | Job::MoveRows { .. }
            | Job::MarkPending { .. }
            | Job::ClearPending { .. }
            | Job::SetUploadError { .. }
            | Job::RecordMtime { .. } => Executor::DbWrite,
            Job::FetchRange { .. }
            | Job::ListRemote { .. }
            | Job::Upload { .. }
            | Job::ResolveConflict { .. }
            | Job::CreateRemoteDir { .. }
            | Job::DeleteRemote { .. }
            | Job::MoveRemote { .. } => Executor::Net,
            Job::HydrateBuffer { .. } | Job::HydrateCache { .. } => Executor::Net,
            Job::ReadBuffer { .. }
            | Job::ReadBlob { .. }
            | Job::BufferSize { .. }
            | Job::StoreBlob { .. }
            | Job::DiscardBuffer { .. }
            | Job::CreateBuffer { .. }
            | Job::WriteBuffer { .. }
            | Job::TruncateBuffer { .. } => Executor::FileIo,
        }
    }

    /// Whether abandoning this job mid-flight is safe.
    ///
    /// A step that only produces a value may be dropped the moment nobody wants
    /// the answer. A step with a side effect may not: an upload that is already
    /// running has to finish and then be undone, or it leaves half an object on
    /// the server. This is the whole content of "abort is a request, not an
    /// act" — and it is a property of the job, so it belongs here rather than
    /// in whoever happens to be cancelling.
    #[must_use]
    pub fn is_droppable(&self) -> bool {
        match self {
            Job::ReadNode { .. }
            | Job::ReadChildren { .. }
            | Job::ReadBuffer { .. }
            | Job::ReadBlob { .. }
            | Job::FetchRange { .. }
            | Job::ListRemote { .. }
            | Job::BufferSize { .. }
            | Job::ReadChild { .. }
            | Job::ReadState { .. } => true,
            // A hydration is the one streamed step where dropping the future
            // genuinely stops the transfer, which is what makes cancelling a
            // large download worth anything at all.
            Job::HydrateBuffer { .. } | Job::HydrateCache { .. } => true,
            Job::Upload { .. }
            | Job::ResolveConflict { .. }
            | Job::RecordVersion { .. }
            | Job::StoreBlob { .. }
            | Job::DiscardBuffer { .. }
            | Job::CreateBuffer { .. }
            | Job::WriteBuffer { .. }
            | Job::InsertNode { .. }
            | Job::CreateRemoteDir { .. }
            | Job::DeleteRemote { .. }
            | Job::MoveRemote { .. }
            | Job::RemoveRows { .. }
            | Job::MoveRows { .. }
            | Job::MarkPending { .. }
            | Job::ClearPending { .. }
            | Job::SetUploadError { .. }
            | Job::TruncateBuffer { .. }
            | Job::RecordMtime { .. } => false,
        }
    }
}

/// What we may legitimately assert about the server when uploading.
///
/// The distinction that matters: an object we have never sent gets
/// [`Precondition::MustNotExist`], one whose version we know gets
/// [`Precondition::Match`], and one that exists with a version we do not know
/// goes out [`Precondition::Unconditional`] — never claiming it is absent,
/// which would report a perfectly ordinary save as a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    MustNotExist,
    Match(String),
    Unconditional,
}

/// What came back from a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion {
    /// A node row and everything the next decision needs.
    Node(NodeFacts),
    /// Bytes were produced and the waiting requests have been answered by the
    /// worker that produced them.
    Bytes,
    /// A listing completed.
    Listed,
    /// A per-object state was worked out.
    StateKnown,
    /// The buffer is this many bytes.
    Size(u64),
    /// An upload landed, with the version the server assigned.
    Uploaded { etag: String, size: u64 },
    /// The server refused the precondition: somebody else changed the object.
    Rejected,
    /// A step with nothing to report finished.
    Done,
    /// A step failed.
    Failed(Failure),
}

/// What one database read tells us about an object.
///
/// Every field here exists because some decision downstream turns on it — and
/// answering them all in one read is what keeps those decisions free of I/O.
/// `blob_current` is the clearest case: deciding whether a cached copy is still
/// good means stating a file, so the worker settles it and the machine merely
/// reads the verdict.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NodeFacts {
    /// Which object this is. Filled when the row was resolved by name, so a
    /// script that started from a parent can address the child it found —
    /// `unlink` and `rename` arrive as (parent, name), never as an identity.
    pub id: ObjectId,
    /// The state knows this object.
    pub found: bool,
    /// Which directory holds it — needed to re-list after a creation lands.
    pub parent: ObjectId,
    /// An ephemeral editor or office file, matched by name. A property of the
    /// *name*, so it is re-decided after a rename — which is exactly what turns
    /// an ignored temp file into a published document.
    pub ignored: bool,
    pub dir: bool,
    /// The server permits writing here (a read-only share does not).
    pub writable: bool,
    pub etag: String,
    pub size: u64,
    /// A cached copy exists and matches `etag`.
    pub blob_current: bool,
    /// A *complete but outdated* local copy exists, and the engine has decided
    /// it may be served rather than fetching the current version.
    ///
    /// The decision is not the machine's — it turns on the connection's cost and
    /// on what the user configured, and both are I/O. What arrives here is the
    /// answer: use it, or go to the server.
    pub stale_copy_ok: bool,
    /// The object exists on the server. False for a local creation that has
    /// never been published — there is nothing to delete or overwrite there.
    pub materialised: bool,
    /// This directory has been listed at least once, so something can be served
    /// without waiting for the server.
    pub children_loaded: bool,
    /// The listing is past its revalidation interval, or a write of ours
    /// invalidated it.
    pub listing_stale: bool,
}

/// Why an operation could not be completed — in terms the machine can hold.
///
/// Not an errno: that is a platform's vocabulary, and this crate does not speak
/// one. The frontend maps these onto `Errno`, `NTSTATUS` or `NSError`, which is
/// the seam that keeps the machine portable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// No such object.
    NotFound,
    /// The object disappeared under us; the caller should stop using its handle.
    Stale,
    /// A directory was required, or must not have been.
    WrongKind,
    /// The server does not permit writing here.
    NotWritable,
    /// The request itself made no sense (a negative offset, a non-UTF-8 name).
    Invalid,
    /// Storage or transport failed.
    Io,
    /// The caller gave up before the work finished. Not an error the caller will
    /// ever see — it is dead, which is why it was abandoned — but the kernel
    /// still needs *an* answer to release the resources it is holding for the
    /// request (a locked page behind an outstanding `read`), so "interrupted" is
    /// what it is told.
    Interrupted,
    /// A step failed in a way retrying will not fix. The machine treats it like
    /// any other failure; what it means is decided by the executor that raised
    /// it — for an upload, that the change is parked rather than retried.
    Permanent,
}
