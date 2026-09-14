// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Pinning over the socket: a `pin` op keeps a file offline and `stat` then
//! reports it pinned, `unpin` reverses it. This is what a Finder "make available
//! offline" action drives.
//!
//! Own test binary — it mutates the process-global XDG environment, so it must
//! not share a binary with another test (see `serve_watch.rs`).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wusel_core::config::Account;
use wusel_core::provider::Provider;
use wusel_core::state::StateDb;
use wusel_core::webdav::WebDavClient;
use wusel_ipc::{Client, Driver, Events, IpcDesktop, Request, Response};

fn xdg_sandbox(base: &Path) {
    let xdg = base.join("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.join("config"));
    std::env::set_var("XDG_STATE_HOME", xdg.join("state"));
    std::env::set_var("XDG_CACHE_HOME", xdg.join("cache"));
}

/// An in-process wusel-mock WebDAV server; keep the returned runtime alive.
fn serve_mock(root: &Path) -> (String, tokio::runtime::Runtime) {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
    let addr = std_listener.local_addr().unwrap().to_string();
    std_listener.set_nonblocking(true).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let root = root.to_path_buf();
    rt.spawn(async move {
        let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
        let _ = wusel_mock::serve(listener, root, "alice").await;
    });
    (addr, rt)
}

/// The `pinned` flag on a `stat` of `path`.
fn stat_pinned(client: &mut Client, path: &str) -> bool {
    let (response, _) = client
        .call(&Request {
            op: "stat".into(),
            path: path.into(),
            ..Default::default()
        })
        .expect("stat call");
    match response {
        Response::Node { pinned, .. } => pinned,
        other => panic!("expected a node for {path}, got {other:?}"),
    }
}

#[test]
fn pin_and_unpin_over_the_socket() {
    let base = std::env::temp_dir().join(format!("wusel-ipc-pin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    xdg_sandbox(&base);

    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("Notes.txt"), b"hello").unwrap();
    let (addr, _rt) = serve_mock(&fixture);

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
    let invalidations = provider.take_invalidations().expect("invalidation channel");

    let events = Events::start(invalidations);
    let desktop = IpcDesktop::new();
    let driver = Arc::new(Driver::start(provider, wusel_core::runtime::Pools::default()).unwrap());

    let socket = std::env::temp_dir().join(format!("wusel-ipc-pin-{}.sock", std::process::id()));
    let listener = wusel_ipc::bind(&socket).expect("bind the test socket");
    let driver_for_thread = Arc::clone(&driver);
    std::thread::spawn(move || {
        let _ = wusel_ipc::serve(driver_for_thread, events, desktop, listener);
    });
    let up = Instant::now();
    while !socket.exists() {
        assert!(up.elapsed() < Duration::from_secs(5), "serve never bound");
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut client = Client::connect(&socket).expect("connect");

    // Not pinned to begin with.
    assert!(
        !stat_pinned(&mut client, "/Notes.txt"),
        "clean start: not pinned"
    );

    // Pin it: the op succeeds and keeps it offline.
    let (pinned, _) = client
        .call(&Request {
            op: "pin".into(),
            path: "/Notes.txt".into(),
            ..Default::default()
        })
        .expect("pin call");
    assert!(
        matches!(pinned, Response::Done { .. }),
        "pin -> done, got {pinned:?}"
    );
    assert!(
        stat_pinned(&mut client, "/Notes.txt"),
        "stat reports it pinned after pin"
    );

    // Unpin it: the flag clears.
    let (unpinned, _) = client
        .call(&Request {
            op: "unpin".into(),
            path: "/Notes.txt".into(),
            ..Default::default()
        })
        .expect("unpin call");
    assert!(
        matches!(unpinned, Response::Done { .. }),
        "unpin -> done, got {unpinned:?}"
    );
    assert!(
        !stat_pinned(&mut client, "/Notes.txt"),
        "stat reports it unpinned after unpin"
    );

    // Pinning a path that does not resolve is a NotFound error, not a panic.
    let (missing, _) = client
        .call(&Request {
            op: "pin".into(),
            path: "/does-not-exist.txt".into(),
            ..Default::default()
        })
        .expect("pin missing call");
    assert!(
        matches!(missing, Response::Error { .. }),
        "pinning a missing path errors, got {missing:?}"
    );

    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&base);
}
