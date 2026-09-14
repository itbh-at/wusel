// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Change signalling over the socket: a `watch` client is told when the server
//! changes, which is what lets a File Provider call `signalEnumerator`.
//!
//! Own test binary — it mutates the process-global XDG environment, so it must
//! not share a binary with another test (see `serve_ipc.rs`).

use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wusel_core::config::Account;
use wusel_core::provider::Provider;
use wusel_core::state::{StateDb, ROOT_INODE};
use wusel_core::webdav::WebDavClient;
use wusel_fsm::{Intent, ObjectId};
use wusel_ipc::{wire, ChangeKind, Driver, Events, Request, Response};

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

#[test]
fn a_server_change_reaches_a_watch_client() {
    let base = std::env::temp_dir().join(format!("wusel-ipc-watch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    xdg_sandbox(&base);

    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("Notes.txt"), b"hello").unwrap();
    let (addr, _rt) = serve_mock(&fixture);

    // Build the engine, but take the change stream and a sync trigger out of the
    // provider before it is moved into the driver.
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
    let sync = provider.sync_trigger(); // Sender<()> — what a notify_push event does

    let events = Events::start(invalidations);
    let driver = Arc::new(Driver::start(provider, wusel_core::runtime::Pools::default()).unwrap());

    // Load the root, so the syncer has a cached listing to compare against.
    driver.call(ObjectId(ROOT_INODE), Intent::Enumerate);

    let socket = std::env::temp_dir().join(format!("wusel-ipc-watch-{}.sock", std::process::id()));
    let listener = wusel_ipc::bind(&socket).expect("bind the test socket");
    let driver_for_thread = Arc::clone(&driver);
    std::thread::spawn(move || {
        let _ = wusel_ipc::serve(
            driver_for_thread,
            events,
            wusel_ipc::IpcDesktop::new(),
            listener,
        );
    });
    // Wait for the socket to appear before connecting.
    let up = Instant::now();
    while !socket.exists() {
        assert!(up.elapsed() < Duration::from_secs(5), "serve never bound");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Open a watch connection and register the subscription.
    let stream = UnixStream::connect(&socket).expect("connect watch");
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = BufWriter::new(stream);
    let watch = Request {
        op: "watch".into(),
        ..Default::default()
    };
    wire::write_frame(&mut writer, &serde_json::to_vec(&watch).unwrap()).unwrap();
    writer.flush().unwrap();
    // Give the server a moment to subscribe before the change is made.
    std::thread::sleep(Duration::from_millis(200));

    // Somebody edits the file in the web interface.
    std::fs::write(fixture.join("Notes.txt"), b"edited in the web interface\n").unwrap();

    // Drive the syncer repeatedly (a single push could race its readiness) and
    // read until the change is announced on the socket.
    let deadline = Instant::now() + Duration::from_secs(15);
    let announced = loop {
        let _ = sync.send(());
        match wire::read_frame(&mut reader) {
            Ok(Some(frame)) => {
                let resp: Response = serde_json::from_slice(&frame).expect("parse changed frame");
                if let Response::Changed { change, path, .. } = resp {
                    break (change, path);
                }
                // Ignore anything that is not a change (there is nothing else on a
                // watch connection, but stay robust).
            }
            Ok(None) => panic!("the watch connection closed before announcing a change"),
            Err(_) => assert!(
                Instant::now() < deadline,
                "a server-side change was never announced on the watch socket"
            ),
        }
    };

    assert_eq!(
        announced.1, "Notes.txt",
        "the change names the path the frontend knows"
    );
    assert!(
        matches!(announced.0, ChangeKind::Content | ChangeKind::Entry),
        "a content or entry change, got {:?}",
        announced.0
    );

    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&base);
}
