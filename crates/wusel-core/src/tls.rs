// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! One place to build the HTTP(S) client, so TLS trust is configured exactly
//! once and applies to everything: WebDAV, OCS, Login, and notify_push (which
//! rides the same reqwest client via `reqwest-websocket`).
//!
//! Trust policy, from safest to loosest:
//! * **default** — the OS trust store (`rustls-tls-native-roots`), i.e. the same
//!   certificates a browser or `curl` trusts. Public and enterprise CAs already
//!   installed system-wide just work.
//! * **`ca_cert`** — additionally trust a private CA (or a self-signed server
//!   cert) from a PEM file, without touching the OS store. The clean path for
//!   self-hosters.
//! * **`insecure`** — turn certificate verification off entirely. A testing-only
//!   escape hatch; the daemon warns loudly when it is on.

use crate::config::TlsSettings;
use crate::{Error, Result};

/// Builds a reqwest client honouring the TLS settings. Used for every HTTP call
/// and, through `reqwest-websocket`, for the notify_push WebSocket too.
pub fn client(settings: &TlsSettings) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent("wusel")
        // Fail fast if the server is unreachable instead of hanging a FUSE op.
        .connect_timeout(std::time::Duration::from_secs(15))
        // Keep pooled connections warm at the TCP layer …
        .tcp_keepalive(std::time::Duration::from_secs(30))
        // … and never *reuse* one that has idled long enough for a reverse proxy
        // (nginx/Apache in front of Nextcloud) to have closed it: a stale reuse
        // surfaces as the opaque "error sending request for url …" and breaks
        // reads/PROPFINDs. Well under the usual 60–75 s server keep-alive.
        .pool_idle_timeout(std::time::Duration::from_secs(20));
    // NOTE: deliberately no whole-request `.timeout()` — it would abort long,
    // legitimate uploads/downloads. `connect_timeout` bounds only the handshake.

    if settings.insecure {
        // No chain, hostname, or expiry checks. Only for trusted networks/tests.
        builder = builder.danger_accept_invalid_certs(true);
    } else if let Some(path) = &settings.ca_cert {
        let pem = std::fs::read(path)
            .map_err(|e| Error::Other(format!("cannot read ca_cert {}: {e}", path.display())))?;
        // A PEM file may hold a whole chain; trust every certificate in it.
        let certs = reqwest::Certificate::from_pem_bundle(&pem)
            .map_err(|e| Error::Other(format!("invalid ca_cert {}: {e}", path.display())))?;
        if certs.is_empty() {
            return Err(Error::Other(format!(
                "no certificates found in ca_cert {}",
                path.display()
            )));
        }
        for cert in certs {
            builder = builder.add_root_certificate(cert);
        }
    }

    builder
        .build()
        .map_err(|e| Error::Other(format!("could not build HTTP client: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway self-signed CA, only to exercise the PEM → client path.
    const TEST_CA: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDFTCCAf2gAwIBAgIUGRdtmyC4kK5P2Piaxy3P5n74xyMwDQYJKoZIhvcNAQEL\n\
BQAwGjEYMBYGA1UEAwwPbmMtc3luYy10ZXN0LWNhMB4XDTI2MDcyMTIwMTkwNFoX\n\
DTM2MDcxODIwMTkwNFowGjEYMBYGA1UEAwwPbmMtc3luYy10ZXN0LWNhMIIBIjAN\n\
BgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEApMX7vgr6l9Eypo8QY6Q153YeRFKz\n\
/SM1kJKvB6MKD+LqreEGRCwzd9XfCJTAn5VE0Evfbvn3eP5Bhn0+UORWJ/Tby6Tz\n\
c5B6XZ0hBaiEdPM1TibM0a6uieqRJpBd6y0Bg4tJldUoDnmSF9HT0H9xGxBZMT9p\n\
0sCv3LgNja+CeS+d5eJTbynUTsAUgLouXJs24U0ws72joR07YIll/NubgekPwXc6\n\
JZNHvrTsT3IaF70XMwFRblS9UmsYlT1nM+9U2LYmadvREUtbLqHJal8fkUXWituj\n\
wDmILux/TZ/bgqXg7JEC3xUa7W59TyPhcmIB1Ha/SxwFNdFD4qG6zwylKwIDAQAB\n\
o1MwUTAdBgNVHQ4EFgQUG0Grqx/POS1vEeDDEpDb8ov+ELswHwYDVR0jBBgwFoAU\n\
G0Grqx/POS1vEeDDEpDb8ov+ELswDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0B\n\
AQsFAAOCAQEAmoqFg3tD05qx1UbGV/YzhgWs+5ROdzGZz2Eo5+lwpQpX7pGcLTaR\n\
CVT6vBxADmr42WaYUWGiF4QYtRhi/6D5CZulvoHSzVhlw8fjgyAnxg+jbjrhsbvF\n\
iC7SX/zItpDEAi3iNd4P9WG/uZpK5BdGKdkc+jh6QVk4tac/5DqodkOpEsyxUuk4\n\
0wsHz9Oxf1Y11iVKkSQfbJKCmkH05GP7JDdMm2Lp3H6e0JQQaViv15O3A5pWBZAM\n\
7LwuQ+IPxLXS5Ampl7VEjOkLvJhkytvlALDq3IsfMUZwkLqgEa3KpdQ5ENAhfOF2\n\
qnQQxRg5qa8Lv+Jzwh67J/aQPC+bydnLZA==\n\
-----END CERTIFICATE-----\n";

    #[test]
    fn default_and_insecure_build() {
        assert!(client(&TlsSettings::default()).is_ok(), "OS-store client");
        let insecure = TlsSettings {
            ca_cert: None,
            insecure: true,
        };
        assert!(client(&insecure).is_ok(), "insecure client");
    }

    #[test]
    fn custom_ca_is_loaded() {
        let path = std::env::temp_dir().join(format!("wusel-ca-{}.pem", std::process::id()));
        std::fs::write(&path, TEST_CA).unwrap();
        let s = TlsSettings {
            ca_cert: Some(path.clone()),
            insecure: false,
        };
        assert!(client(&s).is_ok(), "a valid CA PEM must be accepted");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_or_invalid_ca_errors() {
        let missing = TlsSettings {
            ca_cert: Some("/no/such/ca.pem".into()),
            insecure: false,
        };
        assert!(client(&missing).is_err(), "unreadable ca_cert must fail");

        let bad = std::env::temp_dir().join(format!("wusel-badca-{}.pem", std::process::id()));
        std::fs::write(&bad, b"not a certificate").unwrap();
        let s = TlsSettings {
            ca_cert: Some(bad.clone()),
            insecure: false,
        };
        assert!(client(&s).is_err(), "garbage ca_cert must fail");
        std::fs::remove_file(&bad).ok();
    }
}
