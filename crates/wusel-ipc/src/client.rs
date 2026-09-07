// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A synchronous client for the socket, speaking the same framing as [`serve`].
//!
//! It exists so the socket contract can be driven without a hand-rolled framer —
//! by the `wusel ipc` diagnostic command, by the real-Nextcloud end-to-end test,
//! and by anything else that wants to poke a running daemon. It is the twin of
//! [`serve`], sharing [`crate::wire`], so the two cannot drift.
//!
//! [`serve`]: crate::serve

use std::io::{self, BufReader, BufWriter, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::wire::{self, Request, Response};

/// A connection to a `wusel serve` socket.
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
}

impl Client {
    /// Connect to the socket at `socket_path`.
    ///
    /// # Errors
    /// If the socket cannot be reached (no daemon, wrong path).
    pub fn connect(socket_path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(socket_path)?;
        Ok(Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: BufWriter::new(stream),
        })
    }

    /// Send one request and read its answer. A `fetch` answer carries its content
    /// in the second element; every other answer has `None` there.
    ///
    /// # Errors
    /// On any I/O failure, or if the connection closes before answering.
    pub fn call(&mut self, request: &Request) -> io::Result<(Response, Option<Vec<u8>>)> {
        self.send(request)?;
        let response = self.read_header()?;
        let body = if matches!(response, Response::Bytes { .. }) {
            Some(self.read_body()?)
        } else {
            None
        };
        Ok((response, body))
    }

    /// Send a `write` request whose bytes follow in the next frame, and read the
    /// `written` (or `error`) answer. The twin of the server reading a content
    /// frame after a `write` header.
    ///
    /// # Errors
    /// On any I/O failure, or if the connection closes before answering.
    pub fn call_write(&mut self, request: &Request, data: &[u8]) -> io::Result<Response> {
        let body = serde_json::to_vec(request).map_err(io::Error::other)?;
        wire::write_frame(&mut self.writer, &body)?;
        wire::write_frame(&mut self.writer, data)?;
        self.writer.flush()?;
        self.read_header()
    }

    /// Turn this connection into a change stream: send the `watch` request, then
    /// call [`next_push`](Self::next_push) repeatedly.
    ///
    /// # Errors
    /// If the request cannot be written.
    pub fn watch(&mut self) -> io::Result<()> {
        self.send(&Request {
            op: "watch".into(),
            ..Default::default()
        })
    }

    /// Turn this connection into a notice stream: send the `notices` request, then
    /// call [`next_push`](Self::next_push) repeatedly. The twin of
    /// [`watch`](Self::watch) for user-facing notices.
    ///
    /// # Errors
    /// If the request cannot be written.
    pub fn notices(&mut self) -> io::Result<()> {
        self.send(&Request {
            op: "notices".into(),
            ..Default::default()
        })
    }

    /// Block for the next pushed frame on a `watch` or `notices` connection;
    /// `None` at end of stream (the daemon closed it).
    ///
    /// # Errors
    /// On an I/O failure or an unparseable frame.
    pub fn next_push(&mut self) -> io::Result<Option<Response>> {
        match wire::read_frame(&mut self.reader)? {
            Some(frame) => Ok(Some(parse(&frame)?)),
            None => Ok(None),
        }
    }

    fn send(&mut self, request: &Request) -> io::Result<()> {
        let body = serde_json::to_vec(request).map_err(io::Error::other)?;
        wire::write_frame(&mut self.writer, &body)?;
        self.writer.flush()
    }

    fn read_header(&mut self) -> io::Result<Response> {
        let frame = wire::read_frame(&mut self.reader)?
            .ok_or_else(|| io::Error::new(ErrorKind::UnexpectedEof, "no response header"))?;
        parse(&frame)
    }

    fn read_body(&mut self) -> io::Result<Vec<u8>> {
        wire::read_frame(&mut self.reader)?
            .ok_or_else(|| io::Error::new(ErrorKind::UnexpectedEof, "no content frame after bytes"))
    }
}

fn parse(frame: &[u8]) -> io::Result<Response> {
    serde_json::from_slice(frame).map_err(io::Error::other)
}
