// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The switchboard: what the machine does with requests that meet a busy
//! object, and that every waiter is eventually answered exactly once.
//!
//! These drive the real entry points ([`Machine::on_request`] and
//! [`Machine::on_completion`]) rather than the pure transition function, so
//! they cover the part the scripts cannot: the queue, the join list, and the
//! release of an object when a flow ends.

use wusel_fsm::registry::Buffer;
use wusel_fsm::{
    Action, Completion, Failure, Intent, Job, Machine, NodeFacts, ObjectId, Outcome, Precondition,
    Request, RequestId,
};

const A: ObjectId = ObjectId(1);
const B: ObjectId = ObjectId(2);

fn req(id: u64, object: ObjectId, intent: Intent) -> Request {
    Request {
        id: RequestId(id),
        object,
        intent,
    }
}

fn read(offset: u64, len: u32) -> Intent {
    Intent::Fetch { offset, len }
}

fn a_file() -> Completion {
    Completion::Node(NodeFacts {
        found: true,
        writable: true,
        etag: "v1".into(),
        size: 4096,
        materialised: true,
        identified: true,
        ..NodeFacts::default()
    })
}

/// Every dispatched job, in order.
fn jobs(actions: &[Action]) -> Vec<Job> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Dispatch { job, .. } | Action::ReadBeside { job, .. } => Some(job.clone()),
            Action::Answer { .. } | Action::Schedule { .. } | Action::Refresh { .. } => None,
        })
        .collect()
}

/// Every answer, as (requests, outcome).
fn answers(actions: &[Action]) -> Vec<(Vec<RequestId>, Outcome)> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Answer {
                requests, outcome, ..
            } => Some((requests.clone(), *outcome)),
            Action::Dispatch { .. }
            | Action::ReadBeside { .. }
            | Action::Schedule { .. }
            | Action::Refresh { .. } => None,
        })
        .collect()
}

#[test]
fn an_idle_object_starts_at_once() {
    let mut m = Machine::new();
    let actions = m.on_request(req(1, A, read(0, 16)));
    assert_eq!(jobs(&actions), vec![Job::ReadNode { object: A }]);
    assert!(m.is_busy(A));
}

#[test]
fn the_snapshot_shows_a_flow_with_its_waiters_but_no_file_name() {
    // The diagnostics view: an idle machine shows nothing, and a busy one shows
    // where the work is and how many callers ride or queue behind it — the exact
    // shape that told a wedged fetch from a healthy one — while naming no file.
    let mut m = Machine::new();
    assert!(
        m.snapshot().objects.is_empty(),
        "an idle machine has nothing in flight"
    );

    let _ = m.on_request(req(1, A, read(0, 16))); // begins a fetch
    let _ = m.on_request(req(2, A, read(0, 16))); // same range: joins
    let _ = m.on_request(req(3, A, read(64, 16))); // other range: queues

    let snap = m.snapshot();
    assert_eq!(snap.objects.len(), 1, "one object is busy");
    let o = &snap.objects[0];
    assert_eq!(o.object, A.0, "reported by inode, a number and not a name");
    assert_eq!(o.intent, "fetch");
    assert_eq!(o.step, "FetchNode", "the step pinpoints where it is");
    assert!(o.outstanding, "a job is handed out");
    assert_eq!(o.waiters, 2, "two readers ride the one flow");
    assert_eq!(o.queued, 1, "and one waits behind it");
    assert!(!o.abort);
}

