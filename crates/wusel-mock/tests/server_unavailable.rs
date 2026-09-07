// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A server that answers but cannot serve must say so too.
//!
//! The sibling of `connection_lost`, and the case that went silent for far
//! longer: during a backup window the reverse proxy answers every request with
//! `502`, so the mount is exactly as unusable as an unplugged cable — but the
//! server *is* there, and "an answer means reachable" made every reporting path
//! call it healthy. The journal filled with 502s and the user was told nothing.
//!
//! What the unit tests in `wusel_core::health` cannot cover, and this does: that
//! the real request paths pass a `5xx` on to the tracker at all.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
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
    fn count(&self, want: &Notice) -> usize {
        self.notices
            .lock()
            .unwrap()
            .iter()
            .filter(|n| *n == want)
            .count()
    }
}

/// A server in maintenance: it accepts the connection, reads the whole request,
/// and answers `502` — the shape a reverse proxy takes while what is behind it
/// is down.
///
/// The request is read to its end on purpose. Closing early would look like a
/// dropped connection, which is the *other* failure entirely — and the one this
/// test must not accidentally be exercising.
struct Refusing {
    addr: String,
    stop: Arc<AtomicBool>,
}

impl Refusing {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                if let Ok(stream) = stream {
                    std::thread::spawn(move || refuse(stream));
                }
            }
        });
        Self { addr, stop }
    }
}

impl Drop for Refusing {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Unblock the accept loop so the thread can notice and end.
        let _ = TcpStream::connect(&self.addr);
    }
}

fn refuse(stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return; // client went away
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
        if line == "\r\n" || line == "\n" {
            break; // end of headers
        }
    }
    let mut body = vec![0u8; len];
    let _ = reader.read_exact(&mut body);
    let mut stream = stream;
    let _ = stream
        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = stream.flush();
}

/// Drive listings until `want` is notified, or give up.
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
fn a_server_that_refuses_every_request_is_announced_once_and_its_return_too() {
    let base = std::env::temp_dir().join(format!("wusel-mock-refuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("note.txt"), b"hello").unwrap();

    common::xdg_sandbox(&base);
    // TTL 0: every listing is a real request, the way a user browsing is.
    std::env::set_var("WUSEL_REVALIDATE_SECS", "0");

    let refusing = Refusing::start();
    let addr = refusing.addr.clone();

    let recorder = Arc::new(Recorder::default());
    // No confirmation delay: the policy is the unit tests' subject, the wiring
    // is this one's.
    let health = Arc::new(Reachability::with_confirm_after(
        &format!("http://{addr}"),
        recorder.clone(),
        Duration::ZERO,
    ));
    let mut engine =
        common::Engine::start_with_health(&addr, Some(recorder.clone()), Some(health.clone()));

    let unavailable = Notice::ServerUnavailable {
        server: addr.clone(),
        status: 502,
    };
    assert!(
        drive_until(&mut engine, &recorder, &unavailable),
        "a server refusing every request was never reported: {:?}",
        recorder.notices.lock().unwrap()
    );
    assert_eq!(
        recorder.count(&Notice::ConnectionLost {
            server: addr.clone()
        }),
        0,
        "it answered, so nobody may be sent to check their network"
    );

    for _ in 0..10 {
        let _ = engine.list_dir(ROOT_INODE);
    }
    assert_eq!(
        recorder.count(&unavailable),
        1,
        "one notification per outage, not per request: {:?}",
        recorder.notices.lock().unwrap()
    );

    // Maintenance ends: the same address, now serving properly.
    drop(refusing);
    let _mock = common::Mock::serve_on(&fixture, &addr);
    let restored = Notice::ConnectionRestored {
        server: addr.clone(),
    };
    assert!(
        drive_until(&mut engine, &recorder, &restored),
        "the recovery was never announced: {:?}",
        recorder.notices.lock().unwrap()
    );
    assert!(!health.is_down());

    drop(engine);
    std::env::remove_var("WUSEL_REVALIDATE_SECS");
    std::fs::remove_dir_all(&base).ok();
}
