// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The buffer half of the write path, on the substrate.
//!
//! Writing and then reading the same object back is the sharpest small test of
//! the design: it only works if the machine's registry is the authority on
//! whether a buffer is open. The read's three-way choice consults exactly that,
//! so a read-back that returns the written bytes proves the write's bookkeeping
//! landed where decisions can see it — and one that goes to the network proves
//! it did not.

use std::sync::Arc;
use std::time::Duration;

use wusel_core::content::ContentSource;
use wusel_core::runtime::{Answered, Context, Payload, Pools, Substrate};
use wusel_core::state::{NodeRow, StateDb};
use wusel_fsm::{Intent, ObjectId, Outcome, Request, RequestId};

const PATIENCE: Duration = Duration::from_secs(5);

/// A source that would answer with something recognisably *not* the buffer, so
/// a read served from the wrong place cannot pass unnoticed.
struct WrongBytes;

impl ContentSource for WrongBytes {
    fn read(&self, _node: &NodeRow, _offset: u64, len: u32) -> wusel_core::Result<Vec<u8>> {
        Ok(vec![b'X'; len as usize])
    }
}

fn seeded(dir: &std::path::Path) -> (std::path::PathBuf, ObjectId) {
    let path = dir.join("state.sqlite");
    let mut db = StateDb::open(&path).expect("open the state database");
    db.insert_local_file(wusel_core::state::ROOT_INODE, "note.txt")
        .expect("insert a file");
    let node = db
        .child_by_name(wusel_core::state::ROOT_INODE, "note.txt")
        .expect("look it up")
        .expect("the file we just inserted");
    (path, ObjectId(node.inode))
}

fn take(answers: &std::sync::mpsc::Receiver<Answered>, who: RequestId) -> Answered {
    loop {
        let a = answers
            .recv_timeout(PATIENCE)
            .unwrap_or_else(|e| panic!("no answer for {who:?}: {e}"));
        if a.requests.contains(&who) {
            return a;
        }
    }
}

#[test]
fn bytes_written_are_read_back_from_the_buffer_not_the_server() {
    let dir = std::env::temp_dir().join(format!("wusel-wbuf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test directory");
    let (db_path, object) = seeded(&dir);

    let ctx = Context {
        pins: std::sync::Arc::new(wusel_core::pins::Pins::new(&dir)),
        open_pinned: wusel_core::config::OpenPinned::default(),
        metered: std::sync::Arc::new(wusel_core::runtime::Metered::new(std::sync::Arc::new(
            std::sync::Mutex::new(wusel_core::desktop::null()),
        ))),
        db_path,
        content: Arc::new(WrongBytes),
        scratch_dir: dir.join("scratch"),
        ignore_patterns: Vec::new(),
        revalidate_secs: 30,
        push_floor_secs: 2,
        invalidate_after: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
        async_upload: true,
        write: None,
    };
    let (substrate, answers) = Substrate::start(&ctx, Pools::default()).expect("start");

    // A newly inserted file has no content, so the write needs no download —
    // it starts from an empty buffer.
    substrate
        .submit_write(
            Request {
                id: RequestId(1),
                object,
                intent: Intent::Write { offset: 0, len: 5 },
            },
            b"hello".to_vec(),
        )
        .expect("submit the write");
    assert_eq!(take(&answers, RequestId(1)).outcome, Outcome::Ok);

    substrate
        .submit(Request {
            id: RequestId(2),
            object,
            intent: Intent::Fetch { offset: 0, len: 5 },
        })
        .expect("submit the read");
    let read = take(&answers, RequestId(2));

    drop(substrate);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(read.outcome, Outcome::Ok);
    assert_eq!(
        bytes(&read.payload),
        b"hello",
        "the read must come from the open buffer; 'XXXXX' would mean it went to the source instead"
    );
}

#[test]
fn a_second_write_lands_after_the_first_and_keeps_its_bytes() {
    // Two writes to one object are ordered by the machine's FIFO, and their
    // bytes are consumed in the same order — otherwise the second write's
    // content would end up at the first one's offset.
    let dir = std::env::temp_dir().join(format!("wusel-wbuf2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test directory");
    let (db_path, object) = seeded(&dir);

    let ctx = Context {
        pins: std::sync::Arc::new(wusel_core::pins::Pins::new(&dir)),
        open_pinned: wusel_core::config::OpenPinned::default(),
        metered: std::sync::Arc::new(wusel_core::runtime::Metered::new(std::sync::Arc::new(
            std::sync::Mutex::new(wusel_core::desktop::null()),
        ))),
        db_path,
        content: Arc::new(WrongBytes),
        scratch_dir: dir.join("scratch"),
        ignore_patterns: Vec::new(),
        revalidate_secs: 30,
        push_floor_secs: 2,
        invalidate_after: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
        async_upload: true,
        write: None,
    };
    let (substrate, answers) = Substrate::start(&ctx, Pools::default()).expect("start");

    for (id, offset, bytes) in [(1u64, 0u64, &b"abc"[..]), (2, 3, &b"def"[..])] {
        substrate
            .submit_write(
                Request {
                    id: RequestId(id),
                    object,
                    intent: Intent::Write {
                        offset,
                        len: bytes.len() as u32,
                    },
                },
                bytes.to_vec(),
            )
            .expect("submit a write");
    }
    assert_eq!(take(&answers, RequestId(1)).outcome, Outcome::Ok);
    assert_eq!(take(&answers, RequestId(2)).outcome, Outcome::Ok);

    substrate
        .submit(Request {
            id: RequestId(3),
            object,
            intent: Intent::Fetch { offset: 0, len: 6 },
        })
        .expect("submit the read");
    let read = take(&answers, RequestId(3));

    drop(substrate);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(bytes(&read.payload), b"abcdef");
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