#[test]
fn synchronous_mode_holds_flush_until_the_upload_lands() {
    // The fallback: with async write-back off, `flush` is not answered at the
    // commit — it rides the flow to the end and gets the upload's real result.
    let mut m = Machine::new();
    m.set_async_upload(false);
    m.registry_mut().open(A, Buffer::new("v1".into()));
    m.registry_mut().mark_dirty(A);

    let start = m.on_request(req(1, A, Intent::Publish));
    assert_eq!(jobs(&start), vec![Job::ReadNode { object: A }]);

    // ReadNode -> MarkPending (the commit).
    let commit = m.on_completion(A, a_file());
    assert_eq!(
        jobs(&commit),
        vec![Job::MarkPending {
            object: A,
            base_etag: "v1".into(),
            mtime: None
        }]
    );

    // The commit is done. In async this answers flush; in sync it must not — the
    // upload just goes ahead, with the caller still waiting.
    let after_commit = m.on_completion(A, Completion::Done);
    assert!(
        answers(&after_commit).is_empty(),
        "synchronous flush is not answered at the commit"
    );
    assert_eq!(jobs(&after_commit), vec![Job::BufferSize { object: A }]);

    // Walk the upload to the end; only there is the caller answered.
    let _ = m.on_completion(A, Completion::Size(10)); // -> Upload
    let _ = m.on_completion(
        A,
        Completion::Uploaded {
            etag: "v2".into(),
            size: 10,
        },
    ); // -> RecordVersion
    let _ = m.on_completion(A, Completion::Done); // -> StoreBlob
    let _ = m.on_completion(A, Completion::Done); // -> ClearPending
    let _ = m.on_completion(A, Completion::Done); // -> DiscardBuffer
    let done = m.on_completion(A, Completion::Done); // -> Done
    assert_eq!(
        answers(&done),
        vec![(vec![RequestId(1)], Outcome::Ok)],
        "flush is answered only when the upload has landed"
    );
    assert!(!m.is_busy(A));
}

#[test]
fn the_snapshot_counts_dirty_buffers() {
    let mut m = Machine::new();
    m.registry_mut().open(A, Buffer::new("v1".into()));
    m.registry_mut().open(B, Buffer::new("v1".into()));
    m.registry_mut().mark_dirty(A);

    let snap = m.snapshot();
    assert_eq!(snap.buffers_open, 2);
    assert_eq!(snap.buffers_dirty, 1, "only A was written to");
}

#[test]
fn unrelated_objects_do_not_wait_for_each_other() {
    // The whole point of per-object occupancy: a transfer on A must not make a
    // stat of B queue behind it.
    let mut m = Machine::new();
    let _ = m.on_request(req(1, A, read(0, 16)));
    let actions = m.on_request(req(2, B, Intent::Stat));
    assert_eq!(jobs(&actions), vec![Job::ReadNode { object: B }]);
}

#[test]
fn a_joined_reader_is_answered_by_the_one_transfer() {
    let mut m = Machine::new();
    let first = m.on_request(req(1, A, read(0, 16)));
    assert_eq!(jobs(&first).len(), 1);

    // A second reader of the same range rides along: no second job.
    let second = m.on_request(req(2, A, read(0, 16)));
    assert!(second.is_empty(), "a join dispatches nothing new");

    let after_row = m.on_completion(A, a_file());
    assert_eq!(
        jobs(&after_row),
        vec![Job::FetchRange {
            object: A,
            offset: 0,
            len: 16
        }],
        "one transfer, not two"
    );

    let done = m.on_completion(A, Completion::Bytes);
    assert_eq!(
        answers(&done),
        vec![(vec![RequestId(1), RequestId(2)], Outcome::Ok)],
        "both readers answered together"
    );
    assert!(!m.is_busy(A), "the object is idle again");
}

#[test]
fn a_reader_arriving_at_a_flow_that_was_given_up_is_still_served() {
    // The mount hung on opening a file with no job left running behind the
    // parked reply. A reader joined a flow whose last waiter had just left, so
    // the flow was marked to give up; at its next step it ended as Abandoned,
    // which answers nobody — and the joiner waited for ever on a locked page.
    let mut m = Machine::new();
    let _ = m.on_request(req(1, A, read(0, 16)));
    // The only waiter goes away: the flow is now marked to give up.
    let gone = m.abandon(RequestId(1));
    assert_eq!(
        answers(&gone),
        vec![(vec![RequestId(1)], Outcome::Failed(Failure::Interrupted))],
        "the departing reader is still answered"
    );

    // A fresh reader of the same range arrives before the flow notices.
    let joiner = m.on_request(req(2, A, read(0, 16)));
    assert!(joiner.is_empty(), "nothing dispatched while the flow ends");

    // The step in flight finishes and the flow gives up.
    let after = m.on_completion(A, a_file());
    assert!(
        answers(&after)
            .iter()
            .all(|(r, _)| !r.contains(&RequestId(2))),
        "the abandoned flow does not answer the newcomer with its own outcome"
    );
    // …and the newcomer is served by a flow of its own rather than dropped.
    assert!(m.is_busy(A), "the queued reader started");
    let after_row = m.on_completion(A, a_file());
    assert_eq!(
        jobs(&after_row),
        vec![Job::FetchRange {
            object: A,
            offset: 0,
            len: 16
        }],
        "its own transfer"
    );
    let done = m.on_completion(A, Completion::Bytes);
    assert_eq!(
        answers(&done),
        vec![(vec![RequestId(2)], Outcome::Ok)],
        "and it gets its bytes"
    );
}

