// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The status socket, served **beside a live FUSE mount, off the same engine**.
//!
//! This is the gate on the arrangement the file-manager integration needs. The
//! mount and the socket used to be alternatives — each started its own
//! substrate, and two substrates over one state database is not a thing you may
//! do — so on Linux, where the unit runs `wusel mount`, the socket a file
//! manager would query simply did not exist.
//!
//! What is proved here: the mount comes up and serves normally; the socket
//! answers `stat` and `enumerate` for the same objects at the same time; and the
//! two frontends do not steal each other's answers — the failure that a shared
//! request-id allocator exists to make impossible, and the one that would show
//! up in the field as a file manager's status query returning somebody else's
//! file.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use wusel_ipc::{Client, Request, Response};

/// One request, one response, over a fresh connection — driven through the
/// crate's own client, so this test exercises the framing a real frontend uses
/// rather than a hand-rolled copy of it.
fn ask(socket: &Path, request: &Request) -> Response {
    let (response, _) = connect(socket).call(request).expect("call over the socket");
    response
}

/// Connect, retrying while the mount's serve thread binds the socket. The mount
/// serves before the socket does — the socket is started from inside the mount —
/// so a test that raced this would be flaky rather than wrong.
fn connect(socket: &Path) -> Client {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Client::connect(socket) {
            Ok(c) => return c,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("the mount never served {}: {e}", socket.display()),
        }
    }
}

#[test]
fn the_mount_serves_the_status_socket_off_its_own_engine() {
    // The socket goes beside the mount, in the fixture's own directory: a
    // shared location would collide with a concurrent test binary, and the
    // serve path refuses a directory that is not private to us anyway.
    let mut socket_path = None;
    let fx = common::MountFixture::start_with("ipc-beside-mount", |base| {
        let path = base.join("status.sock");
        socket_path = Some(path.clone());
        let (inval_tx, inval_rx) = std::sync::mpsc::channel();
        let events = wusel_ipc::Events::start(inval_rx);
        let notices = wusel_ipc::IpcDesktop::new();
        wusel_fuse::Extras {
            invalidations: Some(inval_tx),
            on_ready: Some(Box::new(move |ids, provider| {
                let driver = std::sync::Arc::new(wusel_ipc::Driver::attach(ids, provider));
                let route = driver.route();
                std::thread::spawn(move || {
                    let _ = wusel_ipc::serve(driver, events, notices, &path);
                });
                Some(route)
            })),
        }
    });
    let socket = socket_path.expect("the extras hook ran");

    // The mount itself must be unaffected — it is the product; the socket is
    // decoration on top of it.
    let listed = std::fs::read_to_string(fx.mnt.join("Notes.txt")).expect("read through the mount");
    assert_eq!(listed, "hello");

    // The socket answers for the same object, at the same time, from the same
    // engine.
    let response = ask(
        &socket,
        &Request {
            op: "stat".into(),
            path: "/Notes.txt".into(),
            ..Request::default()
        },
    );
    match response {
        Response::Node {
            name, size, is_dir, ..
        } => {
            assert_eq!(name, "Notes.txt");
            assert_eq!(size, 5);
            assert!(!is_dir);
        }
        other => panic!("expected a node for /Notes.txt, got {other:?}"),
    }

    // A listing, so the directory path is covered too.
    let response = ask(
        &socket,
        &Request {
            op: "enumerate".into(),
            path: "/".into(),
            ..Request::default()
        },
    );
    match response {
        Response::Entries { entries, .. } => {
            assert!(
                entries.iter().any(|e| e.name == "Notes.txt"),
                "the root listing is missing Notes.txt: {entries:?}"
            );
            assert!(
                entries.iter().any(|e| e.name == "Sub Folder" && e.is_dir),
                "the root listing is missing Sub Folder: {entries:?}"
            );
        }
        other => panic!("expected entries for /, got {other:?}"),
    }

    // The socket must be owner-only however the umask stood when it was bound.
    let mode = std::fs::metadata(&socket)
        .expect("stat the socket")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the status socket is not private: {mode:o}");

    // Interleave the two frontends and check neither takes the other's answer.
    // Both mint request ids from the one allocator; with two allocators an id
    // would be handed out twice and whichever side held it first would win.
    for _ in 0..25 {
        let through_mount =
            std::fs::read_to_string(fx.mnt.join("Sub Folder/deep.txt")).expect("read");
        assert_eq!(through_mount, "nested");
        let response = ask(
            &socket,
            &Request {
                op: "stat".into(),
                path: "/Sub Folder/deep.txt".into(),
                ..Request::default()
            },
        );
        match response {
            Response::Node { name, size, .. } => {
                assert_eq!(name, "deep.txt");
                assert_eq!(size, 6);
            }
            other => panic!("the socket lost an answer to the mount: {other:?}"),
        }
    }
}
