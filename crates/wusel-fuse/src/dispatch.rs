// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Parking a reply while its work runs elsewhere.
//!
//! A dispatch thread translates one callback into an [`Intent`], parks the
//! reply object here under a ticket, and returns at once. The substrate answers
//! whenever the work finishes — on whichever thread finished it, in whatever
//! order they finish — and the pump below turns that answer back into the
//! typed reply the kernel is waiting for.
//!
//! Two properties come out of this arrangement rather than being arranged for.
//! The reply objects are `Send` and carry their own request id, so answering
//! out of order is not a trick but the protocol. And several callers can share
//! one ticket list entry — Join hands the same answer to everybody who was
//! waiting on it, which is how one transfer serves two readers.
//!
//! [`Intent`]: wusel_fsm::Intent

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use fuser::{
    Errno, FileHandle, FileType, FopenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyWrite, ReplyXattr,
};
use wusel_core::provider::FileState;
use wusel_core::runtime::{Answered, Payload};
use wusel_core::state::NodeRow;
use wusel_fsm::{Failure, Outcome, RequestId};

use crate::fs::{reply_xattr, to_attr, DirStreams, GENERATION, TTL};

/// Map a machine failure onto the errno the kernel expects.
///
/// The machine does not speak errno on purpose — it is one platform's
/// vocabulary — so this is the seam where it is put back on.
#[must_use]
pub fn errno_for(failure: Failure) -> Errno {
    match failure {
        Failure::NotFound => Errno::ENOENT,
        Failure::Stale => Errno::ESTALE,
        Failure::WrongKind => Errno::EISDIR,
        Failure::NotWritable => Errno::EACCES,
        Failure::Invalid => Errno::EINVAL,
        Failure::Io => Errno::EIO,
        // A read the caller gave up on. It is dead and will not see this, but the
        // kernel needs it to unlock the page and let the next reader re-fetch.
        Failure::Interrupted => Errno::EINTR,
        // Surfaced only on background work nobody is waiting for; if it ever did
        // reach the kernel, a plain I/O error is the honest answer.
        Failure::Permanent => Errno::EIO,
    }
}

/// A reply waiting for its work to finish.
///
/// One variant per shape the kernel expects back, because a reply object can
/// only be completed one way and the compiler should enforce which.
pub enum Pending {
    Attr(ReplyAttr),
    Entry(ReplyEntry),
    Data(ReplyData),
    Empty(ReplyEmpty),
    Written(ReplyWrite),
    /// The xattr protocol asks twice: first for the size, then for the value.
    Xattr {
        reply: ReplyXattr,
        size: u32,
    },
    /// `listxattr`: the one name we expose, but only when the object actually
    /// has a state — so `getfattr -d` on an unpinned directory shows nothing
    /// rather than an empty-valued attribute.
    XattrList {
        reply: ReplyXattr,
        size: u32,
    },
    Created(ReplyCreate),
    /// A directory chunk. The listing is assembled here and kept for the
    /// stream's continuation chunks — a snapshot, so a background refresh
    /// between two chunks cannot shift the offsets and skip or duplicate
    /// entries.
    Dir {
        reply: ReplyDirectory,
        ino: u64,
        fh: u64,
        start: usize,
    },
}

/// What the pump needs besides the answer itself.
pub struct PumpContext {
    pub dirs: Arc<Mutex<DirStreams>>,
    /// Expose the synthetic indexer-exclusion markers at the root.
    pub markers: bool,
}

impl Pending {
    /// Complete this reply from the substrate's answer.
    fn complete(self, outcome: Outcome, payload: &Payload, ctx: &PumpContext) {
        let failure = match outcome {
            Outcome::Ok => None,
            Outcome::Failed(f) => Some(errno_for(f)),
        };
        match self {
            Pending::Attr(reply) => match (failure, node(payload)) {
                (None, Some(n)) => reply.attr(&TTL, &to_attr(n)),
                (Some(e), _) => reply.error(e),
                (None, None) => reply.error(Errno::EIO),
            },
            Pending::Entry(reply) => match (failure, node(payload)) {
                (None, Some(n)) => reply.entry(&TTL, &to_attr(n), GENERATION),
                (Some(e), _) => reply.error(e),
                (None, None) => reply.error(Errno::EIO),
            },
            Pending::Data(reply) => match (failure, payload) {
                (None, Payload::Bytes(b)) => reply.data(b),
                (Some(e), _) => reply.error(e),
                // A read that succeeded without bytes is a zero-length range,
                // which is a perfectly ordinary answer.
                (None, _) => reply.data(&[]),
            },
            Pending::Empty(reply) => match failure {
                None => reply.ok(),
                Some(e) => reply.error(e),
            },
            Pending::Written(reply) => match (failure, payload) {
                (None, Payload::Written(n)) => reply.written(*n),
                (Some(e), _) => reply.error(e),
                (None, _) => reply.error(Errno::EIO),
            },
            Pending::Xattr { reply, size } => match (failure, payload) {
                (None, Payload::State(state)) => {
                    reply_xattr(reply, state_bytes(*state).as_bytes(), size);
                }
                // No state is not an error the caller should see as one: an
                // unpinned directory simply has no emblem.
                (None, _) | (Some(Errno::ENOENT), _) => reply.error(Errno::ENODATA),
                (Some(e), _) => reply.error(e),
            },
            Pending::XattrList { reply, size } => {
                let mut list = Vec::new();
                if failure.is_none() && matches!(payload, Payload::State(_)) {
                    list.extend_from_slice(crate::fs::STATE_XATTR.as_bytes());
                    list.push(0); // NUL-separated and NUL-terminated
                }
                reply_xattr(reply, &list, size);
            }
            Pending::Created(reply) => match (failure, node(payload)) {
                (None, Some(n)) => reply.created(
                    &TTL,
                    &to_attr(n),
                    GENERATION,
                    FileHandle(0),
                    FopenFlags::empty(),
                ),
                (Some(e), _) => reply.error(e),
                (None, None) => reply.error(Errno::EIO),
            },
            Pending::Dir {
                reply,
                ino,
                fh,
                start,
            } => serve_dir(reply, ino, fh, start, failure, payload, ctx),
        }
    }
}