#[test]
fn a_queued_request_starts_when_the_object_goes_idle() {
    let mut m = Machine::new();
    let _ = m.on_request(req(1, A, read(0, 16)));
    // A different range cannot join, so it waits.
    assert!(m.on_request(req(2, A, read(64, 16))).is_empty());

    let _ = m.on_completion(A, a_file());
    let done = m.on_completion(A, Completion::Bytes);

    assert_eq!(answers(&done), vec![(vec![RequestId(1)], Outcome::Ok)]);
    assert_eq!(
        jobs(&done),
        vec![Job::ReadNode { object: A }],
        "the queued read starts as part of releasing the object"
    );
    assert!(m.is_busy(A));
}

#[test]
fn write_then_flush_keeps_its_order() {
    // The FIFO is where write → flush ordering comes from, without a lock: the
    // publish must not overtake the write whose bytes it is meant to send.
    let mut m = Machine::new();
    m.registry_mut().open(A, Buffer::new("v1".into()));

    let start = m.on_request(req(1, A, Intent::Write { offset: 0, len: 4 }));
    assert_eq!(jobs(&start), vec![Job::ReadNode { object: A }]);

    assert!(m.on_request(req(2, A, Intent::Publish)).is_empty());

    // The write finishes: row, then the buffered bytes.
    let after_row = m.on_completion(A, a_file());
    assert_eq!(
        jobs(&after_row),
        vec![Job::WriteBuffer {
            object: A,
            offset: 0,
            len: 4
        }]
    );

    let released = m.on_completion(A, Completion::Done);
    assert_eq!(answers(&released), vec![(vec![RequestId(1)], Outcome::Ok)]);
    assert_eq!(
        jobs(&released),
        vec![Job::ReadNode { object: A }],
        "and only now does the publish begin"
    );
}

#[test]
fn writing_marks_the_buffer_dirty_so_the_publish_uploads() {
    let mut m = Machine::new();
    m.registry_mut().open(A, Buffer::new("v1".into()));
    assert!(!m.registry().facts(A).buffer_dirty);

    let _ = m.on_request(req(1, A, Intent::Write { offset: 0, len: 4 }));
    let _ = m.on_completion(A, a_file());
    let _ = m.on_completion(A, Completion::Done);

    assert!(
        m.registry().facts(A).buffer_dirty,
        "a completed write is what makes the next publish send anything"
    );
}

#[test]
fn a_refresh_meeting_an_unsaved_edit_is_answered_without_work() {
    let mut m = Machine::new();
    m.registry_mut().open(A, Buffer::new("v1".into()));
    m.registry_mut().mark_dirty(A);
    let _ = m.on_request(req(1, A, Intent::Publish));

    let actions = m.on_request(req(2, A, Intent::Refresh));
    assert!(jobs(&actions).is_empty(), "skipped, not queued");
    assert_eq!(answers(&actions), vec![(vec![RequestId(2)], Outcome::Ok)]);
}

#[test]
fn a_failing_step_answers_the_waiters_and_releases_the_object() {
    let mut m = Machine::new();
    let _ = m.on_request(req(1, A, read(0, 16)));

    let gone = Completion::Node(NodeFacts::default()); // not found
    let actions = m.on_completion(A, gone);

    assert_eq!(
        answers(&actions),
        vec![(
            vec![RequestId(1)],
            Outcome::Failed(wusel_fsm::Failure::NotFound)
        )]
    );
    assert!(
        !m.is_busy(A),
        "a failure releases the object like a success"
    );
}

#[test]
fn a_publish_with_nothing_buffered_is_answered_without_touching_the_database() {
    // Every `release` of a read-only handle takes this path.
    let mut m = Machine::new();
    let actions = m.on_request(req(1, A, Intent::Publish));
    assert!(jobs(&actions).is_empty());
    assert_eq!(answers(&actions), vec![(vec![RequestId(1)], Outcome::Ok)]);
    assert!(!m.is_busy(A));
}

