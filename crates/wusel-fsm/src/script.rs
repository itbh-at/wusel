// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The script of a running operation: a straight sequence of steps with
//! decisions between them, never a graph.
//!
//! [`advance`] is the machine itself, and it is a **pure function**: no I/O, no
//! clock, no randomness. That is what makes the whole design testable without a
//! mount, a server or a database — the tests in `tests/script_walk.rs` drive a
//! complete upload, conflict and all, by handing this function a scripted
//! sequence of completions.
//!
//! Between any two steps nothing waits here. A step is handed out, somebody
//! else blocks on it, and its completion comes back as an argument.

use crate::registry::Facts;
use crate::{Completion, Failure, Intent, Job, NodeFacts, ObjectId, Precondition, RequestId};

/// Where a running operation stands.
///
/// One step is outstanding at a time — the `Vec` of waiters is the only
/// multiplicity, and it is how Join works: several callers ride one flow and
/// are answered together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flow {
    pub object: ObjectId,
    pub intent: Intent,
    pub step: Step,
    /// Everyone waiting for this flow to finish.
    pub waiters: Vec<RequestId>,
    /// Somebody asked for this flow to be given up. Checked between steps —
    /// never mid-step, because a step with a side effect has to finish and then
    /// be undone rather than be shot in the back.
    pub abort: bool,
    /// Facts carried from one step to the next (a size measured, a version the
    /// server assigned). Kept here rather than re-read, so a step's decision
    /// depends only on what it was given.
    pub carry: Carry,
}

/// What one step tells the next.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Carry {
    pub node: NodeFacts,
    pub size: u64,
    pub etag: String,
}

/// Which step of its script a flow is on.
///
/// Flat across all intents rather than one enum per intent: [`advance`] then has
/// a single `match` the compiler can check for completeness, which is the point
/// of the design — a new step cannot be added without deciding what follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    // Fetch — read a range.
    FetchNode,
    FetchBytes,
    FetchPrune,
    // Write — put bytes into the buffer, hydrating a base first if needed.
    WriteNode,
    WritePrepare,
    WriteBytes,
    // Stat — one row.
    StatNode,
    // Lookup — resolve a name inside a directory.
    LookNode,
    LookRemote,
    LookChild,
    // State — what an emblem should show.
    StateRead,
    // SetAttr — resize the buffer, set the mtime, or both.
    AttrNode,
    AttrPrepare,
    AttrTruncate,
    AttrMtime,
    AttrFinal,
    // Enumerate — list a directory.
    EnumNode,
    EnumRemote,
    EnumChildren,
    // Materialise — a file is deferred, a directory is not.
    MatInsert,
    MatBuffer,
    MatRemote,
    MatList,
    MatLookup,
    // Publish — the upload, and the longest script.
    PubNode,
    PubCommit,
    PubSize,
    PubUpload,
    PubConflict,
    PubRecord,
    PubStore,
    PubReload,
    PubClear,
    PubError,
    PubDiscard,
    // Remove.
    RemNode,
    RemRemote,
    RemBuffer,
    RemRows,
    // Move.
    MovNode,
    MovRemote,
    MovRows,
    // Refresh — the server said this changed.
    RefNode,
    RefHydrate,
    // Relist — a background listing nobody waits for.
    RelistRemote,
}

/// What the machine should do with a flow after a step finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Hand this out and wait for its completion.
    Do(Job),
    /// Answer everyone waiting **now**, with success, then keep the flow running
    /// with this job and nobody waiting. This is what makes an upload
    /// asynchronous: `flush` is answered the moment the change is durable, and
    /// the transfer to the server continues on its own behind it.
    AnswerThen(Job),
    /// The flow finished; answer everyone waiting and release the object.
    Done,
    /// The flow failed; report this and release the object.
    Fail(Failure),
    /// The flow was given up. Nobody is waiting any more, so there is nothing
    /// to answer — the object is simply released.
    Abandoned,
}

