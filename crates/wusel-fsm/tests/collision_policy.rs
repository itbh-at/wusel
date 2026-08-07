// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! One test per row of the collision table, named after its row.
//!
//! The table is the concurrency policy, so it should be demonstrated rather
//! than asserted — and a row whose test is missing is a row nobody has checked.
//! These need no mount, no server and no database: the policy is a pure
//! function of what is running, what arrived, and what we know about the buffer.

use wusel_fsm::registry::Facts;
use wusel_fsm::script::{Carry, Flow, Step};
use wusel_fsm::{collision, Collision, Intent, ObjectId, RequestId};

const OBJ: ObjectId = ObjectId(7);

/// A flow parked on a given step, which is all the policy looks at.
fn running(intent: Intent, step: Step) -> Flow {
    Flow {
        object: OBJ,
        intent,
        step,
        waiters: vec![RequestId(1)],
        abort: false,
        carry: Carry::default(),
    }
}

fn clean() -> Facts {
    Facts::default()
}

fn dirty_buffer() -> Facts {
    Facts {
        buffer_open: true,
        buffer_dirty: true,
        ignored: false,
        base_etag: "v1".into(),
        pending_mtime: None,
    }
}

fn read(offset: u64, len: u32) -> Intent {
    Intent::Fetch { offset, len }
}

// --- Row 1 -----------------------------------------------------------------

#[test]
fn hydration_meets_a_write_and_waits() {
    // The base being fetched is what a later three-way merge is made of.
    // Aborting it would destroy exactly what the arriving write will need.
    let flow = running(Intent::Write { offset: 0, len: 8 }, Step::WritePrepare);
    let arriving = Intent::Write { offset: 8, len: 8 };
    assert_eq!(collision(&flow, &arriving, &clean()), Collision::Queue);
}

// --- Row 2 -----------------------------------------------------------------

#[test]
fn background_hydration_meets_a_remove_and_aborts() {
    // The object is gone, so the rest of the download is waste — the one shape
    // of arrival that makes running work pointless rather than merely late.
    let flow = running(Intent::Refresh, Step::RefHydrate);
    let arriving = Intent::Remove {
        name: "doc.odt".into(),
    };
    assert_eq!(collision(&flow, &arriving, &clean()), Collision::Abort);
}

// --- Row 3 -----------------------------------------------------------------

#[test]
fn a_second_read_of_the_same_range_joins() {
    let flow = running(read(0, 4096), Step::FetchBytes);
    assert_eq!(
        collision(&flow, &read(0, 4096), &clean()),
        Collision::Join,
        "one transfer should answer both readers"
    );
}

#[test]
fn a_read_of_a_different_range_does_not_join() {
    // The bytes on their way are not the bytes being asked for. Joining here
    // would answer the second reader with somebody else's window.
    let flow = running(read(0, 4096), Step::FetchBytes);
    assert_eq!(
        collision(&flow, &read(4096, 4096), &clean()),
        Collision::Queue
    );
}

// --- Row 4 -----------------------------------------------------------------

#[test]
fn an_upload_meets_a_write_and_queues() {
    // Aborting mid-upload can leave half an object on the server.
    let flow = running(Intent::Publish, Step::PubUpload);
    let arriving = Intent::Write { offset: 0, len: 1 };
    assert_eq!(collision(&flow, &arriving, &clean()), Collision::Queue);
}

// --- Row 5 -----------------------------------------------------------------

#[test]
fn an_upload_meets_a_remove_and_queues_before_deleting() {
    // The upload may already have landed, so the delete has to apply to what is
    // really on the server rather than to what we assumed.
    let flow = running(Intent::Publish, Step::PubUpload);
    let arriving = Intent::Remove {
        name: "doc.odt".into(),
    };
    assert_eq!(collision(&flow, &arriving, &clean()), Collision::Queue);
}

// --- Row 6 -----------------------------------------------------------------

#[test]
fn an_upload_meets_a_rename_and_queues() {
    // The office-suite atomic save: write a temporary file, then rename it onto
    // the document. Ordering is the whole content of that sequence.
    let flow = running(Intent::Publish, Step::PubUpload);
    let arriving = Intent::Move {
        from_name: "doc.odt.tmp".into(),
        to_parent: ObjectId(1),
        to_name: "doc.odt".into(),
    };
    assert_eq!(collision(&flow, &arriving, &clean()), Collision::Queue);
}

// --- Row 7 -----------------------------------------------------------------

#[test]
fn conflict_resolution_makes_everything_queue() {
    // It is reading the buffer, merging it and re-uploading it: nothing else
    // may touch the object until it is finished.
    let flow = running(Intent::Publish, Step::PubConflict);
    for arriving in [
        read(0, 16),
        Intent::Write { offset: 0, len: 16 },
        Intent::Stat,
        Intent::Publish,
        Intent::Remove {
            name: "doc.odt".into(),
        },
        Intent::Move {
            from_name: "x.tmp".into(),
            to_parent: ObjectId(1),
            to_name: "x".into(),
        },
        Intent::Refresh,
    ] {
        assert_eq!(
            collision(&flow, &arriving, &clean()),
            Collision::Queue,
            "{arriving:?} must wait for conflict resolution"
        );
    }
}

// --- Row 8 -----------------------------------------------------------------

#[test]
fn a_second_listing_joins() {
    let flow = running(Intent::Enumerate, Step::EnumRemote);
    assert_eq!(
        collision(&flow, &Intent::Enumerate, &clean()),
        Collision::Join
    );
}

// --- Row 9 -----------------------------------------------------------------

#[test]
fn a_refresh_meeting_an_unsaved_edit_is_skipped() {
    // A server-side change must never overwrite an unsaved local one. Until our
    // upload lands, our copy *is* the newer one.
    for step in [Step::FetchBytes, Step::PubUpload, Step::EnumChildren] {
        let flow = running(Intent::Publish, step);
        assert_eq!(
            collision(&flow, &Intent::Refresh, &dirty_buffer()),
            Collision::Skip,
            "a refresh must not run against a dirty buffer (step {step:?})"
        );
    }
}

#[test]
fn a_refresh_without_a_local_edit_is_not_skipped() {
    // The other half of row 9: with nothing unsaved there is nothing to protect,
    // so the refresh is ordinary work and merely waits its turn.
    let flow = running(Intent::Publish, Step::PubUpload);
    assert_eq!(
        collision(&flow, &Intent::Refresh, &clean()),
        Collision::Queue
    );
}

#[test]
fn at_most_one_refresh_is_pending_per_object() {
    // A web editor with autosave would otherwise queue one hydration per
    // keystroke.
    let flow = running(Intent::Refresh, Step::RefHydrate);
    assert_eq!(
        collision(&flow, &Intent::Refresh, &clean()),
        Collision::Skip
    );
}
