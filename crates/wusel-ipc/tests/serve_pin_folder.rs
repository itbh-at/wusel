// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Pinning a whole folder over the socket — the "make available offline" a
//! file-manager context menu offers on a directory (Nautilus, Finder), matching
//! the CLI's `wusel pin <dir>`. A directory has no content sync-state of its
//! own, so the daemon reports a kept-offline one as "pinned" and an unpinned one
//! as stateless; the menu offers the pin off that absence of a state, so this
//! locks the contract down.
//!
//! Own test binary — it mutates the process-global XDG environment, so it must
//! not share a binary with another test (see `serve_pin.rs`).

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

/// The `(pinned, is_dir, has_state)` a `stat` of `path` reports. `has_state` is
/// whether the node carries a content sync-state of its own — a plain directory
/// does not, which is why a folder's pin can only be read from `pinned`.
fn stat_node(client: &mut Client, path: &str) -> (bool, bool, bool) {
    let (response, _) = client
        .call(&Request {
            op: "stat".into(),
            path: path.into(),
            ..Default::default()
        })
        .expect("stat call");
    match response {
        Response::Node {
            pinned,
            is_dir,
            state,
            ..
        } => (pinned, is_dir, state.is_some()),
        other => panic!("expected a node for {path}, got {other:?}"),
    }
}

#[test]
fn a_folder_pins_over_the_socket_and_reports_it_without_a_content_state() {
    let base = std::env::temp_dir().join(format!("wusel-ipc-dirpin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    xdg_sandbox(&base);

    let fixture = base.join("fixture");
    std::fs::create_dir_all(fixture.join("Docs").join("Sub")).unwrap();
    std::fs::write(fixture.join("Docs").join("plan.txt"), b"keep me").unwrap();
    std::fs::write(fixture.join("Docs").join("Sub").join("deep.txt"), b"nested").unwrap();
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

    let socket = std::env::temp_dir().join(format!("wusel-ipc-dirpin-{}.sock", std::process::id()));
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

    // Not kept offline to begin with: a directory reports no content state of
    // its own, so a file manager can only tell "unpinned folder" from the
    // *absence* of a state — which is exactly what makes it offer the pin.
    let (pinned, is_dir, has_state) = stat_node(&mut client, "/Docs");
    assert!(is_dir, "/Docs is a directory");
    assert!(!pinned, "clean start: the folder is not pinned");
    assert!(
        !has_state,
        "an unpinned directory carries no content sync-state"
    );

    // Pin the whole folder.
    let (done, _) = client
        .call(&Request {
            op: "pin".into(),
            path: "/Docs".into(),
            ..Default::default()
        })
        .expect("pin call");
    assert!(
        matches!(done, Response::Done { .. }),
        "pin -> done, got {done:?}"
    );

    // The invariant the file-manager menu relies on: a kept-offline directory
    // reports both `pinned` and a content state ("pinned"), so a *stateless*
    // directory is always an unpinned one. The pin here, and the coverage of the
    // subdirectory below it, must both surface that way — otherwise the menu
    // would offer "make offline" on a folder that already is.
    let (pinned, is_dir, has_state) = stat_node(&mut client, "/Docs");
    assert!(
        is_dir && pinned && has_state,
        "the pinned folder reports pinned, with a content state"
    );
    let (sub_pinned, sub_is_dir, sub_has_state) = stat_node(&mut client, "/Docs/Sub");
    assert!(
        sub_is_dir && sub_pinned && sub_has_state,
        "a directory under a pinned folder is kept offline too, and says so"
    );

    // Unpin reverses it: back to no pin and no state.
    let (done, _) = client
        .call(&Request {
            op: "unpin".into(),
            path: "/Docs".into(),
            ..Default::default()
        })
        .expect("unpin call");
    assert!(
        matches!(done, Response::Done { .. }),
        "unpin -> done, got {done:?}"
    );
    let (pinned, _, has_state) = stat_node(&mut client, "/Docs");
    assert!(
        !pinned && !has_state,
        "stat reports the folder unpinned and stateless again"
    );

    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&base);
}
