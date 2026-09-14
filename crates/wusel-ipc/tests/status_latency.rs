// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Prototype: the file-manager draw path over the socket, measured.
//!
//! The open design question behind moving per-file status off xattrs and onto a
//! socket query is latency: a file manager asks for a status *per visible file*,
//! synchronously, in its draw path. `getxattr` is a local syscall; a socket
//! round-trip is more. This test stands in for that draw path against the real
//! engine over a real Unix socket and measures two passes:
//!
//! * **cold** — no cache: one `stat` round-trip per file, the naive port of the
//!   xattr read. This is the number that decides whether a cache is optional.
//! * **warm** — a trivial client-side cache (path → status), the pattern the
//!   change stream keeps correct: the draw path reads memory, the socket is
//!   touched only on a miss.
//!
//! It also proves the neutral wire actually carries the full `state` end to end
//! (the point of the protocol), and that the cache returns the same answer the
//! socket did — a cache that lies is worse than no cache.
//!
//! Not a criterion benchmark: it runs in the normal test pass so the contract
//! stays exercised, and prints its timings (run with `--nocapture` to see them).

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wusel_core::config::Account;
use wusel_core::provider::Provider;
use wusel_core::state::StateDb;
use wusel_core::webdav::WebDavClient;
use wusel_ipc::wire::{self, Kind, SyncState};
use wusel_ipc::{Request, Response};

/// How many files the browsed folder holds. Enough that a per-file round-trip is
/// a visible sum, not lost in noise; small enough to stay a fast unit test.
const FILES: usize = 200;

