// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Reproduction: a large file copied in many sequential writes (as `cp` does
//! through the kernel — hundreds of 128 KiB `write()` calls) must accumulate in
//! the buffer and reach the server byte-for-byte. `chunked.rs` covers a single
//! large write; this covers the many-small-writes path that assembles the same
//! content, which is what a real copy exercises.

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn a_large_file_written_in_many_chunks_reaches_the_server() {
    let base = std::env::temp_dir().join(format!("wusel-mock-mw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();
    let engine = common::Engine::start(&addr);

    // ~10 MiB — well over one 4 MiB chunk — written as `cp` would: many small,
    // sequential, offset-increasing writes.
    let total = 10 * 1024 * 1024usize;
    let step = 128 * 1024usize;
    let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();

    let node = engine.create(ROOT_INODE, "big.bin").expect("create");
    let mut off = 0usize;
    while off < total {
        let end = (off + step).min(total);
        engine
            .write(node.inode, off as u64, &data[off..end])
            .expect("write chunk");
        off = end;
    }
    engine.flush(node.inode).expect("flush");
    engine.wait_for_uploads();

    let landed = std::fs::read(fixture.join("big.bin")).unwrap();
    assert_eq!(
        landed.len(),
        total,
        "the server file is the wrong size: got {} bytes, expected {total}",
        landed.len()
    );
    assert_eq!(
        landed, data,
        "the server file is not byte-for-byte the source"
    );

    // Now overwrite it in place — the "compress to ZIP" pattern that spawned a
    // flood of conflicted copies. With the pre-flight precondition check the
    // second chunked upload must land cleanly, replacing the content and leaving
    // no `conflicted copy` sibling behind.
    let data2: Vec<u8> = (0..total).map(|i| ((i + 7) % 251) as u8).collect();
    let mut off = 0usize;
    while off < total {
        let end = (off + step).min(total);
        engine
            .write(node.inode, off as u64, &data2[off..end])
            .expect("overwrite chunk");
        off = end;
    }
    engine.flush(node.inode).expect("flush overwrite");
    engine.wait_for_uploads();

    assert_eq!(
        std::fs::read(fixture.join("big.bin")).unwrap(),
        data2,
        "the in-place overwrite did not replace the server content"
    );
    let copies = std::fs::read_dir(&fixture)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("conflicted copy"))
        .count();
    assert_eq!(
        copies, 0,
        "the overwrite spawned {copies} conflicted copies"
    );

    std::fs::remove_dir_all(&base).ok();
}