impl Pending {
    /// Answer with an error, for when the work could not even be submitted.
    pub fn fail(self, errno: Errno) {
        match self {
            Pending::Attr(r) => r.error(errno),
            Pending::Entry(r) => r.error(errno),
            Pending::Data(r) => r.error(errno),
            Pending::Empty(r) => r.error(errno),
            Pending::Written(r) => r.error(errno),
            Pending::Xattr { reply, .. } | Pending::XattrList { reply, .. } => reply.error(errno),
            Pending::Created(r) => r.error(errno),
            Pending::Dir { reply, .. } => reply.error(errno),
        }
    }
}

/// Answer one directory chunk, taking a fresh snapshot when the stream starts.
fn serve_dir(
    mut reply: ReplyDirectory,
    ino: u64,
    fh: u64,
    start: usize,
    failure: Option<Errno>,
    payload: &Payload,
    ctx: &PumpContext,
) {
    if let Some(e) = failure {
        return reply.error(e);
    }
    let Payload::Entries(children) = payload else {
        return reply.error(Errno::EIO);
    };
    let mut entries: Vec<(u64, FileType, String)> = Vec::with_capacity(children.len() + 2);
    // `.` and `..` first; the parent comes from any child, or from the
    // directory itself when it is empty.
    entries.push((ino, FileType::Directory, ".".to_string()));
    let parent = children.first().map_or(ino, |c| c.parent);
    entries.push((parent, FileType::Directory, "..".to_string()));
    let at_root = ino == wusel_core::state::ROOT_INODE;
    for c in children {
        // A freedesktop wastebasket lives at the filesystem root as `.Trash` or
        // `.Trash-<uid>`. We do not host one (see the `create`/`mkdir`/`rename`
        // guards), but one can already exist on the server — created by another
        // client, or before this guard shipped. Hide it from the root listing so
        // a file manager neither shows it nor treats it as a usable trash to move
        // deletions into; combined with the `lookup` ENOENT below, it is as good
        // as absent, and "Move to Trash" is forced to fall back to a real delete.
        if at_root && crate::fs::is_trash_name(&c.name) {
            continue;
        }
        let kind = if c.is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        entries.push((c.inode, kind, c.name.clone()));
    }
    if ctx.markers && ino == wusel_core::state::ROOT_INODE {
        for (mino, mname) in crate::fs::MARKERS {
            entries.push((mino, FileType::RegularFile, mname.to_string()));
        }
    }

    crate::fs::serve_chunk(&entries, start, &mut reply);
    // Keep it for this stream's continuation chunks.
    if fh != 0 {
        let mut dirs = ctx.dirs.lock().unwrap_or_else(|e| e.into_inner());
        dirs.remember(fh, entries);
    }
    reply.ok();
}

fn node(payload: &Payload) -> Option<&NodeRow> {
    match payload {
        Payload::Node(n) => Some(n),
        Payload::None
        | Payload::Bytes(_)
        | Payload::Entries(_)
        | Payload::Written(_)
        | Payload::State(_) => None,
    }
}

fn state_bytes(state: FileState) -> &'static str {
    state.as_xattr()
}

/// The ticket list: replies parked while their work runs.
pub struct Replies {
    pending: Mutex<HashMap<RequestId, Pending>>,
    next: AtomicU64,
}

impl Replies {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }

    /// Park a reply and return the ticket to submit with.
    pub fn park(&self, reply: Pending) -> RequestId {
        let id = RequestId(self.next.fetch_add(1, Ordering::Relaxed));
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, reply);
        id
    }

    /// How many replies are parked right now.
    ///
    /// For diagnostics: this held against the kernel connection's `waiting`
    /// count is what tells a lost reply from ordinary in-flight work — the
    /// difference that pinpointed the read that never got answered.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Give back a parked reply — used when submitting fails, so the caller is
    /// told rather than left waiting.
    pub fn take(&self, id: RequestId) -> Option<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
    }

    fn deliver(&self, answered: &Answered, ctx: &PumpContext) {
        for id in &answered.requests {
            if let Some(pending) = self.take(*id) {
                pending.complete(answered.outcome, &answered.payload, ctx);
            }
        }
    }
}

impl Default for Replies {
    fn default() -> Self {
        Self::new()
    }
}

/// Drain answers and complete the replies they belong to.
///
/// Its own thread, and deliberately not the deciding one: `reply.data()` writes
/// to `/dev/fuse`, which is I/O, and the whole design turns on the decider
/// never doing any.
pub fn spawn_pump(
    answers: std::sync::mpsc::Receiver<Answered>,
    replies: Arc<Replies>,
    ctx: PumpContext,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("wusel-fuse-replies".into())
        .spawn(move || {
            while let Ok(answered) = answers.recv() {
                replies.deliver(&answered, &ctx);
            }
        })
        .expect("spawn the reply pump")
}
