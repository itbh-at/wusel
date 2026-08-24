// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Whole operations walked end to end by handing the machine a scripted
//! sequence of completions.
//!
//! No mount, no server, no database, no threads — which is the point. Today a
//! flush is seven nested blocking calls that can only be exercised against a
//! real Nextcloud; as a script it is seven decisions, and a conflict on the
//! seventh is three lines of test.

use wusel_fsm::registry::Facts;
use wusel_fsm::script::{advance, start, Next};
use wusel_fsm::{Completion, Failure, Intent, Job, NodeFacts, ObjectId, Precondition, RequestId};

const OBJ: ObjectId = ObjectId(42);
const REQ: RequestId = RequestId(1);

fn file(size: u64, blob_current: bool) -> Completion {
    Completion::Node(NodeFacts {
        id: OBJ,
        parent: ObjectId(1),
        ignored: false,
        found: true,
        dir: false,
        writable: true,
        etag: "v1".into(),
        size,
        blob_current,
        materialised: true,
        children_loaded: false,
        listing_stale: false,
        stale_copy_ok: false,
    })
}

fn buffered(dirty: bool) -> Facts {
    Facts {
        buffer_open: true,
        buffer_dirty: dirty,
        ignored: false,
        base_etag: "v1".into(),
        pending_mtime: None,
    }
}

// --- read ------------------------------------------------------------------

#[test]
fn a_read_with_nothing_cached_goes_to_the_network() {
    let facts = Facts::default();
    let (flow, next) = start(
        OBJ,
        Intent::Fetch {
            offset: 0,
            len: 4096,
        },
        REQ,
        &facts,
    );
    assert_eq!(next, Next::Do(Job::ReadNode { object: OBJ }));

    let (flow, next) = advance(flow, file(1_000_000, false), &facts);
    assert_eq!(
        next,
        Next::Do(Job::FetchRange {
            object: OBJ,
            offset: 0,
            len: 4096
        }),
        "online-only: serve just this range, live"
    );

    let (_, next) = advance(flow, Completion::Bytes, &facts);
    assert_eq!(next, Next::Done);
}

#[test]
fn a_read_prefers_the_write_buffer_over_everything() {
    // Coherence: an edit in progress must be visible at once, and a creation
    // that has never been published is readable before it exists anywhere else.
    let facts = buffered(true);
    let (flow, _) = start(OBJ, Intent::Fetch { offset: 8, len: 16 }, REQ, &facts);
    let (_, next) = advance(flow, file(1_000_000, true), &facts);
    assert_eq!(
        next,
        Next::Do(Job::ReadBuffer {
            object: OBJ,
            offset: 8,
            len: 16
        })
    );
}

#[test]
fn a_read_uses_the_cache_only_while_it_is_current() {
    let facts = Facts::default();
    let (flow, _) = start(OBJ, Intent::Fetch { offset: 0, len: 32 }, REQ, &facts);
    let (_, next) = advance(flow, file(999, true), &facts);
    assert_eq!(
        next,
        Next::Do(Job::ReadBlob {
            object: OBJ,
            offset: 0,
            len: 32
        })
    );
}

#[test]
fn reading_a_directory_fails_rather_than_returning_bytes() {
    let facts = Facts::default();
    let (flow, _) = start(OBJ, Intent::Fetch { offset: 0, len: 32 }, REQ, &facts);
    let dir = Completion::Node(NodeFacts {
        id: OBJ,
        parent: ObjectId(1),
        ignored: false,
        found: true,
        dir: true,
        ..NodeFacts::default()
    });
    let (_, next) = advance(flow, dir, &facts);
    assert_eq!(next, Next::Fail(Failure::WrongKind));
}

// --- flush -----------------------------------------------------------------

#[test]
fn a_publish_commits_answers_then_uploads_records_stores_clears_discards() {
    let facts = buffered(true);
    let (flow, next) = start(OBJ, Intent::Publish, REQ, &facts);
    assert_eq!(next, Next::Do(Job::ReadNode { object: OBJ }));

    // The change is committed locally first: the upload target and precondition
    // are recorded and the buffer is made durable.
    let (flow, next) = advance(flow, file(4096, false), &facts);
    assert_eq!(
        next,
        Next::Do(Job::MarkPending {
            object: OBJ,
            base_etag: "v1".into(),
            mtime: None,
        })
    );

    // With the change durable, `flush` is answered here — and the upload runs
    // on with nobody waiting for it.
    let (flow, next) = advance(flow, Completion::Done, &facts);
    assert_eq!(next, Next::AnswerThen(Job::BufferSize { object: OBJ }));

    let (flow, next) = advance(flow, Completion::Size(8192), &facts);
    assert_eq!(
        next,
        Next::Do(Job::Upload {
            object: OBJ,
            size: 8192,
            precondition: Precondition::Match("v1".into()),
            mtime: None,
        }),
        "a known base version is asserted, so a concurrent change is caught"
    );

    let (flow, next) = advance(
        flow,
        Completion::Uploaded {
            etag: "v2".into(),
            size: 8192,
        },
        &facts,
    );
    assert_eq!(
        next,
        Next::Do(Job::RecordVersion {
            object: OBJ,
            etag: "v2".into(),
            size: 8192
        })
    );

    let (flow, next) = advance(flow, Completion::Done, &facts);
    assert_eq!(next, Next::Do(Job::StoreBlob { object: OBJ }));

    let (flow, next) = advance(flow, Completion::Done, &facts);
    assert_eq!(
        next,
        Next::Do(Job::ClearPending { object: OBJ }),
        "the upload landed, so the pending record is cleared before the buffer"
    );

    let (flow, next) = advance(flow, Completion::Done, &facts);
    assert_eq!(next, Next::Do(Job::DiscardBuffer { object: OBJ }));

    let (_, next) = advance(flow, Completion::Done, &facts);
    assert_eq!(next, Next::Done);
}

