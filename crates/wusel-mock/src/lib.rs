// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! A tiny Nextcloud-style WebDAV mock server, for e2e runs (container and
//! in-process tests). It serves a **real directory** from disk as if it were a
//! Nextcloud user root, so exercising change detection is just `echo`/`rm` on
//! that directory — no mock-internal mutation API. It answers only what the
//! client needs:
//!
//! * `PROPFIND` (Depth 0/1) → a `<d:multistatus>` listing (ETag, size, file id),
//! * `GET` (optionally with a `Range`) → file bytes (200 or 206),
//! * `PUT` / `MKCOL` / `DELETE` / `MOVE` → mutate the backing directory, so a
//!   write round-trip (write then re-list/read) can be tested end-to-end.
//!
//! Everything is hand-rolled on `tokio` — no HTTP framework, no XML crate, no
//! percent-coding crate. It is a mock: the correctness bar is "the `wusel-core`
//! client is happy", not RFC completeness. The binary is a thin wrapper around
//! [`serve`]; tests can call [`serve`] in-process.
//!
//! **Deliberate tradeoff — blocking I/O inside async fns.** The handlers call
//! `std::fs` directly (including a whole-file `std::fs::read` for every range
//! GET), which blocks the runtime worker thread for the duration of the disk
//! operation. For a single-purpose test server with small fixtures that is
//! fine — it keeps the code short and readable, and nothing else competes for
//! the runtime. A production server must not do this: one slow disk operation
//! would stall every connection sharing the worker. There one would reach for
//! `tokio::fs` (async wrappers) or `tokio::task::spawn_blocking` (move the
//! blocking call onto the dedicated blocking thread pool).

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::fs::Metadata;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Server configuration, shared (by clone) with every connection.
#[derive(Clone)]
struct Config {
    /// Directory served as the user root.
    root: PathBuf,
    /// The href prefix the client expects: `/remote.php/dav/files/{user}`.
    prefix: String,
    /// The chunked-upload prefix: `/remote.php/dav/uploads/{user}`.
    uploads_prefix: String,
    /// Scratch directory where chunked-upload parts are staged.
    uploads_dir: PathBuf,
    /// Test-only fault injection: paths whose first `PUT` we have already failed.
    /// A `PUT` to a file named `*.fail-once` returns 500 the first time and
    /// succeeds afterwards, so a test can assert the client retries without
    /// losing the buffered content. Shared across connections via `Arc`.
    failed_once: Arc<Mutex<HashSet<String>>>,
}

/// Removes the uploads scratch directory when the server goes away.
///
/// `serve` never returns normally (its loop runs until the listener errors), so
/// "shutdown" is the caller *dropping the future* — aborting the task or
/// shutting down the runtime. Rust drops an async fn's locals at that point,
/// which is exactly the hook we need: holding this guard across the accept loop
/// turns future-drop into cleanup. Best-effort by design — a SIGKILLed mock
/// binary cannot run destructors, but the directory lives under the OS temp dir,
/// which reclaims leftovers.
struct UploadsDirGuard(PathBuf);

impl Drop for UploadsDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Serve the WebDAV mock on an already-bound `listener`, presenting `root` as the
/// user root for `user`. Runs until the listener errors (normally never), so
/// callers spawn it on a task/thread and drop it when done.
pub async fn serve(listener: TcpListener, root: PathBuf, user: &str) -> std::io::Result<()> {
    // Named by pid *and* port: several servers can run inside one test process
    // (parallel in-process tests), and pid alone would make them share — and,
    // worse, delete — each other's staging area.
    let uploads_dir = std::env::temp_dir().join(format!(
        "wusel-mock-uploads-{}-{}",
        std::process::id(),
        listener.local_addr()?.port()
    ));
    std::fs::create_dir_all(&uploads_dir)?;
    let _uploads_cleanup = UploadsDirGuard(uploads_dir.clone());
    let cfg = Config {
        root: std::fs::canonicalize(&root).unwrap_or(root),
        prefix: format!("/remote.php/dav/files/{user}"),
        uploads_prefix: format!("/remote.php/dav/uploads/{user}"),
        uploads_dir,
        failed_once: Arc::new(Mutex::new(HashSet::new())),
    };
    loop {
        let (stream, _peer) = listener.accept().await?;
        let cfg = cfg.clone();
        // One task per connection. We answer a single request and close (the
        // response carries `Connection: close`), which keeps the loop trivial.
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, &cfg).await {
                eprintln!("wusel-mock: connection error: {e}");
            }
        });
    }
}

