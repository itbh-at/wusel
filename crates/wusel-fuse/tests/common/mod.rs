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
///
/// `dead_code` is allowed because a shared test module is compiled *into each*
/// test binary separately, so anything one binary does not touch looks unused
/// to it. Splitting the harness per test would be the alternative, and a worse
/// one.
#[allow(dead_code)]
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
        // A freedesktop wastebasket left on the server by another client. The
        // mount must keep it hidden — never list it, never resolve it — so every
        // test implicitly exercises that, and `trash_e2e` asserts it explicitly.
        std::fs::create_dir_all(fixture.join(".Trash-1000/files")).unwrap();
        std::fs::write(fixture.join(".Trash-1000/files/old.txt"), b"gone").unwrap();

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
        // Run the mount multi-threaded so these end-to-end tests exercise the
        // concurrent dispatch path (Etappe 6): several FUSE callbacks in flight at
        // once, which is what would expose an unguarded bit of frontend state
        // (a directory-stream snapshot, a flush's per-inode entry). A race there
        // shows up as a flaky rewinddir/dir_streams run.
        let cfg_dir = xdg.join("config").join("wusel");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        // A test that wants a short revalidation interval sets it before
        // starting; the default keeps the production value out of the way.
        let sync = std::env::var("WUSEL_TEST_REVALIDATE_SECS")
            .map(|v| format!("\n[sync]\nrevalidate_secs = {v}\n"))
            .unwrap_or_default();
        let threads = std::env::var("WUSEL_TEST_DISPATCH_THREADS").unwrap_or_else(|_| "4".into());
        std::fs::write(
            cfg_dir.join("config.toml"),
            format!("[mount]\ndispatch_threads = {threads}\n{sync}"),
        )
        .unwrap();
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

/// Poll until `cond` holds, or fail after ~10 s. Uploads are asynchronous now —
/// a file's bytes reach the server shortly after the kernel write returns — so a
/// test that inspects the *server* must wait for the effect to land. (Directory
/// creates, renames and unlinks stay synchronous and need no wait.)
#[allow(dead_code)]
pub fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "the server-side effect never landed: {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
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