/// Begin a script.
///
/// Some intents are settled without any job at all, and that is not an
/// optimisation but the correct answer: publishing an object with no buffer
/// open has genuinely nothing to do, and saying so here keeps a pointless
/// database round-trip off the hot path of every `release`.
#[must_use]
pub fn start(object: ObjectId, intent: Intent, waiter: RequestId, facts: &Facts) -> (Flow, Next) {
    let (step, next) = match &intent {
        Intent::Fetch { .. } => (Step::FetchNode, Next::Do(Job::ReadNode { object })),
        Intent::Write { .. } => (Step::WriteNode, Next::Do(Job::ReadNode { object })),
        Intent::Stat => (Step::StatNode, Next::Do(Job::ReadNode { object })),
        Intent::Lookup { .. } => (Step::LookNode, Next::Do(Job::ReadNode { object })),
        Intent::State => (
            Step::StateRead,
            Next::Do(Job::ReadState {
                object,
                // An unsaved edit outranks anything the disk could say, and the
                // machine is the only place that knows about one.
                buffered: facts.buffer_open,
            }),
        ),
        Intent::SetAttr { .. } => (Step::AttrNode, Next::Do(Job::ReadNode { object })),
        Intent::Enumerate => (Step::EnumNode, Next::Do(Job::ReadNode { object })),
        Intent::Materialise { name, dir } => {
            if *dir {
                // A directory has to exist on the server before anything can be
                // put into it, so it is created there first.
                (
                    Step::MatRemote,
                    Next::Do(Job::CreateRemoteDir {
                        parent: object,
                        name: name.clone(),
                    }),
                )
            } else {
                // A file is deferred: nothing reaches the server until it is
                // published, so a probe file created and deleted in between
                // costs no round-trip at all.
                (
                    Step::MatInsert,
                    Next::Do(Job::InsertNode {
                        parent: object,
                        name: name.clone(),
                        dir: false,
                    }),
                )
            }
        }
        Intent::Publish => publish_start(object, facts),
        // Both arrive from the kernel as (parent, name), never as an identity,
        // so both begin by resolving the child they are about to act on.
        Intent::Remove { name } => (
            Step::RemNode,
            Next::Do(Job::ReadChild {
                parent: object,
                name: name.clone(),
            }),
        ),
        Intent::Move { from_name, .. } => (
            Step::MovNode,
            Next::Do(Job::ReadChild {
                parent: object,
                name: from_name.clone(),
            }),
        ),
        Intent::Refresh => (Step::RefNode, Next::Do(Job::ReadNode { object })),
        Intent::Relist => (Step::RelistRemote, Next::Do(Job::ListRemote { object })),
    };
    let flow = Flow {
        object,
        intent,
        step,
        waiters: vec![waiter],
        abort: false,
        carry: Carry::default(),
    };
    (flow, next)
}

/// The three ways a publish ends before it begins.
fn publish_start(object: ObjectId, facts: &Facts) -> (Step, Next) {
    if !facts.buffer_open {
        // Already published, or never written. `flush` is idempotent by
        // contract, and every `release` of a read-only handle lands here.
        (Step::PubDiscard, Next::Done)
    } else if !facts.buffer_dirty {
        (Step::PubDiscard, Next::Do(Job::DiscardBuffer { object }))
    } else {
        (Step::PubNode, Next::Do(Job::ReadNode { object }))
    }
}