/// A parsed request — everything downstream needs.
struct Request {
    method: String,
    /// Percent-decoded path relative to the user root (no leading slash).
    rel: String,
    depth: String,
    range: Option<(u64, Option<u64>)>,
    /// Request body (for `PUT`).
    body: Vec<u8>,
    /// The `Destination` header (for `MOVE`), verbatim.
    destination: Option<String>,
    /// The `If-Match` header (for conditional `PUT`/`MOVE`), verbatim (quoted).
    if_match: Option<String>,
    /// Path under the chunked-upload endpoint, if the target is there.
    upload_rel: Option<String>,
    /// The `X-OC-Mtime` header (unix seconds) to stamp the written file with.
    oc_mtime: Option<i64>,
}

async fn handle_conn(mut stream: TcpStream, cfg: &Config) -> std::io::Result<()> {
    let Some(req) = read_request(&mut stream, cfg).await? else {
        return Ok(()); // malformed or empty — just drop it
    };

    // Chunked-upload endpoint is a separate namespace.
    if let Some(upload_rel) = req.upload_rel.clone() {
        if rel_is_unsafe(&upload_rel) {
            return respond(
                &mut stream,
                "403 Forbidden",
                "text/plain",
                &[],
                b"forbidden",
            )
            .await;
        }
        return handle_upload(&mut stream, cfg, &req, &upload_rel).await;
    }

    // Guard against path traversal: refuse any `..` escaping the root.
    if rel_is_unsafe(&req.rel) {
        return respond(
            &mut stream,
            "403 Forbidden",
            "text/plain",
            &[],
            b"forbidden",
        )
        .await;
    }
    let fs_path = cfg.root.join(&req.rel);

    match req.method.as_str() {
        "OPTIONS" => {
            respond(
                &mut stream,
                "200 OK",
                "text/plain",
                &[("DAV".into(), "1, 2".into())],
                b"",
            )
            .await
        }
        "PROPFIND" => propfind(&mut stream, cfg, &req, &fs_path).await,
        "GET" | "HEAD" => get(&mut stream, &req, &fs_path).await,
        "PUT" => put(&mut stream, cfg, &req, &fs_path).await,
        "MKCOL" => mkcol(&mut stream, &fs_path).await,
        "DELETE" => delete(&mut stream, &fs_path).await,
        "MOVE" => move_(&mut stream, cfg, &req, &fs_path).await,
        _ => {
            respond(
                &mut stream,
                "405 Method Not Allowed",
                "text/plain",
                &[],
                b"nope",
            )
            .await
        }
    }
}

/// 500 with the I/O error text as the body. Filesystem errors on the request
/// path (disk full, permissions) must surface as a loud server error — a mock
/// that answers 201 after a failed write would let a client bug and a broken
/// test box look identical: silent corruption instead of a red test.
async fn respond_io_error(stream: &mut TcpStream, e: &std::io::Error) -> std::io::Result<()> {
    respond(
        stream,
        "500 Internal Server Error",
        "text/plain",
        &[],
        e.to_string().as_bytes(),
    )
    .await
}

