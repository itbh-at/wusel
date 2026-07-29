// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Shared harness for the FUSE end-to-end tests: a fixture tree served by the
//! in-process `wusel-mock` WebDAV server, mounted for real through the kernel.
//!
//! Each test binary gets its own process, which matters: the harness points the
//! account's XDG dirs at a throwaway location via `std::env::set_var`, and that
//! is process-global. One mount per test binary keeps that safe.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// A live mount plus the fixture directory behind it. Unmounts and cleans up
/// on drop, so a failing assertion cannot leave a dangling mount behind.
pub struct MountFixture {
    base: PathBuf,
    /// The server-side tree the mock serves — write here to simulate the server.
    pub fixture: PathBuf,
    /// The mountpoint the test drives through the kernel.
    pub mnt: PathBuf,
    mount_thread: Option<std::thread::JoinHandle<()>>,
}

impl MountFixture {
    /// Build the fixture, start the mock server, mount, and wait until the
    /// mount serves. `tag` keeps concurrent test binaries in separate dirs.
    pub fn start(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("wusel-fuse-{tag}-{}", std::process::id()));
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
        Self {
            base,
            fixture,
            mnt,
            mount_thread: Some(mount_thread),
        }
    }
}

impl Drop for MountFixture {
    fn drop(&mut self) {
        // Unmount → the mount thread's blocking call returns, and its `Session`
        // drop tidies up the mountpoint.
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", self.mnt.to_str().unwrap()])
            .status();
        if let Some(t) = self.mount_thread.take() {
            let _ = t.join();
        }
        std::fs::remove_dir_all(&self.base).ok();
    }
}

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
