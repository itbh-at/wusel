// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! End-to-end proof of the TLS trust modes against a real self-signed HTTPS
//! server (generated at runtime), so the behaviour is verified in CI rather than
//! by hand:
//!
//! * default (OS store)  → a self-signed cert is **rejected**,
//! * `insecure = true`   → **accepted**,
//! * `ca_cert = <pem>`   → **accepted**.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use wusel_core::config::TlsSettings;

/// A trivial HTTPS server that answers every request with `200 ok`.
async fn serve(listener: TcpListener, acceptor: TlsAcceptor) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            if let Ok(mut tls) = acceptor.accept(stream).await {
                let mut buf = [0u8; 1024];
                let _ = tls.read(&mut buf).await; // consume the request head
                let _ = tls
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
                let _ = tls.shutdown().await;
            }
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_signed_trust_modes() {
    // A fresh self-signed cert for "localhost".
    let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let ca_pem = issued.cert.pem();
    let cert_der = issued.cert.der().clone();
    let key_der = PrivatePkcs8KeyDer::from(issued.key_pair.serialize_der());

    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], PrivateKeyDer::Pkcs8(key_der))
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve(listener, acceptor));

    // Use the hostname so the "localhost" SAN matches in ca_cert mode.
    let url = format!("https://localhost:{port}/");

    // 1) Default: the OS store does not know this cert → reject.
    let strict = wusel_core::tls::client(&TlsSettings::default()).unwrap();
    assert!(
        strict.get(&url).send().await.is_err(),
        "a self-signed cert must be rejected by default"
    );

    // 2) insecure: verification off → accept.
    let insecure = wusel_core::tls::client(&TlsSettings {
        ca_cert: None,
        insecure: true,
    })
    .unwrap();
    let resp = insecure.get(&url).send().await;
    assert!(
        resp.is_ok() && resp.unwrap().status().is_success(),
        "insecure must accept the self-signed cert"
    );

    // 3) ca_cert: trust exactly this cert → accept.
    let ca_path = std::env::temp_dir().join(format!("wusel-e2e-ca-{}.pem", std::process::id()));
    std::fs::write(&ca_path, ca_pem).unwrap();
    let trusting = wusel_core::tls::client(&TlsSettings {
        ca_cert: Some(ca_path.clone()),
        insecure: false,
    })
    .unwrap();
    let resp = trusting.get(&url).send().await;
    assert!(
        resp.is_ok() && resp.unwrap().status().is_success(),
        "a matching ca_cert must accept the cert"
    );
    std::fs::remove_file(&ca_path).ok();
}