/// Chunked upload NG: MKCOL the collection, PUT chunks named by offset, then
/// MOVE the `.file` marker to assemble them at the destination.
async fn handle_upload(
    stream: &mut TcpStream,
    cfg: &Config,
    req: &Request,
    upload_rel: &str,
) -> std::io::Result<()> {
    let staged = cfg.uploads_dir.join(upload_rel);
    match req.method.as_str() {
        "MKCOL" => match std::fs::create_dir_all(&staged) {
            Ok(()) => respond(stream, "201 Created", "text/plain", &[], b"").await,
            Err(e) => {
                respond(
                    stream,
                    "409 Conflict",
                    "text/plain",
                    &[],
                    e.to_string().as_bytes(),
                )
                .await
            }
        },
        "PUT" => {
            let written = staged
                .parent()
                .map(std::fs::create_dir_all)
                .unwrap_or(Ok(()))
                .and_then(|()| std::fs::write(&staged, &req.body));
            match written {
                Ok(()) => respond(stream, "201 Created", "text/plain", &[], b"").await,
                Err(e) => respond_io_error(stream, &e).await,
            }
        }
        "MOVE" => assemble_upload(stream, cfg, req, upload_rel).await,
        _ => respond(stream, "405 Method Not Allowed", "text/plain", &[], b"nope").await,
    }
}

/// MOVE `<id>/.file` → concatenate the chunks (sorted by their offset names) into
/// the destination file. Honours `If-Match` on the destination.
async fn assemble_upload(
    stream: &mut TcpStream,
    cfg: &Config,
    req: &Request,
    upload_rel: &str,
) -> std::io::Result<()> {
    let Some(id) = upload_rel
        .strip_suffix("/.file")
        .or_else(|| upload_rel.strip_suffix(".file"))
    else {
        return respond(stream, "400 Bad Request", "text/plain", &[], b"bad marker").await;
    };
    let chunk_dir = cfg.uploads_dir.join(id.trim_end_matches('/'));

    let Some(dest_rel) = req.destination.as_deref().and_then(|d| dest_rel(cfg, d)) else {
        return respond(
            stream,
            "400 Bad Request",
            "text/plain",
            &[],
            b"bad destination",
        )
        .await;
    };
    if rel_is_unsafe(&dest_rel) {
        return respond(stream, "403 Forbidden", "text/plain", &[], b"forbidden").await;
    }
    let dest = cfg.root.join(&dest_rel);

    // If-Match precondition on the destination.
    if let Some(expected) = &req.if_match {
        if expected != "*" {
            let current = std::fs::metadata(&dest)
                .ok()
                .map(|m| format!("\"{}\"", etag_for(&m)));
            if current.as_deref() != Some(expected.as_str()) {
                return respond(
                    stream,
                    "412 Precondition Failed",
                    "text/plain",
                    &[],
                    b"conflict",
                )
                .await;
            }
        }
    }

    // Collect chunk names (all but the marker) and sort — offset-padded → in
    // order. An unreadable staging dir (no MKCOL happened, or it got wiped)
    // means there is nothing to assemble — propagate instead of quietly
    // creating an empty destination file.
    let mut chunks: Vec<PathBuf> = match std::fs::read_dir(&chunk_dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().map(|n| n != ".file").unwrap_or(false))
            .collect(),
        Err(e) => return respond_io_error(stream, &e).await,
    };
    chunks.sort();

    // Concatenate the chunks into the destination, propagating every I/O error
    // (a full disk here must be a 500, not a silently truncated file). The
    // closure gives us `?` inside a fn that itself answers over the stream.
    let assemble = || -> std::io::Result<()> {
        use std::io::Write;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&dest)?;
        for chunk in &chunks {
            out.write_all(&std::fs::read(chunk)?)?;
        }
        Ok(())
    };
    if let Err(e) = assemble() {
        return respond_io_error(stream, &e).await;
    }
    // Best-effort staging cleanup: the destination is already complete, so a
    // leftover chunk dir is untidy but harmless — not worth failing the MOVE.
    let _ = std::fs::remove_dir_all(&chunk_dir);
    set_file_mtime(&dest, req.oc_mtime);

    let etag = std::fs::metadata(&dest)
        .map(|m| etag_for(&m))
        .unwrap_or_default();
    respond(
        stream,
        "201 Created",
        "text/plain",
        &[("ETag".into(), format!("\"{etag}\""))],
        b"",
    )
    .await
}