/// The machine: a step finished — what happens next?
///
/// Pure. Every input is an argument and every output is a return value, which
/// is why a full upload including a conflict can be walked in a unit test.
#[must_use]
pub fn advance(mut flow: Flow, completion: Completion, facts: &Facts) -> (Flow, Next) {
    // Abort is checked *between* steps, which is the only place it is safe: the
    // step that just finished has already had whatever effect it was going to
    // have, and the next one has not started.
    if flow.abort {
        return (flow, Next::Abandoned);
    }
    // One place where a failed step ends its flow. For a publish this is also
    // what keeps the user's bytes: nothing here discards the buffer, so the
    // next flush retries instead of silently losing the edit.
    if let Completion::Failed(f) = completion {
        // One exception, and it is not error handling but bookkeeping: a read
        // that found the object gone on the server drops the row before
        // reporting. Left in place, a file manager sitting in that directory
        // keeps reading it and hammering the server with 404s.
        if flow.step == Step::FetchBytes && matches!(f, Failure::Stale) {
            let object = flow.object;
            flow.step = Step::FetchPrune;
            return (flow, Next::Do(Job::RemoveRows { object }));
        }
        // An upload that failed *after* the local commit — flush was already
        // answered, and a pending record already exists. Steps before the commit
        // (`PubNode`, `PubCommit`) have no record yet, so an ordinary failure
        // there still fails the flush.
        let past_commit = matches!(flow.intent, Intent::Publish)
            && matches!(
                flow.step,
                Step::PubSize
                    | Step::PubUpload
                    | Step::PubConflict
                    | Step::PubRecord
                    | Step::PubStore
                    | Step::PubReload
                    | Step::PubClear
            );
        if past_commit && matches!(f, Failure::Permanent) {
            // Retrying will not help (wrong permissions, a conflict, no quota):
            // park the record as an error so it is visible and not retried, and
            // keep the buffer so it can still be resolved.
            let object = flow.object;
            flow.step = Step::PubError;
            return (
                flow,
                Next::Do(Job::SetUploadError {
                    object,
                    message: format!("{f:?}"),
                }),
            );
        }
        // A transient failure past the commit just ends the flow. The pending
        // record is left as `pending` and the buffer is kept, so the uploader
        // retries it. A failure before the commit fails the flush the same way.
        return (flow, Next::Fail(f));
    }

    let object = flow.object;
    let next = match flow.step {
        // --- Fetch ---------------------------------------------------------
        Step::FetchNode => match completion {
            Completion::Node(n) => {
                let r = fetch_source(&flow.intent, object, &n, facts);
                flow.carry.node = n;
                flow.step = Step::FetchBytes;
                r
            }
            _ => wrong_completion(),
        },
        Step::FetchBytes => Next::Done,
        Step::FetchPrune => Next::Fail(Failure::Stale),

        // --- Write ---------------------------------------------------------
        Step::WriteNode => match completion {
            Completion::Node(n) => {
                // Remember the version this edit starts from: it is the
                // precondition a later upload asserts, and without it every
                // save would go out unconditionally and silently overwrite
                // whatever the server had.
                flow.carry.node = n.clone();
                let n = &n;
                if !n.found {
                    Next::Fail(Failure::NotFound)
                } else if n.dir {
                    Next::Fail(Failure::WrongKind)
                } else if !n.writable {
                    Next::Fail(Failure::NotWritable)
                } else if facts.buffer_open {
                    flow.step = Step::WriteBytes;
                    write_bytes(&flow.intent, object)
                } else if n.size > 0 {
                    // The base has to be here in full before it can be edited:
                    // a later three-way merge is made of it, and a partial copy
                    // would silently become the merge base.
                    flow.step = Step::WritePrepare;
                    Next::Do(Job::HydrateBuffer { object })
                } else {
                    flow.step = Step::WritePrepare;
                    Next::Do(Job::CreateBuffer { object })
                }
            }
            _ => wrong_completion(),
        },
        Step::WritePrepare => {
            flow.step = Step::WriteBytes;
            write_bytes(&flow.intent, object)
        }
        Step::WriteBytes => Next::Done,

        // --- Stat ----------------------------------------------------------
        Step::StatNode => match completion {
            Completion::Node(n) => {
                if n.found {
                    Next::Done
                } else {
                    Next::Fail(Failure::NotFound)
                }
            }
            _ => wrong_completion(),
        },

        // --- Lookup ---------------------------------------------------------
        Step::LookNode => match (completion, &flow.intent) {
            (Completion::Node(n), Intent::Lookup { name }) => {
                if !n.found {
                    Next::Fail(Failure::NotFound)
                } else if !n.dir {
                    Next::Fail(Failure::WrongKind)
                } else if n.children_loaded {
                    let name = name.clone();
                    flow.step = Step::LookChild;
                    Next::Do(Job::ReadChild {
                        parent: object,
                        name,
                    })
                } else {
                    flow.step = Step::LookRemote;
                    Next::Do(Job::ListRemote { object })
                }
            }
            _ => wrong_completion(),
        },
        Step::LookRemote => match &flow.intent {
            Intent::Lookup { name } => {
                let name = name.clone();
                flow.step = Step::LookChild;
                Next::Do(Job::ReadChild {
                    parent: object,
                    name,
                })
            }
            _ => wrong_completion(),
        },
        Step::LookChild => match completion {
            Completion::Node(n) => {
                if n.found {
                    Next::Done
                } else {
                    // A negative answer the kernel is allowed to cache.
                    Next::Fail(Failure::NotFound)
                }
            }
            _ => wrong_completion(),
        },

        // --- State ----------------------------------------------------------
        Step::StateRead => Next::Done,

        // --- SetAttr --------------------------------------------------------
        Step::AttrNode => match (completion, &flow.intent) {
            (Completion::Node(n), Intent::SetAttr { size, mtime }) => {
                // As in the write path: the version this change starts from is
                // the precondition its upload will assert. A truncate that
                // forgets it turns every later save into an unconditional
                // overwrite of whatever the server has.
                flow.carry.node = n.clone();
                let n = &n;
                if !n.found {
                    Next::Fail(Failure::NotFound)
                } else if n.dir && size.is_some() {
                    Next::Fail(Failure::WrongKind)
                } else if size.is_some() && !n.writable {
                    Next::Fail(Failure::NotWritable)
                } else if let Some(size) = *size {
                    if facts.buffer_open {
                        flow.step = Step::AttrTruncate;
                        Next::Do(Job::TruncateBuffer { object, size })
                    } else if size == 0 {
                        // The base is about to be discarded wholesale, so
                        // fetching it first would spend a full download on bytes
                        // we throw away. This is the common overwrite path: `cp`
                        // onto an existing file, and every O_TRUNC editor.
                        flow.step = Step::AttrPrepare;
                        Next::Do(Job::CreateBuffer { object })
                    } else {
                        flow.step = Step::AttrPrepare;
                        Next::Do(Job::HydrateBuffer { object })
                    }
                } else if let Some(mtime) = *mtime {
                    flow.step = Step::AttrMtime;
                    Next::Do(Job::RecordMtime { object, mtime })
                } else {
                    // Everything else is accepted as a no-op, so the caller is
                    // answered with the attributes it already has.
                    flow.step = Step::AttrFinal;
                    Next::Done
                }
            }
            _ => wrong_completion(),
        },
        Step::AttrPrepare => match &flow.intent {
            Intent::SetAttr { size, .. } => {
                let size = size.unwrap_or(0);
                flow.step = Step::AttrTruncate;
                Next::Do(Job::TruncateBuffer { object, size })
            }
            _ => wrong_completion(),
        },
        Step::AttrTruncate => match &flow.intent {
            Intent::SetAttr { mtime, .. } => match *mtime {
                Some(mtime) => {
                    flow.step = Step::AttrMtime;
                    Next::Do(Job::RecordMtime { object, mtime })
                }
                None => {
                    flow.step = Step::AttrFinal;
                    Next::Do(Job::ReadNode { object })
                }
            },
            _ => wrong_completion(),
        },
        Step::AttrMtime => {
            // Read the row back, so the caller is answered with what the
            // attributes actually became rather than what was asked for.
            flow.step = Step::AttrFinal;
            Next::Do(Job::ReadNode { object })
        }
        Step::AttrFinal => Next::Done,

        // --- Enumerate -----------------------------------------------------
        Step::EnumNode => match completion {
            Completion::Node(n) => {
                let (found, dir, loaded) = (n.found, n.dir, n.children_loaded);
                flow.carry.node = n;
                if !found {
                    Next::Fail(Failure::NotFound)
                } else if !dir {
                    Next::Fail(Failure::WrongKind)
                } else if loaded {
                    // Serve what we have. A stale listing is refreshed in the
                    // background — the one thing that must never happen is
                    // making this caller wait for the server when we already
                    // have an answer.
                    flow.step = Step::EnumChildren;
                    Next::Do(Job::ReadChildren { object })
                } else {
                    // Listing it *now* is the refresh. Without clearing this,
                    // the flow's end would ask for another one — a second
                    // PROPFIND of a directory that was just read, on every cold
                    // listing. It also made a server-side change invisible for
                    // a while: the redundant refresh reconciled the new state
                    // into the database before the syncer ever compared, so
                    // there was nothing left for it to notice.
                    flow.carry.node.listing_stale = false;
                    flow.step = Step::EnumRemote;
                    Next::Do(Job::ListRemote { object })
                }
            }
            _ => wrong_completion(),
        },
        Step::EnumRemote => {
            flow.step = Step::EnumChildren;
            Next::Do(Job::ReadChildren { object })
        }
        Step::EnumChildren => Next::Done,

        // --- Materialise ---------------------------------------------------
        Step::MatInsert => match &flow.intent {
            Intent::Materialise { name, .. } => {
                // The buffer belongs to the *new* object, which only the
                // lookup below can name — so the row is resolved first.
                let name = name.clone();
                flow.step = Step::MatLookup;
                Next::Do(Job::ReadChild {
                    parent: object,
                    name,
                })
            }
            _ => wrong_completion(),
        },
        Step::MatRemote => {
            flow.step = Step::MatList;
            Next::Do(Job::ListRemote { object })
        }
        Step::MatList => match &flow.intent {
            Intent::Materialise { name, .. } => {
                let name = name.clone();
                flow.step = Step::MatLookup;
                Next::Do(Job::ReadChild {
                    parent: object,
                    name,
                })
            }
            _ => wrong_completion(),
        },
        Step::MatLookup => match (completion, &flow.intent) {
            (Completion::Node(n), Intent::Materialise { dir, .. }) => {
                if !n.found {
                    Next::Fail(Failure::Io)
                } else if *dir {
                    Next::Done
                } else {
                    let id = n.id;
                    flow.carry.node = n;
                    flow.step = Step::MatBuffer;
                    Next::Do(Job::CreateBuffer { object: id })
                }
            }
            _ => wrong_completion(),
        },
        Step::MatBuffer => Next::Done,

        // --- Publish -------------------------------------------------------
        Step::PubNode => match completion {
            Completion::Node(n) => {
                if n.found && n.ignored {
                    // An editor's swap file or an office lock: the buffer *is*
                    // the file's content until the file is removed, so it is
                    // kept rather than sent. Decided here and not at the start
                    // because it follows the *name*, and a rename onto a real
                    // document is precisely how such a file gets published.
                    Next::Done
                } else if n.found {
                    flow.carry.node = n;
                    // Commit locally first — record the upload as owed and make
                    // the buffer durable — so `flush` can be answered before the
                    // bytes reach the server.
                    flow.step = Step::PubCommit;
                    Next::Do(Job::MarkPending {
                        object,
                        base_etag: facts.base_etag.clone(),
                        mtime: facts.pending_mtime,
                    })
                } else {
                    // Unlinked while the buffer was open — the editor deleted
                    // its own swap file. Drop it; do not resurrect the file.
                    flow.step = Step::PubDiscard;
                    Next::Do(Job::DiscardBuffer { object })
                }
            }
            _ => wrong_completion(),
        },
        Step::PubCommit => {
            // The change is safe: durably in the buffer and recorded as owed to
            // the server. Answer `flush`/`release` now — the app is told "saved"
            // truthfully — and let the upload run on with nobody waiting.
            flow.step = Step::PubSize;
            Next::AnswerThen(Job::BufferSize { object })
        }
        Step::PubSize => match completion {
            Completion::Size(size) => {
                flow.carry.size = size;
                flow.step = Step::PubUpload;
                Next::Do(Job::Upload {
                    object,
                    size,
                    precondition: precondition(&flow.carry.node, facts),
                    mtime: facts.pending_mtime,
                })
            }
            _ => wrong_completion(),
        },
        Step::PubConflict => {
            // The sub-script recorded the version, stored the bytes and
            // reconciled the parent itself — whichever way it resolved. All
            // that is left is to clear the pending record and let the buffer go.
            flow.step = Step::PubClear;
            Next::Do(Job::ClearPending { object })
        }
        Step::PubUpload => match completion {
            Completion::Uploaded { etag, size } => {
                flow.carry.etag = etag.clone();
                flow.carry.size = size;
                flow.step = Step::PubRecord;
                Next::Do(Job::RecordVersion { object, etag, size })
            }
            Completion::Rejected => {
                // 412: somebody changed it since we based our edit on it. The
                // sub-script merges, or parks our bytes under a second name —
                // it never drops them.
                flow.step = Step::PubConflict;
                Next::Do(Job::ResolveConflict { object })
            }
            _ => wrong_completion(),
        },
        Step::PubRecord => {
            flow.step = Step::PubStore;
            Next::Do(Job::StoreBlob { object })
        }
        Step::PubStore => {
            if flow.carry.node.materialised {
                flow.step = Step::PubClear;
                Next::Do(Job::ClearPending { object })
            } else {
                // It had no server identity before this upload. Re-listing the
                // parent is what gives the row its server-assigned file id —
                // without which a later rename or delete would think the object
                // exists only locally.
                flow.step = Step::PubReload;
                Next::Do(Job::ListRemote {
                    object: flow.carry.node.parent,
                })
            }
        }
        Step::PubReload => {
            flow.step = Step::PubClear;
            Next::Do(Job::ClearPending { object })
        }
        Step::PubClear => {
            flow.step = Step::PubDiscard;
            Next::Do(Job::DiscardBuffer { object })
        }
        // The failure has been recorded on the pending upload. End the flow —
        // nobody is waiting (flush was answered at the commit) — keeping the
        // buffer, so a retry has the bytes to send.
        Step::PubError => Next::Fail(Failure::Io),
        Step::PubDiscard => Next::Done,

        // --- Remove --------------------------------------------------------
        Step::RemNode => match completion {
            Completion::Node(n) => {
                let child = n.id;
                let materialised = n.materialised;
                let found = n.found;
                flow.carry.node = n;
                if !found {
                    Next::Fail(Failure::NotFound)
                } else if materialised {
                    flow.step = Step::RemRemote;
                    Next::Do(Job::DeleteRemote { object: child })
                } else {
                    // Never published, so there is nothing on the server to
                    // delete — a created-and-deleted temp file costs no
                    // round-trip at all.
                    flow.step = Step::RemBuffer;
                    Next::Do(Job::DiscardBuffer { object: child })
                }
            }
            _ => wrong_completion(),
        },
        Step::RemRemote => {
            flow.step = Step::RemBuffer;
            Next::Do(Job::DiscardBuffer {
                object: flow.carry.node.id,
            })
        }
        Step::RemBuffer => {
            flow.step = Step::RemRows;
            Next::Do(Job::RemoveRows {
                object: flow.carry.node.id,
            })
        }
        Step::RemRows => Next::Done,

        // --- Move ----------------------------------------------------------
        Step::MovNode => match (completion, &flow.intent) {
            (
                Completion::Node(n),
                Intent::Move {
                    to_parent, to_name, ..
                },
            ) => {
                let child = n.id;
                let (found, materialised) = (n.found, n.materialised);
                flow.carry.node = n;
                let (to_parent, to_name) = (*to_parent, to_name.clone());
                if !found {
                    Next::Fail(Failure::NotFound)
                } else if materialised {
                    flow.step = Step::MovRemote;
                    Next::Do(Job::MoveRemote {
                        object: child,
                        to_parent,
                        to_name,
                    })
                } else {
                    // Never published, so the rename is ours alone and the later
                    // publish sends it under the new name. This is the
                    // office-suite atomic save.
                    flow.step = Step::MovRows;
                    Next::Do(Job::MoveRows {
                        object: child,
                        to_parent,
                        to_name,
                    })
                }
            }
            _ => wrong_completion(),
        },
        Step::MovRemote => match &flow.intent {
            Intent::Move {
                to_parent, to_name, ..
            } => {
                let (to_parent, to_name) = (*to_parent, to_name.clone());
                flow.step = Step::MovRows;
                Next::Do(Job::MoveRows {
                    // The child the first step resolved — never the parent this
                    // flow is keyed on.
                    object: flow.carry.node.id,
                    to_parent,
                    to_name,
                })
            }
            _ => wrong_completion(),
        },
        Step::MovRows => Next::Done,

        // --- Refresh -------------------------------------------------------
        Step::RefNode => match completion {
            Completion::Node(n) => {
                if !n.found || n.blob_current {
                    // Gone, or already current: a refresh that has nothing to
                    // do is a success, not an error.
                    Next::Done
                } else {
                    flow.step = Step::RefHydrate;
                    Next::Do(Job::HydrateCache { object })
                }
            }
            _ => wrong_completion(),
        },
        Step::RefHydrate => Next::Done,
        Step::RelistRemote => Next::Done,
    };
    (flow, next)
}

