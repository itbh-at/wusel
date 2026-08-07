// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! `open_pinned = "newest-unmetered"` on a metered connection serves the local
//! copy — the whole chain, from the desktop's metering answer through the cache
//! into the read path.
//!
//! The pure decision is unit-tested in `config`; this proves the wiring: that a
//! `Some(true)` from the desktop backend really reaches `stale_copy_ok` and
//! turns an open into a local read rather than a fetch. The unmetered direction
//! is the same path as the default `newest`, covered there.

mod common;

use std::sync::{Arc, Mutex};
use wusel_core::desktop::{Desktop, Notice, Stale};

/// A desktop that reports a metered connection and records stale-copy notices.
#[derive(Default)]
struct Metered(Mutex<Vec<String>>);

impl Desktop for Metered {
    fn set_status(&self, _s: wusel_core::desktop::Status) {}
    fn notify(&self, notice: &Notice) {
        if let Notice::StaleCopyServed { path, reason } = notice {
            assert_eq!(
                *reason,
                Stale::ByChoice,
                "served by policy, not by a dead server"
            );
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(path.clone());
        }
    }
    fn is_metered(&self) -> Option<bool> {
        Some(true) // a phone hotspot
    }
}

#[test]
fn a_metered_connection_opens_the_local_copy() {
    let base = std::env::temp_dir().join(format!("wusel-mock-openmet-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("Notes.txt"), b"the offline version").unwrap();
    common::xdg_sandbox(&base);

    let config_dir = wusel_core::config::Account::new("default").config_dir();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[sync]\nopen_pinned = \"newest-unmetered\"\n",
    )
    .unwrap();

    let mock = common::Mock::serve(&fixture);
    let desktop = Arc::new(Metered::default());
    let mut engine = common::Engine::start_with(&mock.addr, Some(desktop.clone()));

    engine.pin("Notes.txt").expect("pin the file");
    let node = engine.resolve("Notes.txt").unwrap().unwrap();

    std::fs::write(
        fixture.join("Notes.txt"),
        b"a much newer version from the web",
    )
    .unwrap();
    engine.wait_until_stale("Notes.txt", &node.etag);

    let served = engine.read(node.inode, 0, 64).expect("read the file");
    assert_eq!(
        String::from_utf8_lossy(&served),
        "the offline version",
        "a metered connection serves the copy that is already paid for"
    );
    assert_eq!(
        desktop.0.lock().unwrap().clone(),
        vec!["Notes.txt".to_string()],
        "and says so, once"
    );
    assert_eq!(
        engine.write(node.inode, 0, b"an edit"),
        Err(wusel_fsm::Outcome::Failed(wusel_fsm::Failure::NotWritable)),
        "an outdated copy served over a metered link is read-only, like any other"
    );

    std::fs::remove_dir_all(&base).ok();
}