#[test]
fn a_rejected_upload_goes_through_conflict_resolution_and_still_records() {
    let facts = buffered(true);
    let (flow, _) = start(OBJ, Intent::Publish, REQ, &facts);
    let (flow, _) = advance(flow, file(4096, false), &facts); // -> MarkPending
    let (flow, _) = advance(flow, Completion::Done, &facts); // -> AnswerThen(BufferSize)
    let (flow, _) = advance(flow, Completion::Size(4096), &facts); // -> Upload

    let (flow, next) = advance(flow, Completion::Rejected, &facts);
    assert_eq!(next, Next::Do(Job::ResolveConflict { object: OBJ }));

    // The sub-script records the version, stores the bytes and reconciles the
    // parent itself — whichever way it resolved — so all that is left here is
    // to clear the pending record and let the buffer go.
    let (_, next) = advance(flow, Completion::Done, &facts);
    assert_eq!(next, Next::Do(Job::ClearPending { object: OBJ }));
}

#[test]
fn a_transient_upload_failure_leaves_the_record_pending_for_retry() {
    // The upload runs after the commit, so flush is already answered. A
    // *transient* failure (a 5xx, a timeout) just ends the flow: the pending
    // record is untouched — still `pending` — and the buffer is kept, so the
    // uploader retries it. Nothing discards a dirty buffer except a landed
    // upload.
    let facts = buffered(true);
    let (flow, _) = start(OBJ, Intent::Publish, REQ, &facts);
    let (flow, _) = advance(flow, file(4096, false), &facts); // -> MarkPending
    let (flow, _) = advance(flow, Completion::Done, &facts); // -> AnswerThen(BufferSize)
    let (flow, _) = advance(flow, Completion::Size(4096), &facts); // -> Upload

    let (_, next) = advance(flow, Completion::Failed(Failure::Io), &facts);
    assert_eq!(
        next,
        Next::Fail(Failure::Io),
        "no job — the record stays queued"
    );
}

#[test]
fn a_permanent_upload_failure_parks_the_record_and_keeps_the_buffer() {
    // A *permanent* failure (wrong permissions, a conflict, no quota) will not
    // fix itself, so it is recorded on the pending upload as an error — visible,
    // and not retried — with the buffer kept so it can still be resolved.
    let facts = buffered(true);
    let (flow, _) = start(OBJ, Intent::Publish, REQ, &facts);
    let (flow, _) = advance(flow, file(4096, false), &facts); // -> MarkPending
    let (flow, _) = advance(flow, Completion::Done, &facts); // -> AnswerThen(BufferSize)
    let (flow, _) = advance(flow, Completion::Size(4096), &facts); // -> Upload

    let (flow, next) = advance(flow, Completion::Failed(Failure::Permanent), &facts);
    assert_eq!(
        next,
        Next::Do(Job::SetUploadError {
            object: OBJ,
            message: "Permanent".into(),
        })
    );

    // Then the flow ends — nobody is waiting, and the buffer is left intact.
    let (_, next) = advance(flow, Completion::Done, &facts);
    assert_eq!(next, Next::Fail(Failure::Io));
}

#[test]
fn publishing_with_no_buffer_open_does_nothing_at_all() {
    // Every `release` of a read-only handle lands here; it must not cost a
    // database round-trip.
    let (_, next) = start(OBJ, Intent::Publish, REQ, &Facts::default());
    assert_eq!(next, Next::Done);
}

#[test]
fn publishing_an_ignored_file_keeps_its_buffer_and_sends_nothing() {
    // A vim swap file or an office lock file: the buffer *is* the content until
    // the file is removed.
    //
    // Decided from the row rather than at the start, because being ephemeral
    // follows the *name* — and renaming such a file onto a real document is
    // exactly how it stops being one.
    let facts = buffered(true);
    let (flow, next) = start(OBJ, Intent::Publish, REQ, &facts);
    assert_eq!(next, Next::Do(Job::ReadNode { object: OBJ }));

    let lock = Completion::Node(NodeFacts {
        id: OBJ,
        found: true,
        ignored: true,
        ..NodeFacts::default()
    });
    let (_, next) = advance(flow, lock, &facts);
    assert_eq!(next, Next::Done, "nothing sent, and the buffer is kept");
}

