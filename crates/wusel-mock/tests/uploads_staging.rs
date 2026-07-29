// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A fresh mock must never inherit a previous process's chunk staging area.
//!
//! The staging directory is named after pid and port under the OS temp dir. A
//! mock killed with `SIGKILL` runs no destructor and leaves it behind; a later
//! process that happens to get the same pid and bind the same port would then
//! find a populated directory — and `assemble_upload`, which simply
//! concatenates everything it finds sorted by name, would splice the stale
//! bytes into the freshly uploaded file. Recycled pids are not exotic: on Linux
//! they wrap at `/proc/sys/kernel/pid_max` (32768 by default), and a test suite
//! that starts and kills mocks in a loop cycles through them quickly.
//!
//! This test stages exactly that: it plants a leftover chunk *before* the server
//! starts, at the very path the server derives from its own pid and port.

use std::path::PathBuf;

use reqwest::{Method, StatusCode};

/// Mirrors the naming in `wusel_mock::serve`. Duplicated on purpose: the point
/// of the test is to be an outside observer that can predict — and therefore
/// poison — the staging path, exactly as a recycled pid would.
fn staging_dir(port: u16) -> PathBuf {
    std::env::temp_dir().join(format!(
        "wusel-mock-uploads-{}-{}",
        std::process::id(),
        port
    ))
}

#[tokio::test]
async fn a_fresh_server_does_not_inherit_leftover_chunks() {
    let base = std::env::temp_dir().join(format!("wusel-mock-staging-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();

    // Bind here, so the port — and with it the staging path — is known before
    // the server exists. `common::Mock` binds internally and would not let us
    // poison the directory in time.
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    std_listener.set_nonblocking(true).unwrap();

    // Poison: a chunk of a *previous*, SIGKILLed incarnation. It sorts before
    // the chunk this test uploads, so a server that inherits it would prepend
    // the stale bytes.
    let staging = staging_dir(addr.port());
    let upload_id = "wusel-upload-1";
    std::fs::create_dir_all(staging.join(upload_id)).unwrap();
    std::fs::write(
        staging.join(upload_id).join("00000000000000000000"),
        b"STALE",
    )
    .unwrap();
    // A second leftover, from an upload this test never mentions: nothing about
    // a fresh start may keep foreign staging content around at all.
    std::fs::create_dir_all(staging.join("foreign-upload")).unwrap();
    std::fs::write(staging.join("foreign-upload").join("00000000"), b"junk").unwrap();

    let root = fixture.clone();
    let server = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
        let _ = wusel_mock::serve(listener, root, "alice").await;
    });

    let client = reqwest::Client::new();
    let uploads = format!("http://{addr}/remote.php/dav/uploads/alice/{upload_id}");
    let dest = format!("http://{addr}/remote.php/dav/files/alice/big.bin");

    // Chunked upload NG: MKCOL, one chunk, then MOVE the marker to assemble.
    // The first request is answered only after the server finished its own
    // start-up, so no polling is needed to order this against the staging setup.
    let mkcol = client
        .request(Method::from_bytes(b"MKCOL").unwrap(), &uploads)
        .send()
        .await
        .expect("mock answers MKCOL");
    assert_eq!(mkcol.status(), StatusCode::CREATED);

    let put = client
        .put(format!("{uploads}/00000000000000000001"))
        .body("FRESH")
        .send()
        .await
        .expect("mock answers PUT");
    assert_eq!(put.status(), StatusCode::CREATED);

    let assemble = client
        .request(
            Method::from_bytes(b"MOVE").unwrap(),
            format!("{uploads}/.file"),
        )
        .header("Destination", &dest)
        .send()
        .await
        .expect("mock answers MOVE");
    assert_eq!(assemble.status(), StatusCode::CREATED);

    assert_eq!(
        std::fs::read(fixture.join("big.bin")).unwrap(),
        b"FRESH",
        "the assembled file must contain only this run's chunks"
    );
    assert!(
        !staging.join("foreign-upload").exists(),
        "a fresh server must not keep a dead predecessor's staging content"
    );

    server.abort();
    std::fs::remove_dir_all(&base).ok();
    std::fs::remove_dir_all(&staging).ok();
}
