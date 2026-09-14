// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end for the notify_push client: a fake server advertises the endpoint
//! via OCS capabilities and, over WebSocket, authenticates then pushes
//! `notify_file`. We assert the shared invalidation timestamp gets stamped —
//! the exact signal the provider consumes. No Nextcloud, pure Rust.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_file_stamps_invalidation() {
    // 1) WebSocket endpoint on its own port: accept, read login + password,
    //    reply `authenticated`, then push one `notify_file`.
    let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_port = ws.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((stream, _)) = ws.accept().await {
            let mut sock = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _login = sock.next().await; // login name
            let _pass = sock.next().await; // app password
            sock.send(Message::Text("authenticated".into()))
                .await
                .unwrap();
            sock.send(Message::Text("notify_file".into()))
                .await
                .unwrap();
            // Hold the socket open briefly so the client processes the message.
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    });

    // 2) OCS capabilities on another port: point notify_push at the WS endpoint.
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_port = http.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = http.accept().await {
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf).await; // consume the request head
            let body = format!(
                r#"{{"ocs":{{"data":{{"capabilities":{{"notify_push":{{"endpoints":{{"websocket":"ws://127.0.0.1:{ws_port}/push"}}}}}}}}}}}}"#
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });

    // 3) Run the real listener against the fake server.
    let invalidate_after = Arc::new(AtomicI64::new(0));
    let (sync_tx, sync_rx) = std::sync::mpsc::channel::<()>();
    let _listener = wusel_core::push::spawn(
        &format!("http://127.0.0.1:{http_port}"),
        "alice",
        "app-pw",
        wusel_core::config::TlsSettings::default(),
        invalidate_after.clone(),
        sync_tx,
        // No reachability tracker: this test is about the push protocol, and the
        // listener behaves identically without one.
        None,
    );

    // 4) The file event must stamp the timestamp within a few seconds.
    let mut stamped = false;
    for _ in 0..100 {
        if invalidate_after.load(Ordering::SeqCst) > 0 {
            stamped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(stamped, "notify_file must stamp invalidate_after");
    // …and it must also trigger the background syncer.
    assert!(
        sync_rx.try_recv().is_ok(),
        "notify_file must trigger the syncer"
    );
}

/// Records what the user would have been told.
#[derive(Default)]
struct Spy {
    notices: std::sync::Mutex<Vec<wusel_core::desktop::Notice>>,
}

impl wusel_core::desktop::Desktop for Spy {
    fn notify(&self, notice: &wusel_core::desktop::Notice) {
        self.notices.lock().unwrap().push(notice.clone());
    }
    fn set_status(&self, _status: wusel_core::desktop::Status) {}
}

/// A port nothing listens on: bound, read, released.
async fn closed_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

/// A capabilities server that advertises `ws_port` as the push endpoint.
async fn capabilities_server(ws_port: u16) -> u16 {
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_port = http.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = http.accept().await {
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf).await;
            let body = format!(
                r#"{{"ocs":{{"data":{{"capabilities":{{"notify_push":{{"endpoints":{{"websocket":"ws://127.0.0.1:{ws_port}/push"}}}}}}}}}}}}"#
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    http_port
}

/// The case from the field: the server answers every HTTP request and
/// advertises a WebSocket endpoint nobody can reach. That is a broken endpoint,
/// not a lost connection — the user must not be told the latter, however long
/// the socket keeps failing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_endpoint_behind_a_live_server_is_not_an_outage() {
    let ws_port = closed_port().await;
    let http_port = capabilities_server(ws_port).await;

    let spy = Arc::new(Spy::default());
    // Announce on the second failure, whenever it comes: the strictest tracker
    // there is, so silence here is silence by design, not by timing.
    let health = Arc::new(wusel_core::health::Reachability::with_timings(
        &format!("http://127.0.0.1:{http_port}"),
        spy.clone(),
        Duration::ZERO,
        Duration::ZERO,
    ));
    let (sync_tx, _sync_rx) = std::sync::mpsc::channel::<()>();
    let listener = wusel_core::push::spawn(
        &format!("http://127.0.0.1:{http_port}"),
        "alice",
        "app-pw",
        wusel_core::config::TlsSettings::default(),
        Arc::new(AtomicI64::new(0)),
        sync_tx,
        Some(health.clone()),
    );

    // Backoff 1 s, 2 s: at least three attempts inside this wait.
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    while std::time::Instant::now() < deadline && listener.status().snapshot().failures < 3 {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let status = listener.status().snapshot();
    assert!(status.failures >= 3, "the socket kept failing: {status:?}");
    assert!(
        spy.notices.lock().unwrap().is_empty(),
        "a broken endpoint is not a lost connection: {:?}",
        spy.notices.lock().unwrap()
    );
    assert!(!health.is_down(), "the server answered every probe");
}

/// The heartbeat still beats: with the server itself gone, the very same loop
/// is what tells the user — and it is judged by HTTP, so the socket's failure
/// is not what says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_server_is_still_announced() {
    let http_port = closed_port().await;

    let spy = Arc::new(Spy::default());
    let health = Arc::new(wusel_core::health::Reachability::with_timings(
        &format!("http://127.0.0.1:{http_port}"),
        spy.clone(),
        Duration::ZERO,
        Duration::ZERO,
    ));
    let (sync_tx, _sync_rx) = std::sync::mpsc::channel::<()>();
    let _listener = wusel_core::push::spawn(
        &format!("http://127.0.0.1:{http_port}"),
        "alice",
        "app-pw",
        wusel_core::config::TlsSettings::default(),
        Arc::new(AtomicI64::new(0)),
        sync_tx,
        Some(health.clone()),
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    while std::time::Instant::now() < deadline && spy.notices.lock().unwrap().is_empty() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let notices = spy.notices.lock().unwrap().clone();
    assert!(
        matches!(
            notices.first(),
            Some(wusel_core::desktop::Notice::ConnectionLost { .. })
        ),
        "an unreachable server is announced: {notices:?}"
    );
}
