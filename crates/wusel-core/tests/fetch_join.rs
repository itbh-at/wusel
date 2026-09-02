// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Gate 3: two readers of the same range cost **one** transfer.
//!
//! Today this exists only for uploads, as a hand-written per-inode map. Here it
//! is one row of the collision policy, so it applies to reads for free — and
//! this test is what says so.
//!
//! Deterministic rather than timed: the content source blocks until the test
//! releases it, so the second reader provably arrives while the first fetch is
//! still in flight. A `sleep` would make the same claim on a good day.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wusel_core::content::ContentSource;
use wusel_core::runtime::{Answered, Context, Payload, Pools, Substrate};
use wusel_core::state::{NodeRow, StateDb};
use wusel_fsm::{Failure, Intent, ObjectId, Outcome, Request, RequestId};

const PATIENCE: Duration = Duration::from_secs(5);
const RANGE: (u64, u32) = (0, 16);

/// A source that announces each fetch and then waits to be let go.
struct GatedContent {
    started: Sender<()>,
    release: Mutex<Receiver<()>>,
    reads: AtomicUsize,
}

impl ContentSource for GatedContent {
    fn read(&self, _node: &NodeRow, _offset: u64, len: u32) -> wusel_core::Result<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let _ = self.started.send(());
        // Held until the test says so, so "still in flight" is a fact rather
        // than a hope.
        let _ = self
            .release
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(PATIENCE);
        Ok(vec![7u8; len as usize])
    }
}

fn seeded_db(dir: &std::path::Path) -> (std::path::PathBuf, ObjectId) {
    let path = dir.join("state.sqlite");
    let mut db = StateDb::open(&path).expect("open the state database");
    db.insert_local_file(wusel_core::state::ROOT_INODE, "big.bin")
        .expect("insert a file to read");
    let node = db
        .child_by_name(wusel_core::state::ROOT_INODE, "big.bin")
        .expect("look the file up")
        .expect("the file we just inserted");
    (path, ObjectId(node.inode))
}

fn read(id: u64, object: ObjectId) -> Request {
    Request {
        id: RequestId(id),
        object,
        intent: Intent::Fetch {
            offset: RANGE.0,
            len: RANGE.1,
        },
    }
}

#[test]
fn a_second_reader_of_the_same_range_joins_the_transfer_in_flight() {
    let dir = std::env::temp_dir().join(format!("wusel-join-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test directory");
    let (db_path, object) = seeded_db(&dir);

    let (started_tx, started_rx) = channel::<()>();
    let (release_tx, release_rx) = channel::<()>();
    let content = Arc::new(GatedContent {
        started: started_tx,
        release: Mutex::new(release_rx),
        reads: AtomicUsize::new(0),
    });

    let ctx = Context {
        pins: std::sync::Arc::new(wusel_core::pins::Pins::new(&dir)),
        open_pinned: wusel_core::config::OpenPinned::default(),
        metered: std::sync::Arc::new(wusel_core::runtime::Metered::new(std::sync::Arc::new(
            std::sync::Mutex::new(wusel_core::desktop::null()),
        ))),
        db_path,
        content: Arc::clone(&content) as Arc<dyn ContentSource>,
        scratch_dir: dir.join("scratch"),
        ignore_patterns: Vec::new(),
        revalidate_secs: 30,
        push_floor_secs: 2,
        invalidate_after: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
        async_upload: true,
        write: None,
        quota: None,
    };
    let (substrate, answers) = Substrate::start(&ctx, Pools::default()).expect("start");

    substrate.submit(read(1, object)).expect("submit the first");
    started_rx
        .recv_timeout(PATIENCE)
        .expect("the first fetch never started");

    // Certainly during the transfer now.
    substrate
        .submit(read(2, object))
        .expect("submit the second");
    // Give the decider a moment to take it, so a missed join shows up as a
    // second fetch rather than as a race we did not run.
    std::thread::sleep(Duration::from_millis(200));

    let _ = release_tx.send(());
    let answered: Vec<Answered> = collect(&answers, 1);

    drop(substrate);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        content.reads.load(Ordering::SeqCst),
        1,
        "the second reader must ride along, not start its own transfer"
    );
    assert_eq!(answered.len(), 1, "one flow, so one answer for both");
    let a = &answered[0];
    assert_eq!(
        a.requests,
        vec![RequestId(1), RequestId(2)],
        "both readers answered together"
    );
    assert_eq!(a.outcome, Outcome::Ok);
    assert_eq!(
        bytes(&a.payload),
        &[7u8; RANGE.1 as usize][..],
        "and both get the bytes that were actually fetched"
    );
}

