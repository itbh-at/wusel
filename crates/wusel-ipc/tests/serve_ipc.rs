// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end proof of the frontend boundary: the real `wusel-core` engine,
//! built against an in-process `wusel-mock` server exactly as the mount builds
//! it, driven over a Unix-domain socket by `wusel_ipc::serve` — browse, stat and
//! read, with no FUSE and no macOS in the loop.
//!
//! This is the Rust half of the future macOS File Provider extension's IPC: if
//! it is green natively, the socket contract a Swift frontend will speak is
//! proven independent of any platform.

use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wusel_core::config::Account;
use wusel_core::provider::Provider;
use wusel_core::state::StateDb;
use wusel_core::webdav::WebDavClient;
use wusel_ipc::{wire, Driver, ErrorKind, Request, Response};

/// Point XDG at a throwaway location, so the account's state/cache never touch
/// the real home directory. Sound here for the same structural reason the mock
/// harness relies on: this binary has exactly one `#[test]`, and this is its
/// first statement, so the process is effectively single-threaded when the
/// environment is mutated (see `wusel-mock/tests/common/mod.rs`).
fn xdg_sandbox(base: &Path) {
    let xdg = base.join("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.join("config"));
    std::env::set_var("XDG_STATE_HOME", xdg.join("state"));
    std::env::set_var("XDG_CACHE_HOME", xdg.join("cache"));
}

/// An in-process wusel-mock WebDAV server, serving `root` as user `alice` on an
/// OS-chosen port. The returned runtime must be kept alive for the server's
/// lifetime. Modelled on `wusel-mock/tests/common/mod.rs`.
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

/// Send one request and read its response (plus the content frame for `fetch`).
fn client_call(
    reader: &mut BufReader<UnixStream>,
    writer: &mut BufWriter<UnixStream>,
    request: &Request,
) -> (Response, Option<Vec<u8>>) {
    use std::io::Write;
    let frame = serde_json::to_vec(request).expect("serialise request");
    wire::write_frame(writer, &frame).expect("write request");
    writer.flush().expect("flush request");

    let header = wire::read_frame(reader)
        .expect("read response header")
        .expect("a response header, not EOF");
    let response: Response = serde_json::from_slice(&header).expect("parse response header");
    let body = match &response {
        Response::Bytes { len, .. } => {
            let bytes = wire::read_frame(reader)
                .expect("read content frame")
                .expect("a content frame, not EOF");
            assert_eq!(
                bytes.len() as u64,
                *len,
                "content frame matches the header len"
            );
            Some(bytes)
        }
        _ => None,
    };
    (response, body)
}

fn stat(r: &mut BufReader<UnixStream>, w: &mut BufWriter<UnixStream>, path: &str) -> Response {
    client_call(
        r,
        w,
        &Request {
            op: "stat".into(),
            path: path.into(),
            ..Default::default()
        },
    )
    .0
}

fn enumerate(r: &mut BufReader<UnixStream>, w: &mut BufWriter<UnixStream>, path: &str) -> Response {
    client_call(
        r,
        w,
        &Request {
            op: "enumerate".into(),
            path: path.into(),
            ..Default::default()
        },
    )
    .0
}

fn fetch(
    r: &mut BufReader<UnixStream>,
    w: &mut BufWriter<UnixStream>,
    path: &str,
    offset: u64,
    len: u32,
) -> Option<Vec<u8>> {
    client_call(
        r,
        w,
        &Request {
            op: "fetch".into(),
            path: path.into(),
            offset,
            len,
            ..Default::default()
        },
    )
    .1
}

#[test]
fn browses_and_reads_over_the_socket() {
    // A throwaway home for this test binary — first statement, single-threaded.
    let base = std::env::temp_dir().join(format!("wusel-ipc-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    xdg_sandbox(&base);

    // The fixture the server exposes: a 5-byte file and a directory with a space.
    let fixture = base.join("fixture");
    std::fs::create_dir_all(fixture.join("Sub Folder")).unwrap();
    std::fs::write(fixture.join("Notes.txt"), b"hello").unwrap();

    let (addr, _rt) = serve_mock(&fixture);

    // Build the engine the way the mount does: creds are the mock's, the state
    // DB lives under the sandboxed XDG dirs.
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
    // The change fan-out `serve` needs; this test does not exercise it (see
    // `serve_watch.rs`), but the provider must still hand it out before it moves.
    let events = wusel_ipc::Events::start(provider.take_invalidations().unwrap());

    let driver = Arc::new(Driver::start(provider, wusel_core::runtime::Pools::default()).unwrap());

    // Serve on a temp Unix socket in the background. A short path keeps well
    // inside the 108-byte sun_path limit.
    let socket = std::env::temp_dir().join(format!("wusel-ipc-{}.sock", std::process::id()));
    let socket_for_thread = socket.clone();
    std::thread::spawn(move || {
        let _ = wusel_ipc::serve(
            driver,
            events,
            wusel_ipc::IpcDesktop::new(),
            &socket_for_thread,
        );
    });

    // Connect, retrying until the background thread has bound the socket.
    let stream = connect(&socket);
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = BufWriter::new(stream);

    // enumerate("/") — the fixture's entries are both there, each with the
    // server's stable file id (the mock reports one per path).
    let notes_file_id = match enumerate(&mut reader, &mut writer, "/") {
        Response::Entries { ok, entries } => {
            assert!(ok);
            let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
            assert!(
                names.contains(&"Notes.txt"),
                "root has Notes.txt: {names:?}"
            );
            assert!(
                names.contains(&"Sub Folder"),
                "root has Sub Folder: {names:?}"
            );
            let folder = entries.iter().find(|e| e.name == "Sub Folder").unwrap();
            assert!(folder.is_dir, "Sub Folder is a directory");
            let notes = entries.iter().find(|e| e.name == "Notes.txt").unwrap();
            assert!(
                notes.file_id.is_some(),
                "a server-side entry carries a stable file id"
            );
            notes.file_id
        }
        other => panic!("expected entries, got {other:?}"),
    };

    // stat("/Notes.txt") — a 5-byte file, and the same file id enumerate gave.
    match stat(&mut reader, &mut writer, "/Notes.txt") {
        Response::Node {
            ok,
            size,
            is_dir,
            file_id,
            ..
        } => {
            assert!(ok);
            assert_eq!(size, 5, "Notes.txt is 5 bytes");
            assert!(!is_dir, "Notes.txt is a file");
            assert_eq!(
                file_id, notes_file_id,
                "stat and enumerate agree on the file id"
            );
        }
        other => panic!("expected a node, got {other:?}"),
    }

    // fetch("/Notes.txt", 0, 5) — the bytes come back in the content frame.
    let bytes = fetch(&mut reader, &mut writer, "/Notes.txt", 0, 5).expect("fetch returned bytes");
    assert_eq!(&bytes[..], b"hello", "fetch reads the file content");

    // A miss resolves to a not_found error rather than a hang or a panic.
    match stat(&mut reader, &mut writer, "/does-not-exist") {
        Response::Error { ok, error } => {
            assert!(!ok);
            assert_eq!(error, ErrorKind::NotFound);
        }
        other => panic!("expected not_found, got {other:?}"),
    }

    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&base);
}

/// Connect to the socket, retrying briefly while the server thread binds it.
fn connect(socket: &Path) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return stream,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("could not connect to {}: {e}", socket.display()),
        }
    }
}