/// Where a read's bytes come from. A pure three-way decision, which it can only
/// be because the freshness of the cached copy was settled by the worker that
/// read the row — stating a file is I/O, and I/O does not happen here.
fn fetch_source(intent: &Intent, object: ObjectId, n: &NodeFacts, facts: &Facts) -> Next {
    let Intent::Fetch { offset, len } = intent else {
        return wrong_completion();
    };
    let (offset, len) = (*offset, *len);
    if !n.found {
        Next::Fail(Failure::NotFound)
    } else if n.dir {
        Next::Fail(Failure::WrongKind)
    } else if facts.buffer_open {
        // An open buffer wins: edits in progress are visible at once, and a
        // creation that has never been published is readable before it exists
        // anywhere else.
        Next::Do(Job::ReadBuffer {
            object,
            offset,
            len,
        })
    } else if n.blob_current || n.stale_copy_ok {
        // Either the copy is current, or it is outdated and the engine has
        // decided that is what the user wants — a pin on a metered connection,
        // or "serve the offline version and let me refresh when I mean to".
        // Whoever hands out the outdated bytes says so; that is not a decision.
        Next::Do(Job::ReadBlob {
            object,
            offset,
            len,
        })
    } else {
        Next::Do(Job::FetchRange {
            object,
            offset,
            len,
        })
    }
}

