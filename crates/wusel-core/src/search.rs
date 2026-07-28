// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Nextcloud **Unified Search** (OCS) — a text query answered by the server's
//! own search providers. The desktop search integration uses this so the desktop
//! searches Nextcloud *directly* (including server-side full text, if the
//! `fulltextsearch` app is present) instead of a local indexer walking the mount.
//!
//! Endpoint: `GET {server}/ocs/v2.php/search/providers/files/search?term=…` with
//! the `OCS-APIRequest: true` header and Basic auth. We parse the JSON leniently
//! (fields vary by server version) — see [`parse_entries`].

use crate::Result;

/// One search result. `rel_path` is the file's account-relative path when we can
/// derive it (so the caller can open the local mount copy); otherwise the caller
/// falls back to `resource_url` (the Nextcloud web link).
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub subline: String,
    pub resource_url: String,
    pub rel_path: Option<String>,
}

/// Query the files search provider. Async so the caller controls the runtime.
pub async fn unified_search(
    http: &reqwest::Client,
    server: &str,
    login: &str,
    app_password: &str,
    term: &str,
) -> Result<Vec<SearchHit>> {
    let url = format!(
        "{}/ocs/v2.php/search/providers/files/search",
        server.trim_end_matches('/')
    );
    let text = http
        .get(&url)
        .basic_auth(login, Some(app_password))
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .query(&[("term", term), ("limit", "25")])
        // Unified search can be slow on a large instance; bound the wait so a
        // per-keystroke search provider returns (empty) instead of hanging the
        // desktop's D-Bus call. The client otherwise has no request timeout.
        .timeout(std::time::Duration::from_secs(12))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(parse_entries(&text))
}

/// Parse `ocs.data.entries[]` leniently: take `title`/`subline`/`resourceUrl`,
/// and derive a local-relative path from `attributes.path` when present (recent
/// Nextcloud files results carry it). Unknown shapes yield no hits rather than an
/// error — a malformed page must never crash the search provider.
fn parse_entries(json: &str) -> Vec<SearchHit> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(entries) = v.pointer("/ocs/data/entries").and_then(|e| e.as_array()) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|e| {
            let title = e.get("title")?.as_str()?.to_string();
            let str_field = |k: &str| e.get(k).and_then(|s| s.as_str()).unwrap_or("").to_string();
            Some(SearchHit {
                title,
                subline: str_field("subline"),
                resource_url: str_field("resourceUrl"),
                rel_path: derive_rel_path(e),
            })
        })
        .collect()
}

/// The account-relative path for opening the local copy, if the entry exposes it.
/// Prefers `attributes.path`; that is the reliable source, so we do not guess
/// from `subline` (a human-facing context string) or parse `resourceUrl`.
fn derive_rel_path(entry: &serde_json::Value) -> Option<String> {
    let p = entry.pointer("/attributes/path").and_then(|p| p.as_str())?;
    let p = p.trim_start_matches('/');
    (!p.is_empty()).then(|| p.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entries_and_derives_path() {
        let json = r#"{"ocs":{"meta":{},"data":{"name":"Files","entries":[
            {"title":"notes.md","subline":"/Work","resourceUrl":"/f/42",
             "attributes":{"path":"/Work/notes.md","fileId":"42"}},
            {"title":"no-path.txt","subline":"x","resourceUrl":"/f/7"}
        ]}}}"#;
        let hits = parse_entries(json);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "notes.md");
        assert_eq!(hits[0].rel_path.as_deref(), Some("Work/notes.md"));
        assert_eq!(hits[0].resource_url, "/f/42");
        assert_eq!(hits[1].rel_path, None, "no attributes.path → web fallback");
    }

    #[test]
    fn malformed_json_yields_no_hits() {
        assert!(parse_entries("not json").is_empty());
        assert!(parse_entries(r#"{"ocs":{"data":{}}}"#).is_empty());
    }
}