/// PUT: write the body to the backing file, replying with the new ETag. Honours
/// `If-Match` (412 if the current ETag differs), the conflict signal.
async fn put(
    stream: &mut TcpStream,
    cfg: &Config,
    req: &Request,
    fs_path: &Path,
) -> std::io::Result<()> {
    // Test-only fault injection (see `Config::failed_once`): fail the first PUT to
    // a `*.fail-once` file, so a test can assert the client retries and the buffer
    // survives.
    if req.rel.ends_with(".fail-once") {
        let first = cfg
            .failed_once
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(req.rel.clone());
        if first {
            return respond(
                stream,
                "500 Internal Server Error",
                "text/plain",
                &[],
                b"injected failure",
            )
            .await;
        }
    }
    if let Some(expected) = &req.if_match {
        if expected != "*" {
            let current = std::fs::metadata(fs_path)
                .ok()
                .map(|m| format!("\"{}\"", etag_for(&m)));
            // Mismatch (or the file no longer exists) → precondition failed.
            if current.as_deref() != Some(expected.as_str()) {
                return respond(
                    stream,
                    "412 Precondition Failed",
                    "text/plain",
                    &[],
                    b"conflict",
                )
                .await;
            }
        }
    }
    let written = fs_path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|()| std::fs::write(fs_path, &req.body));
    if let Err(e) = written {
        return respond_io_error(stream, &e).await;
    }
    set_file_mtime(fs_path, req.oc_mtime);
    let etag = std::fs::metadata(fs_path)
        .map(|m| etag_for(&m))
        .unwrap_or_default();
    respond(
        stream,
        "201 Created",
        "text/plain",
        &[("ETag".into(), format!("\"{etag}\""))],
        b"",
    )
    .await
}

/// MKCOL: create a directory.
async fn mkcol(stream: &mut TcpStream, fs_path: &Path) -> std::io::Result<()> {
    match std::fs::create_dir_all(fs_path) {
        Ok(()) => respond(stream, "201 Created", "text/plain", &[], b"").await,
        Err(e) => {
            respond(
                stream,
                "409 Conflict",
                "text/plain",
                &[],
                e.to_string().as_bytes(),
            )
            .await
        }
    }
}

/// DELETE: remove a file or directory subtree.
async fn delete(stream: &mut TcpStream, fs_path: &Path) -> std::io::Result<()> {
    let result = if fs_path.is_dir() {
        std::fs::remove_dir_all(fs_path)
    } else {
        std::fs::remove_file(fs_path)
    };
    match result {
        Ok(()) => respond(stream, "204 No Content", "text/plain", &[], b"").await,
        Err(_) => respond(stream, "404 Not Found", "text/plain", &[], b"not found").await,
    }
}

/// MOVE: rename to the `Destination` (a full URL under the user root).
async fn move_(
    stream: &mut TcpStream,
    cfg: &Config,
    req: &Request,
    fs_path: &Path,
) -> std::io::Result<()> {
    let Some(dest_rel) = req.destination.as_deref().and_then(|d| dest_rel(cfg, d)) else {
        return respond(
            stream,
            "400 Bad Request",
            "text/plain",
            &[],
            b"bad destination",
        )
        .await;
    };
    if rel_is_unsafe(&dest_rel) {
        return respond(stream, "403 Forbidden", "text/plain", &[], b"forbidden").await;
    }
    let dest_path = cfg.root.join(&dest_rel);
    // A failure to create the destination's parent is a server-side problem
    // (500), unlike the rename below, where a missing *source* is the client's
    // (404).
    if let Some(parent) = dest_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return respond_io_error(stream, &e).await;
        }
    }
    match std::fs::rename(fs_path, &dest_path) {
        Ok(()) => respond(stream, "201 Created", "text/plain", &[], b"").await,
        Err(_) => respond(stream, "404 Not Found", "text/plain", &[], b"not found").await,
    }
}

/// Extract the root-relative path from a `Destination` URL.
fn dest_rel(cfg: &Config, destination: &str) -> Option<String> {
    let decoded = percent_decode(destination);
    let idx = decoded.find(&cfg.prefix)?;
    Some(
        decoded[idx + cfg.prefix.len()..]
            .trim_matches('/')
            .to_string(),
    )
}

