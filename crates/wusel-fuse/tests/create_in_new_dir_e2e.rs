// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Reproduction: creating a file inside a freshly made directory.
//!
//! Reported from a real desktop: `mkdir ~/Wusel/ztest` succeeds, but
//! `echo hallo > ~/Wusel/ztest/a.txt` fails with EIO. Reading is fine; writing a
//! new file into a just-created directory is not.
//!
//! Linux only, and needs `/dev/fuse` — it runs in the container.

mod common;

use std::io::Write;

#[test]
fn a_file_created_in_a_fresh_directory_saves() {
    let m = common::MountFixture::start("createnewdir");

    // Load the root once, as a file manager or shell would.
    let _ = std::fs::read_dir(&m.mnt).unwrap().count();

    // mkdir ztest
    let dir = m.mnt.join("ztest");
    std::fs::create_dir(&dir).expect("mkdir in the mount must succeed");

    // echo hallo > ztest/a.txt
    let file = dir.join("a.txt");
    {
        let mut f = std::fs::File::create(&file).expect("create a file in the fresh directory");
        f.write_all(b"hallo\n").expect("write into it");
    }

    // Read it back through the mount.
    assert_eq!(
        std::fs::read(&file).expect("read the new file back"),
        b"hallo\n",
        "the file created in a fresh directory must be readable"
    );

    // And it reaches the server.
    let on_server = m.fixture.join("ztest").join("a.txt");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::fs::read(&on_server).unwrap_or_default() != b"hallo\n" {
        assert!(
            std::time::Instant::now() < deadline,
            "the new file never reached the server"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
