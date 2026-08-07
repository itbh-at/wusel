// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The substrate answers a diagnostics snapshot even while a fetch is stuck.
//!
//! This is the property `wusel doctor` leans on: the decider is not the thing
//! that wedges — a stalled mount is unanswered kernel reads, with the decider
//! sitting idle — so it can always report what the machine is doing. Here a
//! fetch is deliberately frozen mid-flight, and the snapshot must still come
//! back, naming the object by inode and showing the waiter, with no file name.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wusel_core::content::ContentSource;
use wusel_core::runtime::{Context, Pools, Substrate};
use wusel_core::state::{NodeRow, StateDb};
use wusel_fsm::{Intent, ObjectId, Request, RequestId};

const PATIENCE: Duration = Duration::from_secs(5);

/// A content source that announces a read and then blocks until released, so a
/// fetch is provably still in flight when the snapshot is taken.
struct GatedContent {
    started: Sender<()>,
    release: Mutex<Receiver<()>>,
    reads: AtomicUsize,
}

impl ContentSource for GatedContent {
    fn read(&self, _node: &NodeRow, _offset: u64, len: u32) -> wusel_core::Result<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let _ = self.started.send(());
        let _ = self
            .release
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recv_timeout(PATIENCE);
        Ok(vec![0u8; len as usize])
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
        intent: Intent::Fetch { offset: 0, len: 16 },
    }
}

#[test]
fn the_substrate_reports_a_stuck_fetch_while_the_decider_stays_responsive() {
    let dir = std::env::temp_dir().join(format!("wusel-diag-{}", std::process::id()));
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
    };
    let (substrate, _answers) = Substrate::start(&ctx, Pools::default()).expect("start");

    // Idle: nothing in flight, but the pool sizes are already reported.
    let idle = substrate.snapshot().expect("snapshot while idle");
    assert!(idle.machine.objects.is_empty(), "nothing running yet");
    assert_eq!(idle.pools.db_readers, 2);
    assert_eq!(idle.pools.net, 4);
    assert_eq!(idle.pools.file, 2);

    // Freeze a fetch mid-flight.
    substrate.submit(read(1, object)).expect("submit the read");
    started_rx
        .recv_timeout(PATIENCE)
        .expect("the fetch never started");

    // The snapshot comes back even though the fetch is stuck — the decider is
    // not what is blocked.
    let snap = substrate.snapshot().expect("snapshot while a fetch is stuck");
    assert_eq!(snap.machine.objects.len(), 1, "one object is busy");
    let o = &snap.machine.objects[0];
    assert_eq!(o.object, object.0, "named by inode, not by file name");
    assert_eq!(o.intent, "fetch");
    assert!(o.outstanding, "a job is handed out to a worker");
    assert_eq!(o.waiters, 1);

    let _ = release_tx.send(());
    drop(substrate);
    let _ = std::fs::remove_dir_all(&dir);
}
