// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Reproduction: navigating back up to a directory must not hang.
//!
//! Reported from a real Fedora/GNOME desktop: `ls` works, the first listing
//! works, going into a subdirectory works — but navigating back up to the
//! parent freezes Nautilus hard enough that it offers to force-quit. Terminal
//! `ls` is unaffected.
//!
//! What is special about navigating *up*: the parent's listing is already
//! loaded and, after time in the subdirectory, stale. Serving it locally now
//! schedules a background refresh — and a file manager, unlike `ls`, floods the
//! directory with per-entry `getattr`/`getxattr` (for emblems) at the same
//! moment. So the reproduction is concurrency: hammer a loaded, stale directory
//! with listings and per-entry stats from several threads at once.
//!
//! The test guards itself with a deadline on a worker thread; a hang shows up as
//! that deadline passing, not as the whole suite wedging.
//!
//! Linux only, and needs `/dev/fuse` — it runs in the container.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn navigating_a_loaded_stale_directory_under_load_does_not_hang() {
    // Short revalidation, so every listing after the first finds the directory
    // stale and schedules the refresh that only the navigate-up path triggers.
    std::env::set_var("WUSEL_TEST_REVALIDATE_SECS", "1");
    // The default in production is a single dispatch thread — a current-thread
    // tokio runtime. That is the configuration the hang was reported on, and the
    // one the harness usually hides by running with four.
    std::env::set_var("WUSEL_TEST_DISPATCH_THREADS", "1");
    let m = common::MountFixture::start("navup");
    let mnt = m.mnt.clone();

    // Prime both directories, as opening them once in the file manager would.
    let _ = std::fs::read_dir(&mnt).unwrap().count();
    let _ = std::fs::read_dir(mnt.join("Sub Folder")).unwrap().count();
    std::thread::sleep(Duration::from_millis(1200)); // let it go stale

    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));

    // The server side keeps changing, so each stale re-list actually reconciles
    // a diff and fires `notify_inval_entry` on the root — the notification that
    // can deadlock with a concurrent lookup the kernel is holding the root's
    // lock for. Without a diff there is no notification and no collision.
    let fixture = m.fixture.clone();
    let mut_stop = Arc::clone(&stop);
    let mutator = std::thread::spawn(move || {
        let mut i = 0u64;
        while !mut_stop.load(Ordering::Relaxed) {
            let f = fixture.join(format!("churn-{}.txt", i % 3));
            if f.exists() {
                let _ = std::fs::remove_file(&f);
            } else {
                let _ = std::fs::write(&f, b"x");
            }
            i += 1;
            std::thread::sleep(Duration::from_millis(80));
        }
    });

    // The workload: what a file manager does navigating up and drawing emblems —
    // list the directory, then stat every entry — from several threads at once.
    let mut workers = Vec::new();
    for _ in 0..4 {
        let mnt = mnt.clone();
        let stop = Arc::clone(&stop);
        workers.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                for dir in [mnt.clone(), mnt.join("Sub Folder")] {
                    let Ok(rd) = std::fs::read_dir(&dir) else {
                        continue;
                    };
                    for entry in rd.flatten() {
                        // getattr + the emblem xattr, per entry — the flood.
                        let _ = std::fs::symlink_metadata(entry.path());
                        let mut buf = [0u8; 64];
                        unsafe {
                            let c = std::ffi::CString::new(
                                entry.path().as_os_str().to_string_lossy().as_bytes(),
                            )
                            .unwrap();
                            let name = std::ffi::CString::new("user.wusel.state").unwrap();
                            libc::getxattr(
                                c.as_ptr(),
                                name.as_ptr(),
                                buf.as_mut_ptr().cast(),
                                buf.len(),
                            );
                        }
                    }
                }
            }
        }));
    }

    // A watchdog: if the workers are still going in 30 s, they are wedged.
    let watch_done = Arc::clone(&done);
    let watchdog = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        // Let the load run for a while, then ask the workers to stop and see if
        // they actually can within the deadline.
        std::thread::sleep(Duration::from_secs(6));
        stop.store(true, Ordering::Relaxed);
        while Instant::now() < deadline {
            if watch_done.load(Ordering::Relaxed) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    });

    for w in workers {
        w.join().unwrap();
    }
    mutator.join().unwrap();
    done.store(true, Ordering::Relaxed);

    assert!(
        watchdog.join().unwrap(),
        "navigating a loaded, stale directory under load wedged the mount — \
         the workers did not finish within the deadline"
    );
}
