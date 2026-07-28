// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! `wusel-mock` binary — a thin CLI wrapper around [`wusel_mock::serve`].
//!
//! ```text
//! wusel-mock --addr 127.0.0.1:8080 --user alice --root ./fixture
//! ```

use std::path::PathBuf;

use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut addr = "127.0.0.1:8080".to_string();
    let mut user = "alice".to_string();
    let mut root = PathBuf::from(".");

    // Minimal flag parsing — a mock needs no clap.
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" => addr = args.next().unwrap_or(addr),
            "--user" => user = args.next().unwrap_or(user),
            "--root" => root = args.next().map(PathBuf::from).unwrap_or(root),
            "-h" | "--help" => {
                eprintln!("usage: wusel-mock [--addr HOST:PORT] [--user NAME] [--root DIR]");
                return Ok(());
            }
            other => eprintln!("wusel-mock: ignoring unknown argument {other:?}"),
        }
    }

    let listener = TcpListener::bind(&addr).await?;
    eprintln!(
        "wusel-mock: serving {} at http://{} (user {user})",
        root.display(),
        addr
    );
    wusel_mock::serve(listener, root, &user).await
}