fn write_bytes(intent: &Intent, object: ObjectId) -> Next {
    match intent {
        Intent::Write { offset, len } => Next::Do(Job::WriteBuffer {
            object,
            offset: *offset,
            len: *len,
        }),
        Intent::Fetch { .. }
        | Intent::Stat
        | Intent::Enumerate
        | Intent::Materialise { .. }
        | Intent::Publish
        | Intent::Remove { .. }
        | Intent::Move { .. }
        | Intent::Refresh
        | Intent::Lookup { .. }
        | Intent::State
        | Intent::Relist
        | Intent::SetAttr { .. } => wrong_completion(),
    }
}

/// What we may assert about the server for this upload.
fn precondition(node: &NodeFacts, facts: &Facts) -> Precondition {
    if !node.materialised {
        // It has never been there, so claiming so is exactly right — and it is
        // what turns a racing creation of the same name into a conflict we
        // resolve rather than an overwrite nobody notices.
        Precondition::MustNotExist
    } else if facts.base_etag.is_empty() {
        // It exists but we do not know which version we started from. Asserting
        // absence here would report an ordinary save as a conflict.
        Precondition::Unconditional
    } else {
        Precondition::Match(facts.base_etag.clone())
    }
}

/// A completion that does not belong to the step it arrived for.
///
/// Not reachable through the machine — the executor answers the job it was
/// given — so this is a programming error rather than a runtime condition, and
/// it fails the flow instead of panicking: a wedged operation is recoverable, a
/// panicking decision thread takes the whole mount with it.
fn wrong_completion() -> Next {
    Next::Fail(Failure::Io)
}
