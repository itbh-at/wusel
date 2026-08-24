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