/// Take `n` answers, or fail saying how far it got.
fn collect(answers: &Receiver<Answered>, n: usize) -> Vec<Answered> {
    let mut out = Vec::new();
    for _ in 0..n {
        match answers.recv_timeout(PATIENCE) {
            Ok(a) => out.push(a),
            Err(e) => panic!("only {} of {n} answers arrived: {e}", out.len()),
        }
    }
    out
}

/// The bytes an answer carried, or an empty slice — so a wrong variant fails
/// the assertion rather than the unwrap.
fn bytes(p: &Payload) -> &[u8] {
    match p {
        Payload::Bytes(b) => b,
        Payload::None
        | Payload::Node(_)
        | Payload::Entries(_)
        | Payload::Written(_)
        | Payload::State(_) => &[],
    }
}

#[test]
fn an_abandoned_fetch_is_answered_with_an_error_and_leaves_no_slot_behind() {
    // Cancellation, end to end through the substrate: a reader that goes away
    // mid-transfer is still answered — with an error — and leaves the object
    // usable.
    //
    // The error answer is the fix for a real desktop hang. The kernel holds a
    // locked page for the outstanding read until it gets *some* reply; drop it
    // and the page, and every later reader of it, wedge uninterruptibly. The
    // most ordinary trigger was readahead: read part of a file and close it, and
    // the readahead reads still in flight were abandoned without a reply.
    //
    // What this does *not* prove is that the abort flag is what gives the
    // transfer up — that effect is checked where it can be isolated:
    // `occupancy.rs` in wusel-fsm, which fails when the flag is disabled.
    let dir = std::env::temp_dir().join(format!("wusel-abandon-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test directory");
    let (db_path, object) = seeded_db(&dir);

    let (started_tx, started_rx) = channel::<()>();
    let (release_tx, release_rx) = channel::<()>();
    let content = Arc::new(GatedContent {
        started: started_tx,
        release: Mutex::new(release_rx),
        reads: AtomicUsize::new(0),
    });

    let ctx = Context {
        pins: std::sync::Arc::new(wusel_core::pins::Pins::new(&dir)),
        open_pinned: wusel_core::config::OpenPinned::default(),
        metered: std::sync::Arc::new(wusel_core::runtime::Metered::new(std::sync::Arc::new(
            std::sync::Mutex::new(wusel_core::desktop::null()),
        ))),
        db_path,
        content: Arc::clone(&content) as Arc<dyn ContentSource>,
        scratch_dir: dir.join("scratch"),
        ignore_patterns: Vec::new(),
        revalidate_secs: 30,
        push_floor_secs: 2,
        invalidate_after: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
        async_upload: true,
        write: None,
        quota: None,
    };
    let (substrate, answers) = Substrate::start(&ctx, Pools::default()).expect("start");

    substrate.submit(read(1, object)).expect("submit the read");
    started_rx
        .recv_timeout(PATIENCE)
        .expect("the fetch never started");

    // The reader dies. It is answered at once with an error — the kernel needs
    // that to release the page it locked — while the step already running
    // finishes (a side effect in flight is never cut off) and the flow gives up
    // at the boundary after it.
    substrate.abandon(RequestId(1)).expect("abandon");

    let answered = answers
        .recv_timeout(PATIENCE)
        .expect("the abandoned read must still be answered, or its page wedges");
    assert_eq!(answered.requests, vec![RequestId(1)]);
    assert!(
        matches!(answered.outcome, Outcome::Failed(Failure::Interrupted)),
        "answered with an error, not bytes: {:?}",
        answered.outcome
    );

    let _ = release_tx.send(());

    // And only once: the flow that gave up owes nothing more.
    match answers.recv_timeout(std::time::Duration::from_millis(500)) {
        Err(_) => {}
        Ok(a) => panic!("the abandoned flow answered a second time: {a:?}"),
    }

    // And the object is free: a fresh read on it starts immediately rather than
    // queueing behind the flow nobody is waiting for.
    substrate
        .submit(read(2, object))
        .expect("submit a second read");
    started_rx
        .recv_timeout(PATIENCE)
        .expect("the object was left occupied by the abandoned flow");
    let _ = release_tx.send(());
    let served = answers.recv_timeout(PATIENCE).expect("the second read");
    assert_eq!(served.requests, vec![RequestId(2)]);
    assert_eq!(served.outcome, Outcome::Ok);

    drop(substrate);
    let _ = std::fs::remove_dir_all(&dir);
}