#[test]
fn an_abort_lets_the_running_flow_finish_its_step_then_drops_it() {
    let mut m = Machine::new();
    let _ = m.on_request(req(1, A, Intent::Refresh));

    // A remove makes the running refresh pointless.
    let aborting = m.on_request(req(
        2,
        A,
        Intent::Remove {
            name: "doc.odt".into(),
        },
    ));
    assert!(jobs(&aborting).is_empty(), "the remove waits its turn");

    // The refresh's outstanding step still completes; the flow then gives up
    // without answering anybody, and the remove starts.
    let actions = m.on_completion(A, a_file());
    assert_eq!(
        answers(&actions),
        vec![(vec![RequestId(1)], Outcome::Failed(Failure::Interrupted))],
        "giving up is not a reason to stay silent: whoever is still waiting is \
         answered, because a dropped reply leaves the kernel holding a locked page"
    );
    // The remove starts by resolving the child it was given by name — the
    // kernel never hands us an identity for it.
    assert_eq!(
        jobs(&actions),
        vec![Job::ReadChild {
            parent: A,
            name: "doc.odt".into()
        }]
    );
}

#[test]
fn abandoning_the_last_waiter_answers_it_and_gives_the_transfer_up() {
    // Two things happen, and both matter. The dead reader is still answered —
    // with an error it will never read, because the kernel holds a locked page
    // for the outstanding read until it gets *an* answer, and dropping it wedges
    // that page and every later reader of it for good. And, nobody being left
    // who wants the bytes, the rest of the download is given up as waste.
    let mut m = Machine::new();
    let _ = m.on_request(req(1, A, read(0, 16)));

    let abandoned = m.abandon(RequestId(1));
    assert_eq!(
        answers(&abandoned),
        vec![(vec![RequestId(1)], Outcome::Failed(Failure::Interrupted))],
        "the abandoned read is answered, not dropped"
    );

    let actions = m.on_completion(A, a_file());
    assert!(
        answers(&actions).is_empty(),
        "and it is answered only once — the abandoned flow owes nothing more"
    );
    assert!(jobs(&actions).is_empty(), "and starts no further step");
    assert!(!m.is_busy(A), "the object is released, not left occupied");
}

#[test]
fn abandoning_one_of_two_readers_answers_it_and_leaves_the_transfer_running() {
    // The other reader is still waiting for exactly these bytes, so the fetch
    // goes ahead — but the one that left is still answered with an error rather
    // than left parked.
    let mut m = Machine::new();
    let _ = m.on_request(req(1, A, read(0, 16)));
    let _ = m.on_request(req(2, A, read(0, 16))); // joins
    assert_eq!(
        answers(&m.abandon(RequestId(1))),
        vec![(vec![RequestId(1)], Outcome::Failed(Failure::Interrupted))]
    );

    let after_row = m.on_completion(A, a_file());
    assert_eq!(
        jobs(&after_row),
        vec![Job::FetchRange {
            object: A,
            offset: 0,
            len: 16
        }],
        "the fetch goes ahead for the reader that remains"
    );

    let done = m.on_completion(A, Completion::Bytes);
    assert_eq!(answers(&done), vec![(vec![RequestId(2)], Outcome::Ok)]);
}

#[test]
fn abandoning_a_queued_request_answers_it_too() {
    // A reader parked behind the running flow, not riding it, is just as owed an
    // answer — its page is locked all the same.
    let mut m = Machine::new();
    let _ = m.on_request(req(1, A, read(0, 16)));
    let _ = m.on_request(req(2, A, read(32, 16))); // a different range: queues
    assert_eq!(
        answers(&m.abandon(RequestId(2))),
        vec![(vec![RequestId(2)], Outcome::Failed(Failure::Interrupted))],
        "the queued reader is answered"
    );

    // And having left the queue, it does not start when the flow ends.
    let after_row = m.on_completion(A, a_file());
    let done = m.on_completion(A, Completion::Bytes);
    let served: Vec<_> = answers(&after_row)
        .into_iter()
        .chain(answers(&done))
        .collect();
    assert_eq!(
        served,
        vec![(vec![RequestId(1)], Outcome::Ok)],
        "only the reader that stayed is served, exactly once"
    );
}

