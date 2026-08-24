// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A server that goes away must *say* so — once — and say when it is back.
//!
//! This is the failure that makes the mount look broken rather than offline: a
//! file manager stops drawing the folder, an application stops opening its
//! document, and the only trace is a `WARN` in a journal nobody reads. Users
//! then diagnose it as a hang and start killing things. The engine has known all
//! along; this test holds it to telling.
//!
//! It exercises the wiring the unit tests in `wusel_core::health` cannot: that
//! the *real* request paths report their outcome, that a whole outage produces
//! exactly one notification however many requests fail inside it, and that the
//! recovery is announced when the server comes back at the same address.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wusel_core::desktop::{Desktop, Notice, Status};
use wusel_core::health::Reachability;
use wusel_core::state::ROOT_INODE;

#[derive(Default)]
struct Recorder {
    notices: Mutex<Vec<Notice>>,
}

impl Desktop for Recorder {
    fn notify(&self, n: &Notice) {
        self.notices.lock().unwrap().push(n.clone());
    }
    fn set_status(&self, _s: Status) {}
}

impl Recorder {
    /// How often the user has been told about the connection, either way.
    fn count(&self, want: &Notice) -> usize {
        self.notices
            .lock()
            .unwrap()
            .iter()
            .filter(|n| *n == want)
            .count()
    }
}

/// Drive listings until `want` has been notified, or give up. Returns whether it
/// arrived. Listing is what a file manager does, and with a zero revalidation
/// TTL every call really goes to the server.
fn drive_until(engine: &mut common::Engine, recorder: &Recorder, want: &Notice) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let _ = engine.list_dir(ROOT_INODE);
        if recorder.count(want) > 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn an_unreachable_server_is_announced_once_and_its_return_too() {
    let base = std::env::temp_dir().join(format!("wusel-mock-conn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("note.txt"), b"hello").unwrap();

    common::xdg_sandbox(&base);
    // TTL 0: every listing is a real request, so the test can provoke failures
    // the way a user browsing a folder does.
    std::env::set_var("WUSEL_REVALIDATE_SECS", "0");

    let mock = common::Mock::serve(&fixture);
    let addr = mock.addr.clone();

    let recorder = Arc::new(Recorder::default());
    // No confirmation delay: the *policy* (a blip is not an outage) is the unit
    // tests' subject. Here the subject is the wiring, and waiting out the real
    // ten seconds would only make the suite slower.
    let health = Arc::new(Reachability::with_confirm_after(
        &format!("http://{addr}"),
        recorder.clone(),
        Duration::ZERO,
    ));
    let mut engine =
        common::Engine::start_with_health(&addr, Some(recorder.clone()), Some(health.clone()));

    // A working mount says nothing at all — the bar for a notification is high.
    assert!(!engine.list_dir(ROOT_INODE).is_empty(), "the fixture lists");
    assert!(
        recorder.notices.lock().unwrap().is_empty(),
        "a healthy connection is not news: {:?}",
        recorder.notices.lock().unwrap()
    );

    // The server goes away — the situation the user reads as "everything hangs".
    drop(mock);

    // The message names host and port — what the user sees in their browser's
    // address bar, not the internal base URL.
    let lost = Notice::ConnectionLost {
        server: addr.clone(),
    };
    assert!(
        drive_until(&mut engine, &recorder, &lost),
        "an unreachable server was never reported to the user: {:?}",
        recorder.notices.lock().unwrap()
    );
    assert!(health.is_down(), "the engine knows it is offline");
    // Every failing request after the first reports too; the user hears it once.
    for _ in 0..10 {
        let _ = engine.list_dir(ROOT_INODE);
    }
    assert_eq!(
        recorder.count(&lost),
        1,
        "one notification per outage, not per request: {:?}",
        recorder.notices.lock().unwrap()
    );

    // And back — at the same address, which is what "the connection returned"
    // means to an engine that has been pointed at it since start-up.
    let _mock = common::Mock::serve_on(&fixture, &addr);
    let restored = Notice::ConnectionRestored {
        server: addr.clone(),
    };
    assert!(
        drive_until(&mut engine, &recorder, &restored),
        "the recovery was never announced: {:?}",
        recorder.notices.lock().unwrap()
    );
    assert_eq!(
        recorder.count(&restored),
        1,
        "the good news is told once: {:?}",
        recorder.notices.lock().unwrap()
    );
    assert!(!health.is_down());

    drop(engine);
    std::env::remove_var("WUSEL_REVALIDATE_SECS");
    std::fs::remove_dir_all(&base).ok();
}
