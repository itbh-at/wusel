// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The mock must not disguise I/O failures as "the file is not there".
//!
//! A handler that answers `404` for *every* `io::Error` turns a broken test box
//! (bad permissions, EIO, a path component that is not a directory) into the one
//! answer a sync client treats as authoritative: "the server does not have this
//! file". The client then happily deletes its local copy or re-uploads — silent
//! corruption instead of a red test. Only a genuine `NotFound` may be a 404;
//! everything else is the server's problem and must be a loud 500.
//!
//! The failure we stage is `ENOTDIR`: asking for `note.txt/child` makes the
//! kernel walk *through* a regular file. That is portable (POSIX guarantees
//! `ENOTDIR` for a non-directory path prefix), needs no permission games — which
//! would silently pass when the suite runs as root in a container — and reaches
//! exactly the same `else` arm of the handlers as EACCES or EIO would.

mod common;

use reqwest::{Method, StatusCode};

/// `note.txt/child` — a path *through* a regular file. Any `std::fs` call on it
/// fails with `ENOTDIR`, i.e. an `io::Error` that is not `NotFound`.
const THROUGH_A_FILE: &str = "note.txt/child";
/// A plain absent name: the one case that legitimately deserves a 404.
const GENUINELY_ABSENT: &str = "missing.txt";

#[tokio::test]
async fn io_errors_are_500_and_only_absence_is_404() {
    let base = std::env::temp_dir().join(format!("wusel-mock-ioerr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("note.txt"), b"hello").unwrap();

    let mock = common::Mock::serve(&fixture);
    let url = |rel: &str| format!("http://{}/remote.php/dav/files/alice/{rel}", mock.addr);
    let client = reqwest::Client::new();

    // GET — the handler a hydration goes through.
    let status = |m: Method, rel: &str| {
        let req = client.request(m, url(rel));
        async { req.send().await.expect("mock answers").status() }
    };
    assert_eq!(
        status(Method::GET, THROUGH_A_FILE).await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "GET must report an I/O error as a server error, not as absence"
    );
    assert_eq!(
        status(Method::GET, GENUINELY_ABSENT).await,
        StatusCode::NOT_FOUND
    );

    // DELETE — the handler a remote removal goes through.
    assert_eq!(
        status(Method::DELETE, THROUGH_A_FILE).await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "DELETE must report an I/O error as a server error, not as absence"
    );
    assert_eq!(
        status(Method::DELETE, GENUINELY_ABSENT).await,
        StatusCode::NOT_FOUND
    );

    // MOVE — the handler a rename goes through. The destination's parent is
    // creatable, so the only thing that can fail is the rename of the source.
    let move_ = Method::from_bytes(b"MOVE").unwrap();
    let move_status = |rel: &str| {
        let req = client
            .request(move_.clone(), url(rel))
            .header("Destination", url("moved.txt"));
        async { req.send().await.expect("mock answers").status() }
    };
    assert_eq!(
        move_status(THROUGH_A_FILE).await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "MOVE must report an I/O error as a server error, not as absence"
    );
    assert_eq!(move_status(GENUINELY_ABSENT).await, StatusCode::NOT_FOUND);

    drop(mock);
    std::fs::remove_dir_all(&base).ok();
}