/// Reads and parses the request head (up to the blank line). The body, if any
/// (a PROPFIND carries one), is ignored — we always answer and close.
async fn read_request(stream: &mut TcpStream, cfg: &Config) -> std::io::Result<Option<Request>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        // Stop once the head is complete, or if the peer sends an absurd amount.
        if find_head_end(&buf).is_some() || buf.len() > 64 * 1024 {
            break;
        }
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break; // connection closed before a full head
        }
        buf.extend_from_slice(&tmp[..n]);
    }

    let Some(head_end) = find_head_end(&buf) else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = text.split("\r\n");

    let Some(request_line) = lines.next() else {
        return Ok(None);
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let (method, target) = (method.to_string(), target.to_string());

    let mut depth = "1".to_string();
    let mut range = None;
    let mut content_length = 0usize;
    let mut destination = None;
    let mut if_match = None;
    let mut oc_mtime = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "depth" => depth = value.to_string(),
            "range" => range = parse_range(value),
            "content-length" => content_length = value.parse().unwrap_or(0),
            "destination" => destination = Some(value.to_string()),
            "if-match" => if_match = Some(value.to_string()),
            "x-oc-mtime" => oc_mtime = value.parse().ok(),
            _ => {}
        }
    }

    // Read the body (for PUT): whatever already trails the head, plus the rest.
    let mut body = buf[head_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    // `target` may carry a query string; strip it, then map to a root-relative path.
    let path = target.split(['?', '#']).next().unwrap_or(&target);
    let decoded = percent_decode(path);
    let upload_rel = decoded
        .strip_prefix(&cfg.uploads_prefix)
        .map(|s| s.trim_matches('/').to_string());
    let rel = decoded
        .strip_prefix(&cfg.prefix)
        .unwrap_or(&decoded)
        .trim_matches('/')
        .to_string();

    Ok(Some(Request {
        method,
        rel,
        depth,
        range,
        body,
        destination,
        if_match,
        upload_rel,
        oc_mtime,
    }))
}

/// PROPFIND: describe the target and (at Depth 1, for a directory) its children.
async fn propfind(
    stream: &mut TcpStream,
    cfg: &Config,
    req: &Request,
    fs_path: &Path,
) -> std::io::Result<()> {
    let Ok(meta) = std::fs::metadata(fs_path) else {
        return respond(stream, "404 Not Found", "text/plain", &[], b"not found").await;
    };

    let mut body = String::from(
        "<?xml version=\"1.0\"?>\n\
         <d:multistatus xmlns:d=\"DAV:\" xmlns:oc=\"http://owncloud.org/ns\">\n",
    );
    body.push_str(&entry_xml(cfg, &req.rel, &meta));
    if meta.is_dir() && req.depth != "0" {
        // One entry per immediate child; unreadable entries are skipped.
        if let Ok(rd) = std::fs::read_dir(fs_path) {
            for child in rd.flatten() {
                let name = child.file_name().to_string_lossy().into_owned();
                let child_rel = if req.rel.is_empty() {
                    name
                } else {
                    format!("{}/{name}", req.rel)
                };
                if let Ok(cmeta) = child.metadata() {
                    body.push_str(&entry_xml(cfg, &child_rel, &cmeta));
                }
            }
        }
    }
    body.push_str("</d:multistatus>\n");

    respond(
        stream,
        "207 Multi-Status",
        "application/xml; charset=utf-8",
        &[],
        body.as_bytes(),
    )
    .await
}