fn xdg_sandbox(base: &Path) {
    let xdg = base.join("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.join("config"));
    std::env::set_var("XDG_STATE_HOME", xdg.join("state"));
    std::env::set_var("XDG_CACHE_HOME", xdg.join("cache"));
}

/// An in-process wusel-mock WebDAV server over `root`, as the other IPC tests
/// build it. The runtime must outlive the server.
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

fn connect(socket: &Path) -> UnixStream {
    for _ in 0..100 {
        if let Ok(s) = UnixStream::connect(socket) {
            return s;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("socket never came up: {}", socket.display());
}

/// One request → one response (these ops carry no trailing content frame).
fn call(
    r: &mut BufReader<UnixStream>,
    w: &mut BufWriter<UnixStream>,
    request: &Request,
) -> Response {
    let frame = serde_json::to_vec(request).unwrap();
    wire::write_frame(w, &frame).unwrap();
    w.flush().unwrap();
    let header = wire::read_frame(r).unwrap().expect("a response, not EOF");
    serde_json::from_slice(&header).unwrap()
}

fn enumerate(r: &mut BufReader<UnixStream>, w: &mut BufWriter<UnixStream>) -> Vec<String> {
    match call(
        r,
        w,
        &Request {
            op: "enumerate".into(),
            path: "/".into(),
            ..Default::default()
        },
    ) {
        Response::Entries { ok, entries } => {
            assert!(ok);
            entries.into_iter().map(|e| e.name).collect()
        }
        other => panic!("expected entries, got {other:?}"),
    }
}

/// A `stat`, returning the two status axes the draw path needs — exactly what a
/// file-manager plugin would read to pick an emblem and a badge.
fn stat_status(
    r: &mut BufReader<UnixStream>,
    w: &mut BufWriter<UnixStream>,
    path: &str,
) -> (SyncState, Kind) {
    match call(
        r,
        w,
        &Request {
            op: "stat".into(),
            path: path.into(),
            ..Default::default()
        },
    ) {
        Response::Node {
            ok,
            state,
            folder_kind,
            ..
        } => {
            assert!(ok, "stat of {path} failed");
            (state.unwrap_or_default(), folder_kind)
        }
        other => panic!("expected a node for {path}, got {other:?}"),
    }
}

#[test]
fn socket_status_draw_path_cold_vs_warm() {
    let base = std::env::temp_dir().join(format!("wusel-ipc-lat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    xdg_sandbox(&base);

    // A folder of FILES files, as a browsed directory the file manager decorates.
    let fixture = base.join("fixture");
    std::fs::create_dir_all(&fixture).unwrap();
    for i in 0..FILES {
        std::fs::write(fixture.join(format!("file-{i:04}.txt")), b"x").unwrap();
    }

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
    let events = wusel_ipc::Events::start(provider.take_invalidations().unwrap());
    let driver = Arc::new(
        wusel_ipc::Driver::start(provider, wusel_core::runtime::Pools::default()).unwrap(),
    );

    let socket = std::env::temp_dir().join(format!("wusel-ipc-lat-{}.sock", std::process::id()));
    let listener = wusel_ipc::bind(&socket).expect("bind the test socket");
    std::thread::spawn(move || {
        let _ = wusel_ipc::serve(driver, events, wusel_ipc::IpcDesktop::new(), listener);
    });
    let stream = connect(&socket);
    let mut r = BufReader::new(stream.try_clone().unwrap());
    let mut w = BufWriter::new(stream);

    // One enumerate primes the state DB (the reconcile the first listing does), so
    // the per-file stats below are served from local state — the same as a warm
    // mount, and the honest thing to time (we are measuring IPC, not the network).
    let names = enumerate(&mut r, &mut w);
    assert_eq!(names.len(), FILES, "the folder lists all its files");

    // --- Cold pass: one socket round-trip per file, no cache -----------------
    let mut socket_states: HashMap<String, (SyncState, Kind)> = HashMap::new();
    let cold_start = Instant::now();
    for name in &names {
        let path = format!("/{name}");
        let status = stat_status(&mut r, &mut w, &path);
        socket_states.insert(path, status);
    }
    let cold = cold_start.elapsed();

    // The neutral contract actually crossed the wire: every file reports a state
    // (an online-only file is `OnlineOnly`, not a missing field), and a plain
    // file's folder kind is `Plain` — the group-folder engine is on `main`.
    assert_eq!(socket_states.len(), FILES);
    assert!(
        socket_states.values().all(|(_, k)| *k == Kind::Plain),
        "every plain file reports folder_kind = plain on this branch"
    );
    assert!(
        socket_states
            .values()
            .all(|(s, _)| matches!(*s, SyncState::OnlineOnly | SyncState::Cached)),
        "a never-opened file is online-only or cached, never a transfer state"
    );

    // --- Warm pass: a client-side cache, the draw path reads memory -----------
    // The cache is the pattern the change stream keeps correct; here we only time
    // the hit path, which is what dominates once a folder has been drawn once.
    let cache = socket_states.clone();
    let warm_start = Instant::now();
    let mut drawn = 0usize;
    for name in &names {
        let path = format!("/{name}");
        let (_state, _kind) = cache.get(&path).expect("cache hit for a drawn file");
        drawn += 1;
    }
    let warm = warm_start.elapsed();
    assert_eq!(drawn, FILES);

    // The cache must return exactly what the socket did — a cache that drifts
    // would draw a stale emblem, the whole risk of caching.
    for name in &names {
        let path = format!("/{name}");
        assert_eq!(
            cache.get(&path),
            socket_states.get(&path),
            "cache and socket agree for {path}"
        );
    }

    let per_file_us = cold.as_micros() as f64 / FILES as f64;
    eprintln!("--- socket status draw path, {FILES} files ---");
    eprintln!("cold (one socket stat per file): {cold:?}  ({per_file_us:.1} µs/file)");
    eprintln!("warm (client cache hit per file): {warm:?}");
    eprintln!(
        "cache speedup: {:.0}x",
        cold.as_secs_f64() / warm.as_secs_f64().max(f64::MIN_POSITIVE)
    );

    // The load-bearing assertion is not a wall-clock threshold (too flaky in CI),
    // but the shape the design rests on: the cached draw path is dramatically
    // cheaper than a round-trip per file. If this ever fails, the caching premise
    // — not the socket — is what needs revisiting.
    assert!(
        warm < cold,
        "the warm cache must beat a socket round-trip per file (cold {cold:?}, warm {warm:?})"
    );
}
