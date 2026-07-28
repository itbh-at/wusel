// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Lean WebDAV client for Nextcloud.
//!
//! Endpoints under `{server}/remote.php/dav/files/{user}/…`. We use:
//! * `PROPFIND` (depth 1) — list a directory incl. ETag/size/file ID
//! * `GET` with `Range` — load content on-demand (also partially) → hydration
//! * `PUT` / `MKCOL` / `DELETE` / `MOVE` — writing
//! * chunked upload NG (`/dav/uploads`) — large files, one chunk in memory

use std::io::Read;
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;
use url::Url;

use crate::model::RemoteEntry;
use crate::{Error, Result};

/// Bytes per chunk for chunked upload NG. Bounds memory to one chunk regardless
/// of file size; files larger than this are uploaded in chunks.
pub const CHUNK_SIZE: u64 = 4 * 1024 * 1024;

/// Authenticated WebDAV client, bound to a single user.
#[derive(Clone)]
pub struct WebDavClient {
    http: reqwest::Client,
    /// e.g. `https://cloud.example.org/remote.php/dav/files/alice`
    base: String,
    login_name: String,
    app_password: String,
}

impl WebDavClient {
    pub fn new(
        http: reqwest::Client,
        server_url: &str,
        login_name: &str,
        app_password: &str,
    ) -> Self {
        let base = format!(
            "{}/remote.php/dav/files/{}",
            server_url.trim_end_matches('/'),
            login_name
        );
        Self {
            http,
            base,
            login_name: login_name.to_string(),
            app_password: app_password.to_string(),
        }
    }

    /// A copy of this client that uses a **separate** reqwest client (its own
    /// connection pool), keeping the same server, credentials and base path.
    ///
    /// Required whenever the copy is driven from a *different* tokio runtime: a
    /// single reqwest client shared across runtimes deadlocks, because hyper pins
    /// each pooled connection's driver task to the runtime that created it, and a
    /// request from the other runtime then waits on a task nothing is polling.
    pub fn with_http_client(&self, http: reqwest::Client) -> Self {
        Self {
            http,
            base: self.base.clone(),
            login_name: self.login_name.clone(),
            app_password: self.app_password.clone(),
        }
    }

    /// Builds the request URL for `path`, percent-encoding each segment (so names
    /// with spaces or Unicode work). `as_dir` appends a trailing slash, the
    /// canonical WebDAV address for a collection.
    fn url_for(&self, path: &str, as_dir: bool) -> Result<Url> {
        let mut url = Url::parse(&self.base)?;
        {
            let mut segs = url
                .path_segments_mut()
                .map_err(|_| Error::Other("WebDAV base URL cannot be a base".into()))?;
            for seg in path.split('/').filter(|s| !s.is_empty()) {
                segs.push(seg); // percent-encodes the segment
            }
            if as_dir {
                segs.push(""); // trailing slash
            }
        }
        Ok(url)
    }

    /// Lists a directory (depth 1) and returns its immediate children.
    pub async fn propfind_dir(&self, path: &str) -> Result<Vec<RemoteEntry>> {
        const BODY: &str = r#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:prop>
    <d:getcontentlength/>
    <d:getlastmodified/>
    <d:getetag/>
    <d:resourcetype/>
    <oc:fileid/>
    <oc:permissions/>
  </d:prop>
</d:propfind>"#;

        let resp = self
            .http
            .request(
                reqwest::Method::from_bytes(b"PROPFIND").unwrap(),
                self.url_for(path, true)?,
            )
            .basic_auth(&self.login_name, Some(&self.app_password))
            .header("Depth", "1")
            .header("Content-Type", "application/xml")
            .body(BODY)
            .send()
            .await?
            .error_for_status()?;

        let xml = resp.text().await?;
        let mut entries = parse_multistatus(&xml, &self.base)?;
        // A depth-1 PROPFIND also returns the queried directory itself. For the
        // root it decodes to an empty path and is already dropped; for a
        // subdirectory it comes back as its own path, so drop it explicitly.
        let self_path = path.trim_matches('/');
        entries.retain(|e| e.path != self_path);
        Ok(entries)
    }