/// GET/HEAD: serve the whole file, or the requested byte range (206).
async fn get(stream: &mut TcpStream, req: &Request, fs_path: &Path) -> std::io::Result<()> {
    let data = match std::fs::read(fs_path) {
        Ok(d) => d,
        Err(_) => return respond(stream, "404 Not Found", "text/plain", &[], b"not found").await,
    };
    let total = data.len() as u64;

    // A HEAD carries no body but the same status/headers as the GET would.
    let want_body = req.method == "GET";

    if let Some((start, end)) = req.range {
        // `end` is inclusive per HTTP; clamp to the file and default to the end.
        // A start at/after EOF — including *any* range of an empty file — is
        // unsatisfiable per RFC 7233. Without the `start >= total` check the
        // inclusive slice below would panic on `data[0..=0]` for an empty file,
        // killing the connection task instead of answering.
        let end = end
            .unwrap_or(total.saturating_sub(1))
            .min(total.saturating_sub(1));
        if start >= total || start > end {
            let hdr = [("Content-Range".to_string(), format!("bytes */{total}"))];
            return respond(stream, "416 Range Not Satisfiable", "text/plain", &hdr, b"").await;
        }
        let slice = &data[start as usize..=end as usize];
        let hdr = [(
            "Content-Range".to_string(),
            format!("bytes {start}-{end}/{total}"),
        )];
        let body: &[u8] = if want_body { slice } else { b"" };
        return respond(
            stream,
            "206 Partial Content",
            "application/octet-stream",
            &hdr,
            body,
        )
        .await;
    }

    let body: &[u8] = if want_body { &data } else { b"" };
    respond(stream, "200 OK", "application/octet-stream", &[], body).await
}

/// Renders one `<d:response>` for a file or directory at `rel`.
fn entry_xml(cfg: &Config, rel: &str, meta: &Metadata) -> String {
    let is_dir = meta.is_dir();
    let href = href_for(cfg, rel, is_dir);
    // A directory's ETag propagates its subtree (like Nextcloud), so a deep change
    // bumps every ancestor — which the client's sync walk relies on to find it.
    let etag = if is_dir {
        let fs_path = if rel.is_empty() {
            cfg.root.clone()
        } else {
            cfg.root.join(rel)
        };
        dir_etag(&fs_path)
    } else {
        etag_for(meta)
    };
    let file_id = stable_id(rel);

    // A fixed, well-formed date: enough for the client's RFC-1123 parser.
    let last_modified = "Mon, 01 Jan 2024 00:00:00 GMT";

    let resourcetype = if is_dir {
        "<d:resourcetype><d:collection/></d:resourcetype>".to_string()
    } else {
        format!(
            "<d:resourcetype/><d:getcontentlength>{}</d:getcontentlength>",
            meta.len()
        )
    };

    format!(
        "  <d:response>\n\
         \x20   <d:href>{href}</d:href>\n\
         \x20   <d:propstat><d:prop>\n\
         \x20     {resourcetype}\n\
         \x20     <d:getlastmodified>{last_modified}</d:getlastmodified>\n\
         \x20     <d:getetag>\"{etag}\"</d:getetag>\n\
         \x20     <oc:fileid>{file_id}</oc:fileid>\n\
         \x20   </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>\n\
         \x20 </d:response>\n"
    )
}

/// Absolute href under the user root, each path segment percent-encoded; a
/// directory gets the trailing slash of a WebDAV collection.
fn href_for(cfg: &Config, rel: &str, is_dir: bool) -> String {
    let mut href = cfg.prefix.clone();
    for seg in rel.split('/').filter(|s| !s.is_empty()) {
        href.push('/');
        href.push_str(&encode_segment(seg));
    }
    // Directories get a trailing slash; so does the bare user root (rel empty).
    if is_dir || href == cfg.prefix {
        href.push('/');
    }
    href
}

/// Stamp a file's mtime from an `X-OC-Mtime` value (best-effort), so a write
/// round-trip can verify timestamp propagation.
fn set_file_mtime(path: &Path, mtime: Option<i64>) {
    if let Some(secs) = mtime {
        if let (Ok(file), Ok(secs)) = (
            std::fs::File::options().write(true).open(path),
            u64::try_from(secs),
        ) {
            let _ = file.set_modified(UNIX_EPOCH + Duration::from_secs(secs));
        }
    }
}

