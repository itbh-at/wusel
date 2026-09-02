// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Stopping is bounded, whatever the pools are doing.
//!
//! Shutdown used to join every worker, and a worker only notices that it should
//! stop between jobs. One request against a server that had stopped answering
//! therefore held the process for as long as the HTTP read timeout, and a queued
//! backlog held it longer still — past systemd's stop timeout, which then killed
//! the daemon with `SIGABRT` and a core dump. Restarting the service cost 45
//! seconds and ended in a crash report.
//!
//! So the wait has a deadline. This drives the case that produced it: a read
//! frozen mid-flight, and a substrate dropped while it is still frozen.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wusel_core::content::ContentSource;
use wusel_core::runtime::{Context, Pools, Substrate, SHUTDOWN_GRACE};
use wusel_core::state::{NodeRow, StateDb};
use wusel_fsm::{Intent, ObjectId, Request, RequestId};

/// A read that announces itself and then blocks until released — a worker that
/// is provably still inside a job when shutdown begins.
struct FrozenContent {
    started: Sender<()>,
    release: Mutex<Receiver<()>>,
}

impl ContentSource for FrozenContent {
    fn read(&self, _node: &NodeRow, _offset: u64, len: u32) -> wusel_core::Result<Vec<u8>> {
        let _ = self.started.send(());
        // Long enough that the shutdown below cannot be waiting for it.
        let _ = self
            .release
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(SHUTDOWN_GRACE * 6);
        Ok(vec![0u8; len as usize])
    }
}

#[test]
fn stopping_does_not_wait_for_a_worker_stuck_in_a_job() {
    let dir = std::env::temp_dir().join(format!("wusel-shutdown-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test directory");

    let db_path = dir.join("state.sqlite");
    let mut db = StateDb::open(&db_path).expect("open the state database");
    db.insert_local_file(wusel_core::state::ROOT_INODE, "big.bin")
        .expect("insert a file to read");
    let object = ObjectId(
        db.child_by_name(wusel_core::state::ROOT_INODE, "big.bin")
            .expect("look the file up")
            .expect("the file we just inserted")
            .inode,
    );
    drop(db);

    let (started_tx, started_rx) = channel::<()>();
    let (release_tx, release_rx) = channel::<()>();
    let content = Arc::new(FrozenContent {
        started: started_tx,
        release: Mutex::new(release_rx),
    });

    let ctx = Context {
        pins: Arc::new(wusel_core::pins::Pins::new(&dir)),
        open_pinned: wusel_core::config::OpenPinned::default(),
        metered: Arc::new(wusel_core::runtime::Metered::new(Arc::new(Mutex::new(
            wusel_core::desktop::null(),
        )))),
        db_path,
        content: Arc::clone(&content) as Arc<dyn ContentSource>,
        scratch_dir: dir.join("scratch"),
        ignore_patterns: Vec::new(),
        revalidate_secs: 30,
        push_floor_secs: 2,
        invalidate_after: Arc::new(std::sync::atomic::AtomicI64::new(0)),
        async_upload: true,
        write: None,
        quota: None,
    };
    let (substrate, _answers) = Substrate::start(&ctx, Pools::default()).expect("start");

    substrate
        .submit(Request {
            id: RequestId(1),
            object,
            intent: Intent::Fetch { offset: 0, len: 16 },
        })
        .expect("submit the read");
    started_rx
        .recv_timeout(SHUTDOWN_GRACE)
        .expect("the read never reached the worker");

    // The worker is inside the frozen read. Stopping must not wait for it.
    let began = Instant::now();
    drop(substrate);
    let waited = began.elapsed();

    assert!(
        waited < SHUTDOWN_GRACE + Duration::from_secs(3),
        "stopping took {waited:?}; it must give up after about {SHUTDOWN_GRACE:?} \
         rather than wait for the job to finish"
    );

    // Let the straggler go, so the temp directory can be removed.
    let _ = release_tx.send(());
    std::thread::sleep(Duration::from_millis(200));
    let _ = std::fs::remove_dir_all(&dir);
}