#[test]
fn abandoning_an_unknown_request_answers_nobody() {
    // It was already answered. An empty action list is how the caller tells a
    // live request from one that finished while the message was on its way.
    let mut m = Machine::new();
    assert!(m.abandon(RequestId(99)).is_empty());
}

/// A directory whose children are known, but whose listing is past its
/// revalidation interval — the state a second `ls` within the same minute
/// finds.
fn a_stale_directory() -> Completion {
    Completion::Node(NodeFacts {
        found: true,
        dir: true,
        children_loaded: true,
        listing_stale: true,
        ..NodeFacts::default()
    })
}

/// Serving a stale listing must not put the next caller behind the refresh it
/// causes.
///
/// Measured on a real mount before this held: `ls` took 20 ms, the next `ls`
/// took 2.9 seconds, and the one after that 20 ms again. The middle one had
/// queued behind the PROPFIND that the first one asked for. A file manager
/// makes one `lookup` per visible entry, all keyed on the same directory, so it
/// paid that wait too — and it looked like the mount had hung.
#[test]
fn a_background_refresh_does_not_hold_up_the_next_listing() {
    let mut m = Machine::new();

    let actions = m.on_request(req(1, A, Intent::Enumerate));
    assert_eq!(jobs(&actions), vec![Job::ReadNode { object: A }]);
    let actions = m.on_completion(A, a_stale_directory());
    assert_eq!(jobs(&actions), vec![Job::ReadChildren { object: A }]);
    let actions = m.on_completion(A, Completion::Listed);

    // The refresh is asked for …
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::Refresh { object } if *object == A)),
        "a stale listing still owes the next caller a fresher one: {actions:?}"
    );
    // … but not as work on the object, which is what used to make it a queue.
    assert!(
        !actions.iter().any(|a| matches!(a, Action::Schedule { .. })),
        "a refresh must not be scheduled onto the object: {actions:?}"
    );
    assert!(
        !m.is_busy(A),
        "the directory is free the moment its listing has been served"
    );

    // So the next caller starts at once instead of waiting for a PROPFIND.
    let actions = m.on_request(req(2, A, Intent::Enumerate));
    assert_eq!(
        jobs(&actions),
        vec![Job::ReadNode { object: A }],
        "the second listing must start immediately, not queue: {actions:?}"
    );
}

/// The GNOME atomic save, at the level of the machine.
///
/// An editor writes `.goutputstream-XXXXXX`, closes it — the publish keeps the
/// buffer rather than uploading, because the *name* says temporary — and then
/// renames it over the document. The rename is the moment the name stops saying
/// that, so the bytes have to go out under the new one. If they do not, the
/// user's save is silently lost.
#[test]
fn renaming_a_temporary_onto_a_document_publishes_what_it_holds() {
    let mut m = Machine::new();
    let parent = A;
    let child = B;

    // The editor's temporary: written, closed, kept back because it is ignored.
    let mut buffer = Buffer::new(String::new());
    buffer.dirty = true;
    buffer.ignored = true;
    m.registry_mut().open(child, buffer);

    // rename(".goutputstream-ABC", "Notes.txt")
    let actions = m.on_request(req(
        1,
        parent,
        Intent::Move {
            from_name: ".goutputstream-ABC".into(),
            to_parent: parent,
            to_name: "Notes.txt".into(),
        },
    ));
    assert_eq!(
        jobs(&actions),
        vec![Job::ReadChild {
            parent,
            name: ".goutputstream-ABC".into()
        }]
    );

    // Resolved: the move is handed to the file itself, which is idle, so it
    // starts there at once by reading its own row. The directory stays parked.
    let temporary = || {
        Completion::Node(NodeFacts {
            id: child,
            found: true,
            parent,
            materialised: false,
            ignored: true,
            ..NodeFacts::default()
        })
    };
    let actions = m.on_completion(parent, temporary());
    assert_eq!(jobs(&actions), vec![Job::ReadNode { object: child }]);
    assert!(m.is_busy(parent), "the directory is occupied for the move");

    // It exists locally and has never been on the server, so the rename is ours
    // alone — rows only, no server-side MOVE.
    let actions = m.on_completion(child, temporary());
    assert_eq!(
        jobs(&actions),
        vec![Job::MoveRows {
            object: child,
            to_parent: parent,
            to_name: "Notes.txt".into()
        }]
    );

    let actions = m.on_completion(child, Completion::Done);
    assert_eq!(answers(&actions), vec![(vec![RequestId(1)], Outcome::Ok)]);
    assert!(
        !m.is_busy(parent),
        "the directory is released with the move"
    );
    assert!(
        actions.iter().any(|a| matches!(
            a,
            Action::Schedule {
                object,
                intent: Intent::Publish
            } if *object == child
        )),
        "the renamed buffer must be published under its new name: {actions:?}"
    );
}

