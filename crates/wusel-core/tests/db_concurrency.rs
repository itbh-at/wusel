// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Gate 2: a metadata read is served **while the writer is stuck**.
//!
//! This is the test that decides whether the execution substrate is the new
//! shape or the old one wearing new names, so it has to fail on the old one.
//! Merely reading during a held write transaction would not do that: a read
//! never needs the write lock, so a single owner thread would pass it too. The
//! discriminating question is what happens to a read when the *writer* cannot
//! make progress. One owner thread running operations in turn queues the read
//! behind it; separate pools do not.
//!
//! So the adverse condition is injected twice over. A stranger takes
//! `BEGIN IMMEDIATE` and keeps it — a virus scanner, the syncer, another
//! process — and then a write job is submitted, which parks the writer thread
//! on that lock for its whole busy timeout. Only then is the read submitted.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use wusel_core::content::ContentSource;
use wusel_core::runtime::{Context, Pools, Substrate};
use wusel_core::state::{NodeRow, StateDb};
use wusel_fsm::{Intent, ObjectId, Outcome, Request, RequestId};

/// The read must land well inside the busy timeout the database is opened with
/// (5 s), or it did not really run alongside the writer.
const PATIENCE: Duration = Duration::from_secs(2);

/// A content source that never has anything cached. The substrate asks it
/// whether a blob is current while reading a row; for this test the answer is
/// simply "no", and no bytes are ever fetched.
struct NoContent;

impl ContentSource for NoContent {
    fn read(&self, _node: &NodeRow, _offset: u64, _len: u32) -> wusel_core::Result<Vec<u8>> {
        Err(wusel_core::Error::Other("not used in this test".into()))
    }
}

/// A database with one file in it. Returns the path and the object to look at —
/// looked up rather than assumed, so the test does not depend on how inodes
/// happen to be handed out.
fn seeded_db(dir: &std::path::Path) -> (std::path::PathBuf, ObjectId) {
    let path = dir.join("state.sqlite");
    let mut db = StateDb::open(&path).expect("open the state database");
    db.insert_local_file(wusel_core::state::ROOT_INODE, "doc.odt")
        .expect("insert a file to look at");
    let node = db
        .child_by_name(wusel_core::state::ROOT_INODE, "doc.odt")
        .expect("look the file up")
        .expect("the file we just inserted");
    (path, ObjectId(node.inode))
}

#[test]
fn a_metadata_read_is_served_while_a_write_transaction_is_held() {
    let dir = std::env::temp_dir().join(format!("wusel-dbconc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test directory");
    let (db_path, object) = seeded_db(&dir);

    // A stranger holds the write lock for the whole test — a virus scanner, the
    // syncer, another process. Nothing may depend on it letting go.
    let blocker = rusqlite_hold_write_lock(&db_path);

    let ctx = Context {
        pins: std::sync::Arc::new(wusel_core::pins::Pins::new(&dir)),
        open_pinned: wusel_core::config::OpenPinned::default(),
        metered: std::sync::Arc::new(wusel_core::runtime::Metered::new(std::sync::Arc::new(
            std::sync::Mutex::new(wusel_core::desktop::null()),
        ))),
        db_path: db_path.clone(),
        content: Arc::new(NoContent),
        scratch_dir: dir.join("scratch"),
        ignore_patterns: Vec::new(),
        revalidate_secs: 30,
        push_floor_secs: 2,
        invalidate_after: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
        async_upload: true,
        write: None,
    };
    let (substrate, answers) =
        Substrate::start(&ctx, Pools::default()).expect("start the substrate");

    // Park the writer: this insert needs the lock the stranger is holding, so
    // the write thread sits in its busy timeout and makes no progress.
    substrate
        .submit(Request {
            id: RequestId(1),
            object: ObjectId(wusel_core::state::ROOT_INODE),
            intent: Intent::Materialise {
                name: "new.txt".into(),
                dir: false,
            },
        })
        .expect("submit the write");

    // Give it a moment to actually reach the writer, so the read below really
    // does arrive while that thread is stuck rather than before it started.
    std::thread::sleep(Duration::from_millis(200));

    substrate
        .submit(Request {
            id: RequestId(2),
            object,
            intent: Intent::Stat,
        })
        .expect("submit the read");

    // The read must come back; the write cannot, until the stranger lets go.
    let answered = loop {
        match answers.recv_timeout(PATIENCE) {
            Ok(a) if a.requests == vec![RequestId(2)] => break Ok(a),
            Ok(_) => continue, // the write finished after all — keep waiting for the read
            Err(e) => break Err(e),
        }
    };

    // Release the writer only after the verdict, so a pass cannot have come
    // from the lock quietly going away.
    drop(blocker);
    drop(substrate);
    let _ = std::fs::remove_dir_all(&dir);

    match answered {
        Ok(a) => {
            assert_eq!(
                a.outcome,
                Outcome::Ok,
                "the row was read, not merely attempted"
            );
        }
        Err(RecvTimeoutError::Timeout) => panic!(
            "a metadata read waited more than {PATIENCE:?} while the writer was stuck — \
             reads are not independent of writes, which is the whole point of the split"
        ),
        Err(RecvTimeoutError::Disconnected) => panic!("the substrate stopped without answering"),
    }
}

/// Take `BEGIN IMMEDIATE` on a connection of its own and keep it until dropped.
///
/// A deterministic write-lock stall: no timing luck, no sleeping, and it holds
/// for exactly as long as the test wants it to.
fn rusqlite_hold_write_lock(path: &std::path::Path) -> impl Drop {
    struct Held(
        Option<std::thread::JoinHandle<()>>,
        std::sync::mpsc::Sender<()>,
    );
    impl Drop for Held {
        fn drop(&mut self) {
            let _ = self.1.send(());
            if let Some(h) = self.0.take() {
                let _ = h.join();
            }
        }
    }

    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let path = path.to_path_buf();
    let handle = std::thread::spawn(move || {
        let conn = rusqlite::Connection::open(&path).expect("open a second connection");
        conn.busy_timeout(Duration::from_secs(5))
            .expect("set the busy timeout");
        // Not committed and not rolled back: the lock is held for exactly as
        // long as this thread waits below.
        conn.execute_batch("BEGIN IMMEDIATE")
            .expect("take the write lock");
        ready_tx.send(()).expect("announce the lock is held");
        let _ = release_rx.recv();
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the write lock was never taken");
    Held(Some(handle), release_tx)
}