#[test]
fn publishing_a_clean_buffer_discards_it_without_uploading() {
    let (_, next) = start(OBJ, Intent::Publish, REQ, &buffered(false));
    assert_eq!(next, Next::Do(Job::DiscardBuffer { object: OBJ }));
}

#[test]
fn a_file_unlinked_while_buffered_is_not_resurrected() {
    let facts = buffered(true);
    let (flow, _) = start(OBJ, Intent::Publish, REQ, &facts);
    let gone = Completion::Node(NodeFacts::default());
    let (_, next) = advance(flow, gone, &facts);
    assert_eq!(next, Next::Do(Job::DiscardBuffer { object: OBJ }));
}

// --- preconditions ---------------------------------------------------------

#[test]
fn an_object_never_sent_asserts_that_it_is_absent() {
    let facts = Facts {
        buffer_open: true,
        buffer_dirty: true,
        ignored: false,
        base_etag: String::new(),
        pending_mtime: None,
    };
    let (flow, _) = start(OBJ, Intent::Publish, REQ, &facts);
    let fresh = Completion::Node(NodeFacts {
        id: OBJ,
        parent: ObjectId(1),
        ignored: false,
        found: true,
        materialised: false,
        ..NodeFacts::default()
    });
    let (flow, _) = advance(flow, fresh, &facts); // -> MarkPending
    let (flow, _) = advance(flow, Completion::Done, &facts); // -> AnswerThen(BufferSize)
    let (_, next) = advance(flow, Completion::Size(3), &facts);
    assert_eq!(
        next,
        Next::Do(Job::Upload {
            object: OBJ,
            size: 3,
            precondition: Precondition::MustNotExist,
            mtime: None,
        })
    );
}

#[test]
fn an_object_of_unknown_version_uploads_unconditionally() {
    // It exists, but we do not know what we started from. Claiming absence here
    // would report an ordinary save as a conflict.
    let facts = Facts {
        buffer_open: true,
        buffer_dirty: true,
        ignored: false,
        base_etag: String::new(),
        pending_mtime: None,
    };
    let (flow, _) = start(OBJ, Intent::Publish, REQ, &facts);
    let (flow, _) = advance(flow, file(9, false), &facts); // -> MarkPending
    let (flow, _) = advance(flow, Completion::Done, &facts); // -> AnswerThen(BufferSize)
    let (_, next) = advance(flow, Completion::Size(9), &facts);
    assert_eq!(
        next,
        Next::Do(Job::Upload {
            object: OBJ,
            size: 9,
            precondition: Precondition::Unconditional,
            mtime: None,
        })
    );
}

// --- write -----------------------------------------------------------------

#[test]
fn a_write_to_an_unbuffered_file_fetches_the_whole_base_first() {
    // Never a partial copy: the base is what a later three-way merge is made of.
    let facts = Facts::default();
    let (flow, _) = start(OBJ, Intent::Write { offset: 0, len: 4 }, REQ, &facts);
    let (flow, next) = advance(flow, file(1_000_000, false), &facts);
    assert_eq!(next, Next::Do(Job::HydrateBuffer { object: OBJ }));

    let (_, next) = advance(flow, Completion::Done, &facts);
    assert_eq!(
        next,
        Next::Do(Job::WriteBuffer {
            object: OBJ,
            offset: 0,
            len: 4
        })
    );
}

#[test]
fn a_write_to_an_empty_file_needs_no_download() {
    let facts = Facts::default();
    let (flow, _) = start(OBJ, Intent::Write { offset: 0, len: 4 }, REQ, &facts);
    let (_, next) = advance(flow, file(0, false), &facts);
    assert_eq!(next, Next::Do(Job::CreateBuffer { object: OBJ }));
}

#[test]
fn a_write_to_a_read_only_object_is_refused() {
    let facts = Facts::default();
    let (flow, _) = start(OBJ, Intent::Write { offset: 0, len: 4 }, REQ, &facts);
    let ro = Completion::Node(NodeFacts {
        id: OBJ,
        parent: ObjectId(1),
        ignored: false,
        found: true,
        writable: false,
        ..NodeFacts::default()
    });
    let (_, next) = advance(flow, ro, &facts);
    assert_eq!(next, Next::Fail(Failure::NotWritable));
}

// --- abort -----------------------------------------------------------------

#[test]
fn an_aborted_flow_gives_up_at_the_next_step_boundary() {
    // Abort is a request, not an act: the step that was running finished, and
    // the flow stops before the next one starts.
    let facts = Facts::default();
    let (mut flow, _) = start(OBJ, Intent::Fetch { offset: 0, len: 8 }, REQ, &facts);
    flow.abort = true;
    let (_, next) = advance(flow, file(10, false), &facts);
    assert_eq!(
        next,
        Next::Abandoned,
        "nobody is waiting, so nobody is told"
    );
}