/// The one job a step handed out.
fn only_job(actions: &[Action]) -> Job {
    let js = jobs(actions);
    assert_eq!(js.len(), 1, "exactly one job expected: {actions:?}");
    js[0].clone()
}

/// `echo q > n1; mv n1 n2`, with the rename arriving while the new file's first
/// upload is still on its way.
///
/// The upload reads the file's path when it starts. A rename that moved the rows
/// underneath it sent the bytes to the old name and left nothing under the new
/// one. The rename has to wait for that upload — the collision policy says so —
/// and then, the file being on the server by then, move it there too.
#[test]
fn a_rename_waits_for_the_upload_of_the_file_it_renames() {
    let mut m = Machine::new();
    let parent = A;
    let child = B;
    let new_file = || {
        Completion::Node(NodeFacts {
            id: child,
            found: true,
            parent,
            materialised: false,
            ..NodeFacts::default()
        })
    };

    // n1: created, written, closed — its first upload is in flight.
    let mut buffer = Buffer::new(String::new());
    buffer.dirty = true;
    m.registry_mut().open(child, buffer);
    let a = m.on_request(req(10, child, Intent::Publish));
    assert_eq!(only_job(&a), Job::ReadNode { object: child });
    let a = m.on_completion(child, new_file());
    assert!(matches!(only_job(&a), Job::MarkPending { .. }));
    let a = m.on_completion(child, Completion::Done);
    assert!(matches!(only_job(&a), Job::BufferSize { .. }));
    let a = m.on_completion(child, Completion::Size(1));
    assert!(matches!(only_job(&a), Job::Upload { .. }));

    // mv n1 n2 — keyed on the directory, as the kernel sends it.
    let a = m.on_request(req(
        1,
        parent,
        Intent::Move {
            from_name: "n1".into(),
            to_parent: parent,
            to_name: "n2".into(),
        },
    ));
    assert_eq!(
        only_job(&a),
        Job::ReadChild {
            parent,
            name: "n1".into()
        }
    );
    let a = m.on_completion(parent, new_file());
    // Handed to the file, which is busy: the move waits its turn. Nothing runs
    // beside the upload, nobody is answered yet, the directory stays occupied.
    assert!(
        jobs(&a).is_empty(),
        "the move must not run beside the upload: {a:?}"
    );
    assert!(answers(&a).is_empty());
    assert!(m.is_busy(parent));

    // The upload lands and the publish runs to its end...
    let a = m.on_completion(
        child,
        Completion::Uploaded {
            etag: "e1".into(),
            size: 1,
        },
    );
    assert!(matches!(only_job(&a), Job::RecordVersion { .. }));
    let a = m.on_completion(child, Completion::Done);
    assert!(matches!(only_job(&a), Job::StoreBlob { .. }));
    let a = m.on_completion(child, Completion::Done);
    assert!(matches!(only_job(&a), Job::ListRemote { .. }));
    let a = m.on_completion(child, Completion::Listed);
    assert!(matches!(only_job(&a), Job::ClearPending { .. }));
    let a = m.on_completion(child, Completion::Done);
    assert!(matches!(only_job(&a), Job::DiscardBuffer { .. }));
    // ...and only then does the move start, reading where the file is now.
    let a = m.on_completion(child, Completion::Done);
    assert_eq!(only_job(&a), Job::ReadNode { object: child });

    // The upload gave it a server identity, so this is a MOVE there as well as
    // in the rows — the bytes end up under the new name.
    let a = m.on_completion(
        child,
        Completion::Node(NodeFacts {
            id: child,
            found: true,
            parent,
            materialised: true,
            ..NodeFacts::default()
        }),
    );
    assert_eq!(
        only_job(&a),
        Job::MoveRemote {
            object: child,
            to_parent: parent,
            to_name: "n2".into()
        }
    );
    let a = m.on_completion(child, Completion::Done);
    assert_eq!(
        only_job(&a),
        Job::MoveRows {
            object: child,
            to_parent: parent,
            to_name: "n2".into()
        }
    );
    let a = m.on_completion(child, Completion::Done);
    assert_eq!(answers(&a), vec![(vec![RequestId(1)], Outcome::Ok)]);
    assert!(
        !m.is_busy(parent),
        "the directory is released with the move"
    );
    assert!(!m.is_busy(child));
}

