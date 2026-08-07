// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! How GNOME saves a file, done through a real mount.
//!
//! GNOME Text Editor — and everything else built on `g_file_replace` — does not
//! write into the file you opened. It writes a sibling called
//! `.goutputstream-XXXXXX`, closes it, and renames it over the original. That
//! name is in our ignore list, because while it exists it is a temporary nobody
//! wants on the server.
//!
//! The rename is the moment the temporary stops being one. If the ignore
//! decision outlives it, the bytes the user typed never reach the server — and
//! the editor is told the save failed, which is how this was found: "Ein-/
//! Ausgabefehler" on Ctrl+S.
//!
//! Linux only, and needs `/dev/fuse` — it runs in the container.

mod common;

use std::io::Write;

#[test]
fn a_gnome_atomic_save_reaches_the_server() {
    let m = common::MountFixture::start("atomicsave");

    let target = m.mnt.join("Notes.txt");
    let temp = m.mnt.join(".goutputstream-ABC123");
    let content = b"saved by the editor\n";

    // 1. The editor writes its temporary sibling and closes it.
    {
        let mut f = std::fs::File::create(&temp).expect("create the save temporary");
        f.write_all(content).expect("write the new contents");
        f.sync_all().ok(); // gvfs does this; it must not be the thing that fails
    }

    // 2. …then renames it over the file being saved. This is the call the
    //    editor reports as the save, so an error here is the error the user sees.
    std::fs::rename(&temp, &target).expect("the atomic save's rename must succeed");

    // 3. The server gets the bytes, which is the whole point of saving. Checked
    //    before the mount deliberately: if this holds and the next one does not,
    //    the upload works and the kernel is serving a cached lookup — a
    //    different bug with a different fix. Give it a moment; the rename
    //    returns as soon as the name is right.
    let on_server = m.fixture.join("Notes.txt");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if std::fs::read(&on_server).unwrap_or_default() == content {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the saved contents never reached the server; it still has {:?}. \
             The server side holds: {:?}",
            String::from_utf8_lossy(&std::fs::read(&on_server).unwrap_or_default()),
            std::fs::read_dir(&m.fixture)
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    // 4. …and the mount serves them too, without a remount.
    assert_eq!(
        std::fs::read(&target).expect("read the saved file back"),
        content,
        "the mount serves what was saved"
    );
}