    /// Loads (part of) a file. `range` = (start, len) for a range GET.
    pub async fn get(&self, path: &str, range: Option<(u64, u64)>) -> Result<bytes::Bytes> {
        // A zero-length range has an empty answer by definition — and the header
        // arithmetic below (`start + len - 1`, an *inclusive* end per RFC 9110)
        // would underflow at len 0. Short-circuit without any HTTP request.
        if let Some((_, 0)) = range {
            return Ok(bytes::Bytes::new());
        }
        // PROPFIND and PUT log; GET must too, or the dominant traffic of an
        // online-only VFS — content reads — is invisible in a debug log and the
        // mount looks idle while it hammers the server.
        tracing::debug!(%path, ?range, "GET");
        let mut req = self
            .http
            .get(self.url_for(path, false)?)
            .basic_auth(&self.login_name, Some(&self.app_password));
        if let Some((start, len)) = range {
            req = req.header("Range", format!("bytes={}-{}", start, start + len - 1));
        }
        let resp = req.send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // Deleted on the server since we last listed it — a distinct, benign
            // signal (the caller prunes the stale node), not a transport failure.
            return Err(Error::NotFound);
        }
        let status = resp.status();
        let body = resp.error_for_status()?.bytes().await?;
        match range {
            // RFC 9110 §14.2: Range is an optional optimisation — a server (or a
            // proxy stripping the header) MAY answer 200 with the FULL body.
            // Serve the requested window out of it locally. Passing the oversized
            // buffer up would make the FUSE layer answer a 128 KiB read with the
            // whole file, which the kernel rejects with EIO — every read of every
            // file then fails ("cat: Input/output error") with nothing in our
            // logs, since from our side the transfer "succeeded".
            Some((start, len)) if status != reqwest::StatusCode::PARTIAL_CONTENT => {
                warn_range_ignored();
                Ok(slice_full_body(body, start, len))
            }
            _ => Ok(body),
        }
    }

    // --- Writing (phase 1) --------------------------------------------------

    /// Uploads a whole file with a simple `PUT`. Returns the server's new ETag if
    /// it sends one (Nextcloud does). Chunked upload NG (for large files) is a
    /// later refinement; a plain PUT is correct for any size.
    pub async fn put(&self, path: &str, body: Vec<u8>) -> Result<Option<String>> {
        tracing::debug!(%path, bytes = body.len(), "PUT");
        let resp = self
            .http
            .put(self.url_for(path, false)?)
            .basic_auth(&self.login_name, Some(&self.app_password))
            .body(body)
            .send()
            .await?
            .error_for_status()?;
        Ok(etag_from_headers(&resp))
    }

    /// Conditional upload: `PUT` with `If-Match`, so the server rejects (412) if
    /// the file changed since `if_match` — our lost-update / conflict signal. An
    /// **empty** `if_match` means "this file must not exist yet" and sends
    /// `If-None-Match: *` instead: a deferred create (no base version) must not
    /// silently clobber a same-named file created concurrently on the server.
    /// Either precondition failing maps to [`PutResult::Conflict`].
    pub async fn put_if_match(
        &self,
        path: &str,
        body: Vec<u8>,
        if_match: &str,
        mtime: Option<i64>,
    ) -> Result<PutResult> {
        tracing::debug!(%path, bytes = body.len(), "PUT (conditional)");
        let mut req = self
            .http
            .put(self.url_for(path, false)?)
            .basic_auth(&self.login_name, Some(&self.app_password));
        req = apply_precondition(req, if_match);
        if let Some(m) = mtime {
            req = req.header("X-OC-Mtime", m.to_string());
        }
        let resp = req.body(body).send().await?;
        if resp.status() == reqwest::StatusCode::PRECONDITION_FAILED {
            return Ok(PutResult::Conflict);
        }
        Ok(PutResult::Uploaded(etag_from_headers(
            &resp.error_for_status()?,
        )))
    }

    /// Creates a collection (directory) with `MKCOL`.
    pub async fn mkcol(&self, path: &str) -> Result<()> {
        tracing::debug!(%path, "MKCOL");
        self.http
            .request(
                reqwest::Method::from_bytes(b"MKCOL").unwrap(),
                self.url_for(path, true)?,
            )
            .basic_auth(&self.login_name, Some(&self.app_password))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Deletes a file or directory.
    pub async fn delete(&self, path: &str, is_dir: bool) -> Result<()> {
        tracing::debug!(%path, is_dir, "DELETE");
        self.http
            .delete(self.url_for(path, is_dir)?)
            .basic_auth(&self.login_name, Some(&self.app_password))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// URL under the chunked-upload endpoint (`/remote.php/dav/uploads/{user}`).
    fn uploads_url(&self, rel: &str) -> Result<Url> {
        let uploads_base = self.base.replacen("/dav/files/", "/dav/uploads/", 1);
        let mut url = Url::parse(&uploads_base)?;
        {
            let mut segs = url
                .path_segments_mut()
                .map_err(|_| Error::Other("uploads base URL cannot be a base".into()))?;
            for seg in rel.split('/').filter(|s| !s.is_empty()) {
                segs.push(seg);
            }
        }
        Ok(url)
    }

    /// Chunked upload NG: create an upload collection, `PUT` the file in chunks
    /// (named by byte offset so they sort in order), then `MOVE` the `.file`
    /// marker to the target to assemble it. Streams from `source`, so only one
    /// chunk is ever in memory. The precondition on the final `MOVE` matches
    /// [`put_if_match`](Self::put_if_match): a non-empty `if_match` sends
    /// `If-Match` (412 = the file changed under us), an empty one sends
    /// `If-None-Match: *` (412 = a deferred create raced a server-side create).
    /// Nextcloud honours both on the assembling MOVE.
    pub async fn put_chunked(
        &self,
        target: &str,
        source: &Path,
        total: u64,
        if_match: &str,
        mtime: Option<i64>,
    ) -> Result<PutResult> {
        tracing::debug!(path = %target, bytes = total, "PUT (chunked)");
        let id = format!("wusel-{}-{}", std::process::id(), unique_suffix());

        // 1. MKCOL the upload collection. (Nothing to clean up if this fails.)
        self.http
            .request(
                reqwest::Method::from_bytes(b"MKCOL").unwrap(),
                self.uploads_url(&id)?,
            )
            .basic_auth(&self.login_name, Some(&self.app_password))
            .send()
            .await?
            .error_for_status()?;

        let outcome = self
            .upload_chunks_and_assemble(&id, target, source, total, if_match, mtime)
            .await;
        // A successful MOVE consumes the upload collection server-side. On every
        // other outcome — a failed chunk PUT, a failed MOVE, or a 412 — the
        // collection and its chunks would otherwise linger on the server forever
        // (invisible to the user, but counted against the quota). Delete it
        // best-effort: its own failure is only logged, because the caller must
        // see the *original* outcome, not the cleanup's.
        if !matches!(outcome, Ok(PutResult::Uploaded(_))) {
            if let Err(e) = self.delete_upload_collection(&id).await {
                tracing::debug!(%e, upload_id = %id, "could not clean up the upload collection");
            }
        }
        outcome
    }

    /// Steps 2–3 of [`put_chunked`](Self::put_chunked): PUT the chunks, then MOVE
    /// the assembly marker. Split out so the caller can clean up the upload
    /// collection on *any* early `?` return without sprinkling cleanup calls.
    async fn upload_chunks_and_assemble(
        &self,
        id: &str,
        target: &str,
        source: &Path,
        total: u64,
        if_match: &str,
        mtime: Option<i64>,
    ) -> Result<PutResult> {
        // 2. PUT each chunk (read locally, so RAM stays at one chunk).
        let mut file = std::fs::File::open(source)?;
        let mut buf = vec![0u8; CHUNK_SIZE as usize];
        let mut offset = 0u64;
        loop {
            let n = read_up_to(&mut file, &mut buf)?;
            if n == 0 {
                break;
            }
            let chunk = self.uploads_url(&format!("{id}/{offset:016}"))?;
            self.http
                .put(chunk)
                .basic_auth(&self.login_name, Some(&self.app_password))
                .body(buf[..n].to_vec())
                .send()
                .await?
                .error_for_status()?;
            offset += n as u64;
        }

        // 3. MOVE the assembly marker to the destination.
        let dest = self.url_for(target, false)?;
        let mut req = self
            .http
            .request(
                reqwest::Method::from_bytes(b"MOVE").unwrap(),
                self.uploads_url(&format!("{id}/.file"))?,
            )
            .basic_auth(&self.login_name, Some(&self.app_password))
            .header("Destination", dest.as_str())
            .header("OC-Total-Length", total.to_string());
        req = apply_precondition(req, if_match);
        if let Some(m) = mtime {
            req = req.header("X-OC-Mtime", m.to_string());
        }
        let resp = req.send().await?;
        if resp.status() == reqwest::StatusCode::PRECONDITION_FAILED {
            return Ok(PutResult::Conflict);
        }
        Ok(PutResult::Uploaded(etag_from_headers(
            &resp.error_for_status()?,
        )))
    }

    /// Best-effort DELETE of an abandoned upload collection (see
    /// [`put_chunked`](Self::put_chunked)).
    async fn delete_upload_collection(&self, id: &str) -> Result<()> {
        self.http
            .delete(self.uploads_url(id)?)
            .basic_auth(&self.login_name, Some(&self.app_password))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Moves (renames) `from` to `to`. The `Destination` header is the full URL
    /// of the target, per WebDAV.
    pub async fn move_(&self, from: &str, to: &str, is_dir: bool) -> Result<()> {
        tracing::debug!(%from, %to, is_dir, "MOVE");
        let dest = self.url_for(to, is_dir)?;
        self.http
            .request(
                reqwest::Method::from_bytes(b"MOVE").unwrap(),
                self.url_for(from, is_dir)?,
            )
            .basic_auth(&self.login_name, Some(&self.app_password))
            .header("Destination", dest.as_str())
            .header("Overwrite", "T")
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// Outcome of a conditional [`put_if_match`](WebDavClient::put_if_match).
pub enum PutResult {
    /// Uploaded; carries the server's new ETag if it sent one.
    Uploaded(Option<String>),
    /// The server rejected the upload (412) — the file changed under us, or (for
    /// a deferred create's `If-None-Match: *`) it already exists.
    Conflict,
}

/// Attach the shared upload precondition: `If-Match: "<etag>"` when a base
/// version is known, `If-None-Match: *` ("must not exist yet") when it is not.
/// One helper so the plain PUT and the chunked upload's final MOVE cannot
/// drift apart in their conflict semantics.
fn apply_precondition(req: reqwest::RequestBuilder, if_match: &str) -> reqwest::RequestBuilder {
    if if_match.is_empty() {
        req.header(reqwest::header::IF_NONE_MATCH, "*")
    } else {
        req.header(reqwest::header::IF_MATCH, format!("\"{if_match}\""))
    }
}

/// Reads up to `buf.len()` bytes, coalescing short reads until the buffer is
/// full or EOF. Returns how many bytes were read (0 = EOF).
fn read_up_to(file: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        let n = file.read(&mut buf[total..])?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

/// A process-unique suffix for an upload id (no external RNG dependency).
fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// The requested window out of a full-body (200) response to a range GET, with
/// out-of-bounds ranges clamped (a read past EOF yields an empty buffer).
fn slice_full_body(body: bytes::Bytes, start: u64, len: u64) -> bytes::Bytes {
    let s = usize::try_from(start).unwrap_or(usize::MAX).min(body.len());
    let e = usize::try_from(start.saturating_add(len))
        .unwrap_or(usize::MAX)
        .min(body.len());
    body.slice(s..e)
}

/// Warn once per process that the server ignores Range requests. Worth
/// surfacing — every partial read then transfers the whole file, so the
/// server/proxy config deserves a look — but not worth one line per read.
fn warn_range_ignored() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "server ignored a Range request and sent the full body — serving the requested \
             slice locally; partial reads now transfer whole files, check the server/proxy \
             config (something strips Range or does not implement it)"
        );
    }
}

/// Extracts the `ETag` response header, unquoted, if present.
fn etag_from_headers(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"').to_string())
}

/// Parses a `<d:multistatus>` response into [`RemoteEntry`]s.
///
/// Namespaces are handled in a simplified way (local name after `:`), which is
/// sufficient for Nextcloud's fixed output. `base` serves to make the absolute
/// href path relative to the user root again.
fn parse_multistatus(xml: &str, base: &str) -> Result<Vec<RemoteEntry>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let base_path = base
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, p)| format!("/{p}"))
        .unwrap_or_default();

    let mut entries = Vec::new();
    let mut cur = PartialEntry::default();
    let mut text_target: Option<Field> = None;
    let mut in_response = false;

    loop {
        match reader.read_event() {
            Err(e) => return Err(Error::WebDav(e.to_string())),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => match local_name(e.name().as_ref()) {
                b"response" => {
                    in_response = true;
                    cur = PartialEntry::default();
                }
                b"href" => text_target = Some(Field::Href),
                b"getcontentlength" => text_target = Some(Field::Size),
                b"getlastmodified" => text_target = Some(Field::MTime),
                b"getetag" => text_target = Some(Field::ETag),
                b"fileid" => text_target = Some(Field::FileId),
                b"permissions" => text_target = Some(Field::Permissions),
                b"collection" => cur.is_dir = true,
                _ => {}
            },
            // Self-closing elements like `<d:collection/>` arrive as Empty, not Start.
            Ok(Event::Empty(e)) => {
                if local_name(e.name().as_ref()) == b"collection" {
                    cur.is_dir = true;
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(field) = text_target.take() {
                    let val = t.unescape().unwrap_or_default().into_owned();
                    cur.set(field, &val);
                }
            }
            Ok(Event::End(e)) => {
                // Any element end disarms a pending text target: an empty
                // property serialized as `<d:getetag></d:getetag>` (Start+End
                // with no Text in between — XML-equivalent to the self-closing
                // form) must not capture the next text node of a *sibling*
                // element (e.g. the `<d:status>` line of a 404 propstat).
                text_target = None;
                if in_response && local_name(e.name().as_ref()) == b"response" {
                    in_response = false;
                    // Take `cur` out via take() so the next
                    // <response> iteration starts with a fresh default.
                    if let Some(entry) = std::mem::take(&mut cur).finish(&base_path) {
                        entries.push(entry);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(entries)
}

#[derive(Clone, Copy)]
enum Field {
    Href,
    Size,
    MTime,
    ETag,
    FileId,
    Permissions,
}

#[derive(Default)]
struct PartialEntry {
    href: String,
    size: u64,
    mtime: i64,
    etag: String,
    file_id: Option<u64>,
    permissions: String,
    is_dir: bool,
}

impl PartialEntry {
    fn set(&mut self, field: Field, val: &str) {
        match field {
            Field::Href => self.href = val.to_string(),
            Field::Size => self.size = val.parse().unwrap_or(0),
            Field::MTime => self.mtime = parse_http_date(val),
            Field::ETag => self.etag = val.trim_matches('"').to_string(),
            Field::FileId => self.file_id = val.parse().ok(),
            Field::Permissions => self.permissions = val.to_string(),
        }
    }

    /// Builds the final entry; the directory itself (href == base) is dropped.
    fn finish(self, base_path: &str) -> Option<RemoteEntry> {
        let decoded = percent_decode(&self.href);
        let rel = decoded.strip_prefix(base_path).unwrap_or(&decoded);
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            return None; // the requested directory itself
        }
        Some(RemoteEntry {
            path: rel.to_string(),
            is_dir: self.is_dir,
            size: self.size,
            etag: self.etag,
            mtime: self.mtime,
            file_id: self.file_id,
            permissions: self.permissions,
        })
    }
}

/// Removes an `ns:` prefix and returns the local element name.
fn local_name(name: &[u8]) -> &[u8] {
    match name.iter().position(|&b| b == b':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// Minimal percent-decode for href paths (enough for file names).
fn percent_decode(s: &str) -> String {
    // Work on raw bytes throughout: slicing the `&str` for the two hex digits
    // would panic if a `%` is followed within two bytes by a multi-byte UTF-8
    // character (`%aä`) — servers percent-encode hrefs, but a broken or
    // malicious one must not be able to crash the client.
    fn hex_val(b: u8) -> Option<u8> {
        (b as char).to_digit(16).map(|v| v as u8)
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses an RFC-1123 / IMF-fixdate (`getlastmodified`) into Unix seconds.
///
/// WebDAV `getlastmodified` is always GMT in the fixed form
/// `Wed, 21 Jul 2026 18:00:00 GMT`. We parse just that shape (no external time
/// crate) and return `0` on anything unexpected, so a odd value can never fail a
/// listing.
fn parse_http_date(s: &str) -> i64 {
    // Fields: ["Wed,", "21", "Jul", "2026", "18:00:00", "GMT"]
    let mut it = s.split_whitespace();
    let (_wday, day, mon, year, time) =
        match (it.next(), it.next(), it.next(), it.next(), it.next()) {
            (Some(w), Some(d), Some(m), Some(y), Some(t)) => (w, d, m, y, t),
            _ => return 0,
        };
    let (day, year) = match (day.parse::<i64>(), year.parse::<i64>()) {
        (Ok(d), Ok(y)) => (d, y),
        _ => return 0,
    };
    let month = match month_num(mon) {
        Some(m) => m,
        None => return 0,
    };
    let mut hms = time.split(':');
    let (h, mi, sec) = match (hms.next(), hms.next(), hms.next()) {
        (Some(h), Some(m), Some(s)) => (h, m, s),
        _ => return 0,
    };
    let (h, mi, sec) = match (h.parse::<i64>(), mi.parse::<i64>(), sec.parse::<i64>()) {
        (Ok(h), Ok(m), Ok(s)) => (h, m, s),
        _ => return 0,
    };
    days_from_civil(year, month, day) * 86_400 + h * 3_600 + mi * 60 + sec
}

/// Three-letter English month name → 1..=12.
fn month_num(m: &str) -> Option<i64> {
    Some(match m {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    })
}

/// Days since 1970-01-01 for a civil (proleptic Gregorian) date. Howard
/// Hinnant's `days_from_civil` — exact integer arithmetic, no time crate.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:response>
    <d:href>/remote.php/dav/files/alice/</d:href>
    <d:propstat><d:prop>
      <d:resourcetype><d:collection/></d:resourcetype>
      <d:getetag>"root123"</d:getetag>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/Notes.txt</d:href>
    <d:propstat><d:prop>
      <d:getcontentlength>2048</d:getcontentlength>
      <d:getlastmodified>Fri, 13 Feb 2009 23:31:30 GMT</d:getlastmodified>
      <d:getetag>"abc"</d:getetag>
      <oc:fileid>42</oc:fileid>
      <oc:permissions>RGDNVW</oc:permissions>
      <d:resourcetype/>
    </d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/remote.php/dav/files/alice/Sub%20Folder/</d:href>
    <d:propstat><d:prop>
      <d:resourcetype><d:collection/></d:resourcetype>
      <d:getetag>"def"</d:getetag>
      <oc:fileid>7</oc:fileid>
    </d:prop></d:propstat>
  </d:response>
</d:multistatus>"#;

    #[test]
    fn percent_decode_survives_multibyte_after_percent() {
        // A `%` followed within two bytes by a multi-byte UTF-8 character used
        // to byte-slice the &str mid-character and panic.
        assert_eq!(percent_decode("%aä"), "%aä");
        assert_eq!(percent_decode("%ä"), "%ä");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("Sub%20Folder"), "Sub Folder");
        assert_eq!(percent_decode("f%C3%BCr"), "für");
    }

    #[test]
    fn empty_properties_do_not_capture_sibling_text() {
        // A 404 propstat serializing an empty property as
        // `<d:getetag></d:getetag>` (Start+End with no text — XML-equivalent to
        // the self-closing form) must not write the following `<d:status>`
        // text into the still-armed field.
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:response>
    <d:href>/remote.php/dav/files/alice/Notes.txt</d:href>
    <d:propstat>
      <d:prop><d:getetag>"abc"</d:getetag></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
    <d:propstat>
      <d:prop><d:getetag></d:getetag></d:prop>
      <d:status>HTTP/1.1 404 Not Found</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let base = "https://cloud.example.org/remote.php/dav/files/alice";
        let entries = parse_multistatus(xml, base).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].etag, "abc",
            "status text must not leak into the etag"
        );
    }

    #[test]
    fn slice_full_body_clamps_to_the_requested_window() {
        let body = bytes::Bytes::from_static(b"0123456789");
        // Middle window.
        assert_eq!(slice_full_body(body.clone(), 2, 3).as_ref(), b"234");
        // Window reaching past EOF is clamped.
        assert_eq!(slice_full_body(body.clone(), 8, 100).as_ref(), b"89");
        // Start past EOF yields an empty read (EOF), not a panic.
        assert!(slice_full_body(body.clone(), 100, 10).is_empty());
        // Whole file.
        assert_eq!(slice_full_body(body, 0, 10).as_ref(), b"0123456789");
    }

    #[test]
    fn parses_children_excluding_self() {
        let base = "https://cloud.example.org/remote.php/dav/files/alice";
        let entries = parse_multistatus(SAMPLE, base).unwrap();
        assert_eq!(
            entries.len(),
            2,
            "the directory itself must not be included"
        );

        let file = &entries[0];
        assert_eq!(file.path, "Notes.txt");
        assert!(!file.is_dir);
        assert_eq!(file.size, 2048);
        assert_eq!(file.etag, "abc");
        assert_eq!(file.file_id, Some(42));
        assert_eq!(file.mtime, 1_234_567_890, "getlastmodified must be parsed");
        assert_eq!(
            file.permissions, "RGDNVW",
            "oc:permissions must be captured"
        );

        let dir = &entries[1];
        assert_eq!(dir.path, "Sub Folder", "percent decoding must take effect");
        assert!(dir.is_dir);
    }

    #[test]
    fn parses_rfc1123_dates() {
        // Classic reference instant.
        assert_eq!(
            parse_http_date("Fri, 13 Feb 2009 23:31:30 GMT"),
            1_234_567_890
        );
        // The Unix epoch itself.
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), 0);
        // A leap day, to exercise the civil-date maths.
        assert_eq!(
            parse_http_date("Sat, 29 Feb 2020 00:00:00 GMT"),
            1_582_934_400
        );
        // Malformed input degrades to 0, never panics.
        assert_eq!(parse_http_date("not a date"), 0);
        assert_eq!(parse_http_date(""), 0);
    }

    #[test]
    fn url_for_percent_encodes_segments() {
        let dav = WebDavClient::new(
            reqwest::Client::new(),
            "https://cloud.example.org",
            "alice",
            "pw",
        );
        // A file with a space in a parent segment.
        let file = dav.url_for("Sub Folder/notes.txt", false).unwrap();
        assert_eq!(
            file.as_str(),
            "https://cloud.example.org/remote.php/dav/files/alice/Sub%20Folder/notes.txt"
        );
        assert!(
            !file.as_str().contains(' '),
            "no literal space may reach the wire"
        );

        // A directory gets the trailing slash of a collection.
        let dir = dav.url_for("Sub Folder", true).unwrap();
        assert_eq!(
            dir.as_str(),
            "https://cloud.example.org/remote.php/dav/files/alice/Sub%20Folder/"
        );
    }
}
