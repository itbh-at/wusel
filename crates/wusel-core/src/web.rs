// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Nextcloud *web* URLs for an object, built from its stable file id.
//!
//! These are what a file manager's "open in Nextcloud" / "copy internal link"
//! actions point at — the browser side, not WebDAV. Two shapes, both verified
//! against a live Nextcloud 34:
//!
//! * `…/index.php/f/<id>` — the object's canonical link. On a **file** it opens
//!   the web viewer/editor; on a **folder** it navigates *into* the folder. It
//!   is id-based, so it survives a rename or move — which is exactly why it is
//!   also the "internal link" a user copies to paste elsewhere.
//! * `…/index.php/apps/files/files/<id>?dir=<parent>` — the Files app opened at
//!   the object's **parent**, with the object highlighted. This "reveal in its
//!   folder" is the only shape that needs the path, and so the only place a name
//!   is percent-encoded here.
//!
//! The id-only shape needs nothing escaped (a decimal id and a fixed path); the
//! reveal shape carries a real folder path — spaces, `&`, parentheses, umlauts —
//! so its one query value is percent-encoded by hand (see [`encode_dir`]) rather
//! than pulling a URL-builder in for a single string.

/// The canonical, rename-proof link to a file or folder: opens a file in the
/// web viewer, navigates into a folder, and is the "internal link" to copy.
///
/// `server_url` is the instance base (e.g. `https://cloud.example.org`); a
/// trailing slash is tolerated. `/index.php/` is spelled out so the link works
/// on instances without URL rewriting as well as pretty-URL ones.
pub fn object_url(server_url: &str, file_id: u64) -> String {
    format!("{}/index.php/f/{file_id}", server_url.trim_end_matches('/'))
}

/// Reveal an object in the web Files app: its **parent** folder, with the object
/// highlighted. `parent` is the account-relative parent path — `/` at the root,
/// otherwise a rooted path like `/Docs/Reports`.
pub fn reveal_url(server_url: &str, file_id: u64, parent: &str) -> String {
    format!(
        "{}/index.php/apps/files/files/{file_id}?dir={}",
        server_url.trim_end_matches('/'),
        encode_dir(parent),
    )
}

/// Percent-encode a folder path for use as the `dir` query value.
///
/// Everything outside RFC 3986's *unreserved* set is escaped, so a space becomes
/// `%20` (never `+` — form-encoding's space, which Nextcloud would read as a
/// literal `+` in the path), and a name's `&`, parentheses and umlaut bytes are
/// escaped too. The path separator `/` is kept readable, which is the exact form
/// Nextcloud's own Files app emits. UTF-8 is escaped byte by byte, which is what
/// a percent-encoded URL requires.
fn encode_dir(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &b in path.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => {
                use std::fmt::Write;
                // Uppercase hex, two digits — the canonical percent-encoding.
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_url_is_the_id_only_index_php_link() {
        assert_eq!(
            object_url("https://cloud.example.org", 192),
            "https://cloud.example.org/index.php/f/192"
        );
        // A trailing slash on the base must not double up.
        assert_eq!(
            object_url("https://cloud.example.org/", 192),
            "https://cloud.example.org/index.php/f/192"
        );
    }

    #[test]
    fn reveal_url_targets_the_parent_and_carries_the_id() {
        assert_eq!(
            reveal_url("https://cloud.example.org", 192, "/"),
            "https://cloud.example.org/index.php/apps/files/files/192?dir=/"
        );
        assert_eq!(
            reveal_url("https://cloud.example.org", 7, "/Docs/Reports"),
            "https://cloud.example.org/index.php/apps/files/files/7?dir=/Docs/Reports"
        );
    }

    #[test]
    fn reveal_url_percent_encodes_the_nasty_bits_but_keeps_slashes() {
        // A real-world path: spaces, an ampersand, parentheses, a German umlaut.
        let url = reveal_url(
            "https://cloud.example.org",
            42,
            "/OpenProject/ITBH Integration & Automation Hub (11)/Präsentation",
        );
        assert_eq!(
            url,
            "https://cloud.example.org/index.php/apps/files/files/42?dir=\
             /OpenProject/ITBH%20Integration%20%26%20Automation%20Hub%20%2811%29/Pr%C3%A4sentation"
        );
        // The separators stay readable; the ampersand must be escaped so it
        // cannot start another query parameter.
        assert!(url.contains("/OpenProject/ITBH%20Integration%20%26%20"));
        assert!(!url.contains('+'), "a space must be %20, never +");
    }
}
