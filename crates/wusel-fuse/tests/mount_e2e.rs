// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Real FUSE mount end-to-end: mount `wusel` (engine + FUSE) against the
//! in-process `wusel-mock` WebDAV server and drive it through the kernel — `ls`,
//! `cat`, `stat`, `statfs`, plus writing (create, overwrite, mkdir, rename,
//! unlink) — then unmount. Needs `/dev/fuse`, so it runs only on Linux (the
//! podman container); it is a no-op elsewhere.
#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::path::Path;
use std::time::Duration;

/// Poll until the mount serves the expected root entry, or give up.
fn wait_until_mounted(mnt: &Path) -> bool {
    for _ in 0..100 {
        if let Ok(rd) = std::fs::read_dir(mnt) {
            if rd
                .flatten()
                .any(|e| e.file_name() == std::ffi::OsStr::new("Notes.txt"))
            {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn mount_lists_reads_and_reports_statfs() {
    let base = std::env::temp_dir().join(format!("wusel-fuse-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(fixture.join("Sub Folder")).unwrap();
    std::fs::write(fixture.join("Notes.txt"), b"hello").unwrap();
    std::fs::write(fixture.join("Sub Folder/deep.txt"), b"nested").unwrap();

    // Start the mock server in-process; report its port back over a channel.
    let (tx, rx) = std::sync::mpsc::channel();
    let fixture_for_server = fixture.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap().port()).unwrap();
            let _ = wusel_mock::serve(listener, fixture_for_server, "alice").await;
        });
    });
    let port = rx.recv().unwrap();

    // Point the account's XDG dirs at throwaway locations, then build a Provider.
    let xdg = base.join("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.join("config"));
    std::env::set_var("XDG_STATE_HOME", xdg.join("state"));
    std::env::set_var("XDG_CACHE_HOME", xdg.join("cache"));
    let account = wusel_core::config::Account::new("default");
    let dav = wusel_core::webdav::WebDavClient::new(
        reqwest::Client::new(),
        &format!("http://127.0.0.1:{port}"),
        "alice",
        "pw",
    );
    std::fs::create_dir_all(account.state_db_path().parent().unwrap()).unwrap();
    let state = wusel_core::state::StateDb::open(&account.state_db_path()).unwrap();
    let provider = wusel_core::provider::Provider::new(dav, state, &account).unwrap();

    // Mount on a background thread (the mount call blocks until unmounted).
    let mnt = base.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let mnt_for_thread = mnt.clone();
    let mount_thread = std::thread::spawn(move || {
        let _ = wusel_fuse::mount(&mnt_for_thread, provider);
    });

    assert!(wait_until_mounted(&mnt), "mount did not become ready");

    // ls: the tree is visible.
    let names: Vec<_> = std::fs::read_dir(&mnt)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.contains(&"Notes.txt".to_string()),
        "root lists Notes.txt"
    );
    assert!(
        names.contains(&"Sub Folder".to_string()),
        "root lists the subdir"
    );

    // stat + cat: metadata and content come through the kernel.
    assert_eq!(std::fs::metadata(mnt.join("Notes.txt")).unwrap().len(), 5);
    assert_eq!(std::fs::read(mnt.join("Notes.txt")).unwrap(), b"hello");
    assert_eq!(
        std::fs::read(mnt.join("Sub Folder/deep.txt")).unwrap(),
        b"nested"
    );

    // statfs: df must see a non-empty, non-full filesystem (so apps do not balk).
    let cpath = CString::new(mnt.to_str().unwrap()).unwrap();
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut st) };
    assert_eq!(rc, 0, "statvfs syscall failed");
    assert!(st.f_blocks > 0, "statfs reports zero total blocks");
    assert!(st.f_bavail > 0, "statfs reports no free space");

    // --- writing through the kernel ---

    // Create a new file: the write reaches the server and reads back.
    std::fs::write(mnt.join("new.txt"), b"created").unwrap();
    assert_eq!(std::fs::read(fixture.join("new.txt")).unwrap(), b"created");
    assert_eq!(std::fs::read(mnt.join("new.txt")).unwrap(), b"created");

    // Overwrite an existing file (open O_TRUNC → write → flush).
    std::fs::write(mnt.join("Notes.txt"), b"OVERWRITTEN").unwrap();
    assert_eq!(
        std::fs::read(fixture.join("Notes.txt")).unwrap(),
        b"OVERWRITTEN"
    );

    // mkdir, rename, unlink.
    std::fs::create_dir(mnt.join("NewDir")).unwrap();
    assert!(fixture.join("NewDir").is_dir());
    std::fs::rename(mnt.join("new.txt"), mnt.join("renamed.txt")).unwrap();
    assert!(fixture.join("renamed.txt").is_file() && !fixture.join("new.txt").exists());
    std::fs::remove_file(mnt.join("renamed.txt")).unwrap();
    assert!(!fixture.join("renamed.txt").exists());

    // Unmount → the mount thread's blocking call returns.
    let _ = std::process::Command::new("fusermount3")
        .args(["-u", mnt.to_str().unwrap()])
        .status();
    let _ = mount_thread.join();
    std::fs::remove_dir_all(&base).ok();
}
