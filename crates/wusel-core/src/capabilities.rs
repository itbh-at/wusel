// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Nextcloud capability discovery (OCS API).
//!
//! One request to `/ocs/v2.php/cloud/capabilities` tells us two things we care
//! about: the **server version** (a health check — confirms we really are
//! talking to a Nextcloud, and helps diagnostics), and the **notify_push**
//! WebSocket endpoint (if the app is installed). Both are optional: a server
//! without notify_push, or one that does not answer the OCS API, degrades to TTL
//! revalidation rather than failing. See [`crate::push`].

use serde::Deserialize;

use crate::Result;

/// What we learn from the capabilities endpoint.
#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    /// Server version string (e.g. `"29.0.2"`), if reported.
    pub version: Option<String>,
    /// notify_push WebSocket URL, if the app is installed.
    pub push_websocket: Option<String>,
}

/// Fetch the server capabilities. A malformed or missing document yields an
/// empty [`ServerInfo`] rather than an error — the caller decides how loud to be.
pub async fn fetch(
    client: &reqwest::Client,
    server_url: &str,
    login: &str,
    password: &str,
) -> Result<ServerInfo> {
    let url = format!(
        "{}/ocs/v2.php/cloud/capabilities?format=json",
        server_url.trim_end_matches('/')
    );
    let text = client
        .get(&url)
        .basic_auth(login, Some(password))
        // OCS refuses requests without this header (CSRF hardening).
        .header("OCS-APIRequest", "true")
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(parse_server_info(&text))
}

/// Extract version + notify_push endpoint from a capabilities JSON payload.
fn parse_server_info(json: &str) -> ServerInfo {
    match serde_json::from_str::<Root>(json) {
        Ok(root) => ServerInfo {
            version: root.ocs.data.version.map(|v| v.string),
            push_websocket: root
                .ocs
                .data
                .capabilities
                .notify_push
                .and_then(|n| n.endpoints.websocket),
        },
        Err(_) => ServerInfo::default(),
    }
}

// Only the sliver of the (large) capabilities document we care about; unknown
// fields are ignored by serde, so this stays robust across server versions.
#[derive(Deserialize)]
struct Root {
    ocs: Ocs,
}
#[derive(Deserialize)]
struct Ocs {
    data: Data,
}
#[derive(Deserialize)]
struct Data {
    version: Option<RawVersion>,
    capabilities: Capabilities,
}
#[derive(Deserialize)]
struct RawVersion {
    string: String,
}
#[derive(Deserialize)]
struct Capabilities {
    notify_push: Option<NotifyPush>,
}
#[derive(Deserialize)]
struct NotifyPush {
    endpoints: Endpoints,
}
#[derive(Deserialize)]
struct Endpoints {
    websocket: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_version_and_websocket() {
        let json = r#"{"ocs":{"meta":{"status":"ok"},"data":{
          "version":{"major":29,"minor":0,"micro":2,"string":"29.0.2"},
          "capabilities":{"core":{"pollinterval":60},
          "notify_push":{"type":["files"],
          "endpoints":{"websocket":"wss://cloud.example.org/push"}}}}}}"#;
        let info = parse_server_info(json);
        assert_eq!(info.version.as_deref(), Some("29.0.2"));
        assert_eq!(
            info.push_websocket.as_deref(),
            Some("wss://cloud.example.org/push")
        );
    }

    #[test]
    fn version_without_notify_push() {
        let json = r#"{"ocs":{"data":{"version":{"string":"28.0.0"},
          "capabilities":{"core":{"pollinterval":60}}}}}"#;
        let info = parse_server_info(json);
        assert_eq!(info.version.as_deref(), Some("28.0.0"));
        assert!(info.push_websocket.is_none());
    }

    #[test]
    fn garbage_yields_empty() {
        let info = parse_server_info("not json");
        assert!(info.version.is_none() && info.push_websocket.is_none());
    }
}
