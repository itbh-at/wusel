// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end for chunked upload NG: a file larger than one chunk is uploaded
//! in parts (MKCOL + PUT chunks + MOVE assemble) and reassembled byte-for-byte
//! on the server side.

mod common;

use wusel_core::state::ROOT_INODE;

#[test]
fn large_file_uploads_in_chunks_and_reassembles() {
    let base = std::env::temp_dir().join(format!("wusel-mock-chunk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();

    common::xdg_sandbox(&base);

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let engine = common::Engine::start(&addr);

    // ~9 MiB — larger than one 4 MiB chunk, so the upload is chunked.
    let data: Vec<u8> = (0..9 * 1024 * 1024).map(|i| (i % 251) as u8).collect();

    let node = engine.create(ROOT_INODE, "big.bin").expect("create");
    engine.write(node.inode, 0, &data).expect("write");
    engine.flush(node.inode).expect("flush");
    // The upload is asynchronous: flush returns once the change is durable
    // locally, and the transfer runs on behind it. Wait for it to land.
    engine.wait_for_uploads();

    // The server reassembled the file byte-for-byte.
    assert_eq!(std::fs::read(fixture.join("big.bin")).unwrap(), data);

    std::fs::remove_dir_all(&base).ok();
}
