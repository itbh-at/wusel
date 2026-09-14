// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end proof of the frontend boundary's **write path**: the real
//! `wusel-core` engine against an in-process `wusel-mock` server, driven over a
//! Unix-domain socket — create, write, publish, move and remove, with no FUSE
//! and no macOS in the loop.
//!
//! The read-path twin is `serve_ipc.rs`. Together they show that the socket
//! contract a Swift File Provider will speak is proven natively, both
//! directions.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wusel_core::config::Account;
use wusel_core::provider::Provider;
use wusel_core::state::StateDb;
use wusel_core::webdav::WebDavClient;
use wusel_ipc::{Client, Request, Response};

/// Point XDG at a throwaway location; sound because this binary has a single
/// `#[test]` and this is its first statement (see `serve_ipc.rs`).
fn xdg_sandbox(base: &Path) {
    let xdg = base.join("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.join("config"));
    std::env::set_var("XDG_STATE_HOME", xdg.join("state"));
    std::env::set_var("XDG_CACHE_HOME", xdg.join("cache"));
}

/// An in-process wusel-mock WebDAV server over `root` as user `alice`. The
/// returned runtime must outlive the server. Modelled on `serve_ipc.rs`.
fn serve_mock(root: &Path) -> (String, tokio::runtime::Runtime) {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
    let addr = std_listener
        .local_addr()
        .expect("mock listener addr")
        .to_string();
    std_listener.set_nonblocking(true).expect("set_nonblocking");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build mock runtime");
    let root = root.to_path_buf();
    rt.spawn(async move {
        let listener = tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
        let _ = wusel_mock::serve(listener, root, "alice").await;
    });
    (addr, rt)
}

/// Connect to the socket, retrying briefly while the server thread binds it.
fn connect(socket: &Path) -> Client {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Client::connect(socket) {
            Ok(client) => return client,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("could not connect to {}: {e}", socket.display()),
        }
    }
}

/// A one-shot request that expects no content frame back (everything but fetch).
fn one(client: &mut Client, request: Request) -> Response {
    client.call(&request).expect("request over the socket").0
}

#[test]
fn creates_writes_publishes_moves_and_removes_over_the_socket() {
    let base = std::env::temp_dir().join(format!("wusel-ipc-write-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    xdg_sandbox(&base);

    // The server starts empty: everything under test is created over the socket.
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();

    let (addr, _rt) = serve_mock(&fixture);

    // Build the engine exactly as the mount (and the read-path test) does.
    let account = Account::new("default");
    let dav = WebDavClient::new(
        reqwest::Client::new(),
        &format!("http://{addr}"),
        "alice",
        "pw",
    );
    std::fs::create_dir_all(account.state_db_path().parent().unwrap()).unwrap();
    let state = StateDb::open(&account.state_db_path()).unwrap();
    let mut provider = Provider::new(dav, state, &account).unwrap();
    let events = wusel_ipc::Events::start(provider.take_invalidations().unwrap());
    let driver = Arc::new(
        wusel_ipc::Driver::start(provider, wusel_core::runtime::Pools::default()).unwrap(),
    );

    let socket = std::env::temp_dir().join(format!("wusel-ipc-write-{}.sock", std::process::id()));
    let listener = wusel_ipc::bind(&socket).expect("bind the test socket");
    std::thread::spawn(move || {
        let _ = wusel_ipc::serve(driver, events, wusel_ipc::IpcDesktop::new(), listener);
    });

    let mut client = connect(&socket);
    let content = b"content over the socket\n";

    // create("/new.txt") — a fresh, still-local file (no server file id yet).
    match one(
        &mut client,
        Request {
            op: "create".into(),
            path: "/new.txt".into(),
            ..Default::default()
        },
    ) {
        Response::Node { ok, is_dir, .. } => {
            assert!(ok, "create succeeded");
            assert!(!is_dir, "new.txt is a file");
        }
        other => panic!("expected a node from create, got {other:?}"),
    }

    // write("/new.txt", content) — the buffer takes every byte; nothing is on
    // the server yet.
    match client
        .call_write(
            &Request {
                op: "write".into(),
                path: "/new.txt".into(),
                ..Default::default()
            },
            content,
        )
        .expect("write over the socket")
    {
        Response::Written { ok, len } => {
            assert!(ok, "write accepted");
            assert_eq!(len as usize, content.len(), "the buffer took every byte");
        }
        other => panic!("expected written, got {other:?}"),
    }

    // publish("/new.txt") — now it reaches the server. The mock mutates its
    // backing directory, so the bytes land on disk under the fixture.
    assert!(
        matches!(
            one(
                &mut client,
                Request {
                    op: "publish".into(),
                    path: "/new.txt".into(),
                    ..Default::default()
                },
            ),
            Response::Done { ok: true }
        ),
        "publish reported done"
    );
    assert_eq!(
        std::fs::read(fixture.join("new.txt")).expect("published file is on the server"),
        content,
        "the server has exactly what was written"
    );

    // fetch("/new.txt") — the round trip reads back the same bytes over the wire.
    let read_back = client
        .call(&Request {
            op: "fetch".into(),
            path: "/new.txt".into(),
            offset: 0,
            len: 1 << 20,
            ..Default::default()
        })
        .expect("fetch over the socket")
        .1
        .expect("fetch returned a content frame");
    assert_eq!(&read_back[..], content, "fetch reads what was published");

    // create("/folder", dir) — a directory this time.
    match one(
        &mut client,
        Request {
            op: "create".into(),
            path: "/folder".into(),
            dir: true,
            ..Default::default()
        },
    ) {
        Response::Node { ok, is_dir, .. } => {
            assert!(ok && is_dir, "folder is a created directory");
        }
        other => panic!("expected a directory node, got {other:?}"),
    }

    // move("/new.txt" -> "/folder/moved.txt") — a rename into the new directory.
    assert!(
        matches!(
            one(
                &mut client,
                Request {
                    op: "move".into(),
                    path: "/new.txt".into(),
                    to: "/folder/moved.txt".into(),
                    ..Default::default()
                },
            ),
            Response::Done { ok: true }
        ),
        "move reported done"
    );
    // The source is gone; the destination lists the moved child and reads back.
    assert!(
        matches!(
            one(
                &mut client,
                Request {
                    op: "stat".into(),
                    path: "/new.txt".into(),
                    ..Default::default()
                },
            ),
            Response::Error { .. }
        ),
        "the old path no longer resolves"
    );
    match one(
        &mut client,
        Request {
            op: "enumerate".into(),
            path: "/folder".into(),
            ..Default::default()
        },
    ) {
        Response::Entries { ok, entries } => {
            assert!(ok);
            let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
            assert!(
                names.contains(&"moved.txt"),
                "folder lists moved.txt: {names:?}"
            );
        }
        other => panic!("expected entries, got {other:?}"),
    }

    // remove("/folder/moved.txt") — the file is deleted and stops resolving.
    assert!(
        matches!(
            one(
                &mut client,
                Request {
                    op: "remove".into(),
                    path: "/folder/moved.txt".into(),
                    ..Default::default()
                },
            ),
            Response::Done { ok: true }
        ),
        "remove reported done"
    );
    assert!(
        matches!(
            one(
                &mut client,
                Request {
                    op: "stat".into(),
                    path: "/folder/moved.txt".into(),
                    ..Default::default()
                },
            ),
            Response::Error { .. }
        ),
        "the removed path no longer resolves"
    );

    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&base);
}