/// ETag derived from size + mtime, so a content change (which bumps mtime)
/// yields a new ETag — exactly the signal the client's cache invalidation wants.
/// Recursive ("propagated") ETag for a directory: a hash of its children's names
/// and ETags, itself recursive for subdirectories. A change anywhere in the
/// subtree changes it — mirroring Nextcloud's ETag propagation, which the sync
/// walk relies on to locate a change from a path-less push.
fn dir_etag(fs_path: &Path) -> String {
    let mut entries: Vec<(String, String)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(fs_path) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let child = e.path();
            let et = if child.is_dir() {
                dir_etag(&child)
            } else if let Ok(m) = e.metadata() {
                etag_for(&m)
            } else {
                String::new()
            };
            entries.push((name, et));
        }
    }
    entries.sort();
    let mut h = DefaultHasher::new();
    for (n, et) in &entries {
        n.hash(&mut h);
        et.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

fn etag_for(meta: &Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = DefaultHasher::new();
    meta.len().hash(&mut h);
    mtime.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// A stable numeric file id: like Nextcloud's `oc:fileid`, constant per path.
fn stable_id(rel: &str) -> u64 {
    if rel.is_empty() {
        return 1; // the user root
    }
    let mut h = DefaultHasher::new();
    rel.hash(&mut h);
    // Keep it comfortably inside i64 range and non-zero, like real ids.
    (h.finish() >> 1).max(2)
}

/// True if the relative path tries to escape the root (`..`) or is absolute.
fn rel_is_unsafe(rel: &str) -> bool {
    Path::new(rel).components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

/// Parses a single-range `Range: bytes=start-[end]` header. Multi-ranges and
/// suffix ranges (`bytes=-N`) are not needed by the client, so are ignored.
fn parse_range(value: &str) -> Option<(u64, Option<u64>)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end = end.trim();
    let end = if end.is_empty() {
        None
    } else {
        Some(end.parse().ok()?)
    };
    Some((start, end))
}

/// Writes a complete HTTP/1.1 response and closes the connection.
async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    extra: &[(String, String)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         DAV: 1, 2\r\n\
         Connection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    // Half-close the write side so the client sees a clean end of stream.
    stream.shutdown().await.ok();
    Ok(())
}

/// Index just past the `\r\n\r\n` that ends the request head, if present.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Percent-encodes one path segment (RFC 3986 unreserved stays literal).
fn encode_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for &b in seg.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Minimal percent-decode (mirrors the client's), enough for request targets.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            root: PathBuf::from("/srv"),
            prefix: "/remote.php/dav/files/alice".into(),
            uploads_prefix: "/remote.php/dav/uploads/alice".into(),
            uploads_dir: PathBuf::from("/tmp/wusel-mock-uploads"),
            failed_once: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    #[test]
    fn href_encodes_segments_and_marks_dirs() {
        let c = cfg();
        assert_eq!(href_for(&c, "", true), "/remote.php/dav/files/alice/");
        assert_eq!(
            href_for(&c, "Sub Folder", true),
            "/remote.php/dav/files/alice/Sub%20Folder/"
        );
        assert_eq!(
            href_for(&c, "Sub Folder/notes.txt", false),
            "/remote.php/dav/files/alice/Sub%20Folder/notes.txt"
        );
    }

    #[test]
    fn range_header_parsing() {
        assert_eq!(parse_range("bytes=0-99"), Some((0, Some(99))));
        assert_eq!(parse_range("bytes=100-"), Some((100, None)));
        assert_eq!(parse_range("nonsense"), None);
    }

    #[test]
    fn traversal_is_rejected() {
        assert!(rel_is_unsafe("../etc/passwd"));
        assert!(rel_is_unsafe("a/../../b"));
        assert!(!rel_is_unsafe("Sub Folder/notes.txt"));
        assert!(!rel_is_unsafe(""));
    }

    #[test]
    fn stable_id_is_deterministic_and_nonzero() {
        assert_eq!(stable_id("a/b.txt"), stable_id("a/b.txt"));
        assert_ne!(stable_id("a/b.txt"), stable_id("a/c.txt"));
        assert_eq!(stable_id(""), 1);
        assert!(stable_id("x") >= 2);
    }

    #[test]
    fn percent_roundtrip_for_spaces() {
        assert_eq!(percent_decode("Sub%20Folder"), "Sub Folder");
        assert_eq!(encode_segment("Sub Folder"), "Sub%20Folder");
    }
}