/// A rename abandoned while it still waits behind the upload must not leave
/// its directory parked for good.
#[test]
fn an_abandoned_rename_still_releases_its_directory() {
    let mut m = Machine::new();
    let parent = A;
    let child = B;

    // B busy with anything long-running; a read will do.
    let a = m.on_request(req(10, child, read(0, 16)));
    assert_eq!(only_job(&a), Job::ReadNode { object: child });

    let _ = m.on_request(req(
        1,
        parent,
        Intent::Move {
            from_name: "n1".into(),
            to_parent: parent,
            to_name: "n2".into(),
        },
    ));
    let _ = m.on_completion(
        parent,
        Completion::Node(NodeFacts {
            id: child,
            found: true,
            parent,
            ..NodeFacts::default()
        }),
    );
    assert!(m.is_busy(parent), "parked on the queued relocation");

    // The caller gives up while the move is still queued on the file.
    let a = m.abandon(RequestId(1));
    assert_eq!(
        answers(&a),
        vec![(vec![RequestId(1)], Outcome::Failed(Failure::Interrupted))]
    );
    assert!(!m.is_busy(parent), "the directory must not stay parked");
}

/// `echo q > n1; rm n1`, with the delete arriving while the new file's first
/// upload is still on its way.
///
/// Deleting only the rows underneath that upload let it land anyway, and the
/// next listing brought the file back. The delete has to wait for the upload —
/// the collision policy says so — and then delete what really landed.
#[test]
fn a_remove_waits_for_the_upload_of_the_file_it_removes() {
    let mut m = Machine::new();
    let parent = A;
    let child = B;
    let new_file = || {
        Completion::Node(NodeFacts {
            id: child,
            found: true,
            parent,
            materialised: false,
            ..NodeFacts::default()
        })
    };

    // n1: created, written, closed — its first upload is in flight.
    let mut buffer = Buffer::new(String::new());
    buffer.dirty = true;
    m.registry_mut().open(child, buffer);
    let _ = m.on_request(req(10, child, Intent::Publish));
    let _ = m.on_completion(child, new_file()); // -> MarkPending
    let _ = m.on_completion(child, Completion::Done); // -> BufferSize
    let a = m.on_completion(child, Completion::Size(1));
    assert!(matches!(only_job(&a), Job::Upload { .. }));

    // rm n1 — keyed on the directory, as the kernel sends it.
    let a = m.on_request(req(1, parent, Intent::Remove { name: "n1".into() }));
    assert_eq!(
        only_job(&a),
        Job::ReadChild {
            parent,
            name: "n1".into()
        }
    );
    let a = m.on_completion(parent, new_file());
    assert!(
        jobs(&a).is_empty(),
        "the delete must not run beside the upload: {a:?}"
    );
    assert!(answers(&a).is_empty());
    assert!(m.is_busy(parent));

    // The upload lands and the publish runs to its end ...
    let _ = m.on_completion(
        child,
        Completion::Uploaded {
            etag: "e1".into(),
            size: 1,
        },
    ); // -> RecordVersion
    let _ = m.on_completion(child, Completion::Done); // -> StoreBlob
    let _ = m.on_completion(child, Completion::Done); // -> ListRemote
    let _ = m.on_completion(child, Completion::Listed); // -> ClearPending
    let _ = m.on_completion(child, Completion::Done); // -> DiscardBuffer
    let a = m.on_completion(child, Completion::Done);
    // ... and only then does the delete start, reading where the file is now.
    assert_eq!(only_job(&a), Job::ReadNode { object: child });

    // It is on the server now, so the delete goes there too.
    let a = m.on_completion(
        child,
        Completion::Node(NodeFacts {
            id: child,
            found: true,
            parent,
            materialised: true,
            ..NodeFacts::default()
        }),
    );
    assert_eq!(only_job(&a), Job::DeleteRemote { object: child });
    let a = m.on_completion(child, Completion::Done);
    assert_eq!(only_job(&a), Job::DiscardBuffer { object: child });
    let a = m.on_completion(child, Completion::Done);
    assert_eq!(only_job(&a), Job::RemoveRows { object: child });
    let a = m.on_completion(child, Completion::Done);
    assert_eq!(answers(&a), vec![(vec![RequestId(1)], Outcome::Ok)]);
    assert!(
        !m.is_busy(parent),
        "the directory is released with the delete"
    );
}

