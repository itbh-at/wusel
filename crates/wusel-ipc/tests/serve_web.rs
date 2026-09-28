// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Web links and in-place update over the socket — the `weburl` and `update`
//! ops behind a file manager's "Open in Nextcloud" / "Copy internal link" /
//! "Update now" (Nautilus, and the macOS Finder Quick Actions). The engine
//! builds the link from local metadata (the object's file id plus the instance
//! base), so credentials never leave the daemon and the frontend just asks for
//! the finished string.
//!
//! Own test binary — it mutates the process-global XDG environment, so it must
//! not share a binary with another test (see `serve_pin_folder.rs`).

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

/// The stable file id `stat` reports for `path`.
fn file_id(client: &mut Client, path: &str) -> u64 {
    let (response, _) = client
        .call(&Request {
            op: "stat".into(),
            path: path.into(),
            ..Default::default()
        })
        .expect("stat call");
    match response {
        Response::Node { file_id, .. } => file_id.expect("a server-backed file has a file id"),
        other => panic!("expected a node for {path}, got {other:?}"),
    }
}

fn web_url(client: &mut Client, path: &str, reveal: bool) -> String {
    let (response, _) = client
        .call(&Request {
            op: "weburl".into(),
            path: path.into(),
            reveal,
            ..Default::default()
        })
        .expect("weburl call");
    match response {
        Response::WebUrl { url, .. } => url,
        other => panic!("expected a web_url for {path}, got {other:?}"),
    }
}

#[test]
fn weburl_and_update_over_the_socket() {
    let base = std::env::temp_dir().join(format!("wusel-ipc-web-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    xdg_sandbox(&base);

    let fixture = base.join("fixture");
    std::fs::create_dir_all(fixture.join("Docs")).unwrap();
    std::fs::write(fixture.join("Docs").join("plan.txt"), b"keep me").unwrap();
    let (addr, _rt) = serve_mock(&fixture);
    let server = format!("http://{addr}");

    let account = Account::new("default");
    let dav = WebDavClient::new(reqwest::Client::new(), &server, "alice", "pw");
    std::fs::create_dir_all(account.state_db_path().parent().unwrap()).unwrap();
    let state = StateDb::open(&account.state_db_path()).unwrap();
    let mut provider = Provider::new(dav, state, &account).unwrap();
    let invalidations = provider.take_invalidations().expect("invalidation channel");

    let events = Events::start(invalidations);
    let desktop = IpcDesktop::new();
    let driver = Arc::new(Driver::start(provider, wusel_core::runtime::Pools::default()).unwrap());

    let socket = std::env::temp_dir().join(format!("wusel-ipc-web-{}.sock", std::process::id()));
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

    let id = file_id(&mut client, "/Docs/plan.txt");

    // The object link: opens the file in the web viewer. Built from the file id,
    // so it survives a rename.
    assert_eq!(
        web_url(&mut client, "/Docs/plan.txt", false),
        format!("{server}/index.php/f/{id}"),
    );

    // The reveal link: opens the *parent* folder in the Files app with the item
    // highlighted, so its `dir` is the rooted parent path.
    assert_eq!(
        web_url(&mut client, "/Docs/plan.txt", true),
        format!("{server}/index.php/apps/files/files/{id}?dir=/Docs"),
    );

    // A path that is not on the server has no link.
    let (missing, _) = client
        .call(&Request {
            op: "weburl".into(),
            path: "/Docs/nope.txt".into(),
            ..Default::default()
        })
        .expect("weburl call");
    assert!(
        matches!(missing, Response::Error { .. }),
        "a path with no server object yields an error, got {missing:?}"
    );

    // `update` on a path that is not pinned is a quiet no-op — `Updated { 0 }`,
    // not an error. The macOS "Update Now" action is offered on every item (its
    // activation rule cannot read the pin state), so a click on an ordinary
    // online-only file must do nothing rather than fail.
    let (idle, _) = client
        .call(&Request {
            op: "update".into(),
            path: "/Docs".into(),
            ..Default::default()
        })
        .expect("update call");
    assert!(
        matches!(idle, Response::Updated { count: 0, .. }),
        "update on an unpinned path -> updated 0, got {idle:?}"
    );

    // `update` on a pinned folder is answered with a count (0 when already
    // current) — the "Update now" a frontend offers. Pin first, since update
    // refreshes what a pin promised.
    let (done, _) = client
        .call(&Request {
            op: "pin".into(),
            path: "/Docs".into(),
            ..Default::default()
        })
        .expect("pin call");
    assert!(matches!(done, Response::Done { .. }), "pin -> done");

    let (updated, _) = client
        .call(&Request {
            op: "update".into(),
            path: "/Docs".into(),
            ..Default::default()
        })
        .expect("update call");
    assert!(
        matches!(updated, Response::Updated { .. }),
        "update -> updated, got {updated:?}"
    );

    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&base);
}
