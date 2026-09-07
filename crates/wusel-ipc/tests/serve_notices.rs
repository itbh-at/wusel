// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! User notices over the socket: a `notices` client is pushed a localized notice
//! whenever the engine reports one, which is what lets the macOS agent post a
//! Notification Center banner.
//!
//! Own test binary — it mutates the process-global XDG environment, so it must
//! not share a binary with another test (see `serve_watch.rs`).
//!
//! The notice is injected through the real [`wusel_ipc::IpcDesktop`] the engine
//! is wired to, so the whole delivery path — localize, fan out, frame, read — is
//! exercised without having to provoke an actual conflict or upload failure.

use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wusel_core::config::Account;
use wusel_core::desktop::{Desktop, Notice};
use wusel_core::provider::Provider;
use wusel_core::state::StateDb;
use wusel_core::webdav::WebDavClient;
use wusel_ipc::{wire, Driver, Events, IpcDesktop, Request, Response, Severity};

fn xdg_sandbox(base: &Path) {
    let xdg = base.join("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.join("config"));
    std::env::set_var("XDG_STATE_HOME", xdg.join("state"));
    std::env::set_var("XDG_CACHE_HOME", xdg.join("cache"));
}

#[test]
fn a_notice_reaches_a_notices_client() {
    let base = std::env::temp_dir().join(format!("wusel-ipc-notices-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    xdg_sandbox(&base);

    // The engine, with the socket notice backend installed exactly as `cmd_serve`
    // wires it. No server is contacted here — the notice is injected directly —
    // so a dead DAV URL is fine.
    let account = Account::new("default");
    let dav = WebDavClient::new(reqwest::Client::new(), "http://127.0.0.1:1", "alice", "pw");
    std::fs::create_dir_all(account.state_db_path().parent().unwrap()).unwrap();
    let state = StateDb::open(&account.state_db_path()).unwrap();
    let mut provider = Provider::new(dav, state, &account).unwrap();
    let invalidations = provider.take_invalidations().expect("invalidation channel");

    // One backend object: the engine reports through it, and the server subscribes
    // through it — the two ends the real daemon connects.
    let desktop = IpcDesktop::new();
    provider.set_desktop(Arc::clone(&desktop) as Arc<dyn Desktop>);

    let events = Events::start(invalidations);
    let driver = Arc::new(Driver::start(provider, wusel_core::runtime::Pools::default()).unwrap());

    let socket =
        std::env::temp_dir().join(format!("wusel-ipc-notices-{}.sock", std::process::id()));
    let socket_for_thread = socket.clone();
    let desktop_for_serve = Arc::clone(&desktop);
    std::thread::spawn(move || {
        let _ = wusel_ipc::serve(driver, events, desktop_for_serve, &socket_for_thread);
    });
    let up = Instant::now();
    while !socket.exists() {
        assert!(up.elapsed() < Duration::from_secs(5), "serve never bound");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Subscribe a `notices` client.
    let stream = UnixStream::connect(&socket).expect("connect notices");
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = BufWriter::new(stream);
    let sub = Request {
        op: "notices".into(),
        ..Default::default()
    };
    wire::write_frame(&mut writer, &serde_json::to_vec(&sub).unwrap()).unwrap();
    writer.flush().unwrap();
    // Give the server a moment to register the subscription before the notice.
    std::thread::sleep(Duration::from_millis(200));

    // The engine reports a notice; it must arrive on the socket, localized.
    desktop.notify(&Notice::UploadFailed {
        path: "big.iso".into(),
        reason: "quota exceeded".into(),
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let announced = loop {
        match wire::read_frame(&mut reader) {
            Ok(Some(frame)) => {
                let resp: Response = serde_json::from_slice(&frame).expect("parse notice frame");
                if let Response::Notice {
                    severity,
                    title,
                    body,
                    ..
                } = resp
                {
                    break (severity, title, body);
                }
            }
            Ok(None) => panic!("the notices connection closed before announcing a notice"),
            Err(_) => assert!(
                Instant::now() < deadline,
                "a notice was never announced on the notices socket"
            ),
        }
    };

    assert_eq!(
        announced.0,
        Severity::Error,
        "an upload failure is an error-severity notice"
    );
    assert!(
        !announced.1.is_empty() && !announced.2.is_empty(),
        "the notice carries a rendered title and body"
    );
    assert!(
        announced.2.contains("big.iso"),
        "the body names the file, got {:?}",
        announced.2
    );

    // The `test-notice` op — what `wusel desktop notify` uses — injects a sample
    // and reports it reached the still-open subscriber, which then receives it.
    let mut cli = wusel_ipc::Client::connect(&socket).expect("connect test-notice client");
    let (reply, _) = cli
        .call(&Request {
            op: "test-notice".into(),
            path: "warning".into(),
            ..Default::default()
        })
        .expect("test-notice call");
    match reply {
        Response::Notified { delivered, .. } => {
            assert_eq!(delivered, 1, "the one open subscriber received the sample");
        }
        other => panic!("expected a notified reply, got {other:?}"),
    }
    // The subscriber sees the injected sample, a warning.
    let sample_deadline = Instant::now() + Duration::from_secs(5);
    let sample = loop {
        match wire::read_frame(&mut reader) {
            Ok(Some(frame)) => {
                if let Response::Notice { severity, .. } =
                    serde_json::from_slice(&frame).expect("parse sample")
                {
                    break severity;
                }
            }
            Ok(None) => panic!("closed before the injected sample arrived"),
            Err(_) => assert!(
                Instant::now() < sample_deadline,
                "the injected sample never arrived"
            ),
        }
    };
    assert_eq!(
        sample,
        Severity::Warning,
        "the injected sample is a warning"
    );

    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&base);
}