/// A directory that had to be listed from the server was just refreshed by that
/// listing. Asking for another one is a second PROPFIND of a directory nobody
/// has touched since.
///
/// It cost more than the round-trip. The redundant refresh reconciled the
/// server's state into the database moments after the first listing, so a
/// change made in between was already recorded by the time the syncer compared
/// — and the syncer's whole job is to notice exactly that.
#[test]
fn a_listing_fetched_from_the_server_does_not_ask_for_a_refresh() {
    let mut m = Machine::new();

    let actions = m.on_request(req(1, A, Intent::Enumerate));
    assert_eq!(jobs(&actions), vec![Job::ReadNode { object: A }]);

    // Never listed, so it is "stale" in the only sense that matters here: there
    // is nothing to serve without asking the server.
    let actions = m.on_completion(
        A,
        Completion::Node(NodeFacts {
            found: true,
            dir: true,
            children_loaded: false,
            listing_stale: true,
            ..NodeFacts::default()
        }),
    );
    assert_eq!(jobs(&actions), vec![Job::ListRemote { object: A }]);

    let actions = m.on_completion(A, Completion::Listed);
    assert_eq!(jobs(&actions), vec![Job::ReadChildren { object: A }]);
    let actions = m.on_completion(A, Completion::Listed);

    assert!(
        !actions.iter().any(|a| matches!(a, Action::Refresh { .. })),
        "the listing that just ran *is* the refresh: {actions:?}"
    );
}

/// A new file's first upload lands, then the listing that should give it its
/// server id fails, and the buffer is kept for the retry. Whatever is uploaded
/// next from that buffer is based on the version that landed — it asserts that
/// version, not "must not exist" and not nothing, or it collides with our own
/// file or overwrites blind.
#[test]
fn a_kept_buffer_is_based_on_the_version_its_upload_landed_as() {
    let mut m = Machine::new();
    let mut buffer = Buffer::new(String::new());
    buffer.dirty = true;
    m.registry_mut().open(A, buffer);
    let new_file = Completion::Node(NodeFacts {
        id: A,
        found: true,
        parent: B,
        materialised: false,
        ..NodeFacts::default()
    });

    let _ = m.on_request(req(1, A, Intent::Publish)); // -> ReadNode
    let _ = m.on_completion(A, new_file); // -> MarkPending
    let _ = m.on_completion(A, Completion::Done); // -> BufferSize
    let _ = m.on_completion(A, Completion::Size(0)); // -> Upload
    let _ = m.on_completion(
        A,
        Completion::Uploaded {
            etag: "e1".into(),
            size: 0,
        },
    ); // -> RecordVersion
    let _ = m.on_completion(A, Completion::Done); // -> StoreBlob
    let a = m.on_completion(A, Completion::Done);
    assert_eq!(jobs(&a), vec![Job::ListRemote { object: B }]);
    // The relist fails: the flow ends; the buffer and the owed upload stay.
    let _ = m.on_completion(A, Completion::Failed(Failure::Io));
    assert!(!m.is_busy(A));

    // More is written and flushed. The row reads as on the server now — its
    // version was recorded — though still without an id.
    let _ = m.on_request(req(2, A, Intent::Publish));
    let _ = m.on_completion(
        A,
        Completion::Node(NodeFacts {
            id: A,
            found: true,
            parent: B,
            materialised: true,
            etag: "e1".into(),
            ..NodeFacts::default()
        }),
    ); // -> MarkPending
    let _ = m.on_completion(A, Completion::Done); // -> BufferSize
    let a = m.on_completion(A, Completion::Size(8));
    assert!(
        matches!(
            jobs(&a).as_slice(),
            [Job::Upload { precondition: Precondition::Match(e), .. }] if e == "e1"
        ),
        "the next upload must assert the version that landed: {a:?}"
    );
}
