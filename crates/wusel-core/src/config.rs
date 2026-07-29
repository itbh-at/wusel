// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Configuration and default paths.
//!
//! We follow the XDG Base Directory spec, so state lands where users (and their
//! backups) expect it, and each kind of data goes to the right place:
//!
//! * `~/.config/wusel/`      — configuration and the credentials fallback
//! * `~/.local/state/wusel/` — the SQLite metadata state (regenerable from the server)
//! * `~/.cache/wusel/`       — hydrated file blobs (regenerable)
//!
//! Each honours its `XDG_*_HOME` override. The app password lives in the OS
//! keyring by default; the `0600` file under the config dir is the fail-soft
//! fallback (and the opt-out, `[auth] keyring = false`).

use std::path::PathBuf;

/// Runtime configuration of a mount.
#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL of the Nextcloud instance, e.g. `https://cloud.example.org`.
    pub server_url: String,
    /// User name (set after a successful login).
    pub login_name: Option<String>,
    /// App password/token from Login Flow v2.
    pub app_password: Option<String>,
    /// Directory for the metadata DB and configuration.
    pub state_dir: PathBuf,
    /// Directory for hydrated file blobs (content cache).
    pub cache_dir: PathBuf,
    /// Target mountpoint.
    pub mountpoint: PathBuf,
}

impl Config {
    /// Per-host state dir (for later multi-account use).
    pub fn default_state_dir(host: &str) -> PathBuf {
        state_dir().join(sanitize(host))
    }

    pub fn db_path(&self) -> PathBuf {
        self.state_dir.join("state.sqlite")
    }
}

// --- XDG base directories (the default account; see `Account`) --------------

/// `$XDG_CONFIG_HOME/wusel` or `~/.config/wusel`.
pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("wusel")
}

/// `$XDG_STATE_HOME/wusel` or `~/.local/state/wusel`.
///
/// State (regenerable from the server) belongs in `XDG_STATE_HOME`, not
/// `XDG_DATA_HOME` — the latter is for genuine user data, which we do not have.
pub fn state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state").join("wusel")
}

/// `$XDG_CACHE_HOME/wusel` or `~/.cache/wusel`.
pub fn cache_dir() -> PathBuf {
    xdg_dir("XDG_CACHE_HOME", ".cache").join("wusel")
}

/// The configuration file (settings).
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Where `login` records credentials and `mount` reads them. Holds the app
/// password when the keyring is opted out or unavailable; otherwise just the
/// non-secret metadata (server + login name), with the secret in the keyring.
pub fn credentials_path() -> PathBuf {
    config_dir().join("credentials.json")
}

/// SQLite metadata DB for the default account.
pub fn state_db_path() -> PathBuf {
    state_dir().join("state.sqlite")
}

/// Resolves an XDG base dir: the `$XDG_*_HOME` override if set, else `~/<default>`.
/// Deliberately without an external `dirs` crate, to keep dependencies lean.
fn xdg_dir(env: &str, default_rel: &str) -> PathBuf {
    if let Ok(val) = std::env::var(env) {
        if !val.is_empty() {
            return PathBuf::from(val);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(default_rel)
}

/// Replaces characters that cause problems in paths (e.g. `https://`).
fn sanitize(host: &str) -> String {
    host.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// --- Accounts (optional named profiles) -------------------------------------

/// The implicit account, whose data lives directly in the base dirs — so a
/// single-account user never sees any profile machinery.
pub const DEFAULT_ACCOUNT: &str = "default";

/// One Nextcloud account. The `default` account uses the base XDG dirs
/// (`~/.config/wusel`, …); named accounts are opt-in and live under an
/// `accounts/<name>/` subdirectory, each fully isolated (credentials, state,
/// cache, settings, mountpoint).
#[derive(Debug, Clone)]
pub struct Account {
    name: String,
}

impl Account {
    /// An account by name; an empty or `"default"` name is the default account.
    /// The name is sanitized, so it is always a safe single path segment.
    pub fn new(name: &str) -> Self {
        let name = sanitize(name.trim());
        let name = if name.is_empty() {
            DEFAULT_ACCOUNT.to_string()
        } else {
            name
        };
        Self { name }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_default(&self) -> bool {
        self.name == DEFAULT_ACCOUNT
    }

    /// Layer a per-account subdir onto a base dir (default → the base itself).
    fn scope(&self, base: PathBuf) -> PathBuf {
        if self.is_default() {
            base
        } else {
            base.join("accounts").join(&self.name)
        }
    }

    pub fn config_dir(&self) -> PathBuf {
        self.scope(config_dir())
    }
    pub fn state_dir(&self) -> PathBuf {
        self.scope(state_dir())
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.scope(cache_dir())
    }
    pub fn credentials_path(&self) -> PathBuf {
        self.config_dir().join("credentials.json")
    }
    pub fn config_path(&self) -> PathBuf {
        self.config_dir().join("config.toml")
    }
    pub fn state_db_path(&self) -> PathBuf {
        self.state_dir().join("state.sqlite")
    }
    /// Directory for this account's hydrated blobs.
    pub fn blob_cache_dir(&self) -> PathBuf {
        self.cache_dir().join("blobs")
    }
    /// This account's settings (its own `config.toml`), with defaults.
    pub fn settings(&self) -> Settings {
        load_settings_from(&self.config_path())
    }

    /// Where this account mounts when `mount` is given no path and `config.toml`
    /// sets none: `~/Wusel` for the default account, `~/Wusel-<name>` for a named
    /// one (the brand is the folder; Nextcloud is what lives inside it).
    pub fn default_mountpoint(&self) -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let dir = if self.is_default() {
            "Wusel".to_string()
        } else {
            format!("Wusel-{}", self.name)
        };
        PathBuf::from(home).join(dir)
    }
}

/// Names of the accounts that have stored credentials: the default account (if
/// present) plus every `accounts/<name>/` with a `credentials.json`.
pub fn list_accounts() -> Vec<String> {
    let mut names = Vec::new();
    if credentials_path().exists() {
        names.push(DEFAULT_ACCOUNT.to_string());
    }
    if let Ok(entries) = std::fs::read_dir(config_dir().join("accounts")) {
        for entry in entries.flatten() {
            if entry.path().join("credentials.json").exists() {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

// --- Settings (config.toml) -------------------------------------------------

/// Resolved settings from `~/.config/wusel/config.toml`, with defaults.
///
/// ```toml
/// [cache]
/// max_size = "5GiB"  # blob budget (binary units; "GB" reads as GiB); "0"/"unlimited" = no limit
/// max_age  = "30d"   # drop blobs unused this long; "0" = never
/// [sync]
/// revalidate_secs = 30   # re-list a directory older than this (no-push fallback)
/// push_floor_secs = 5    # min interval between push-triggered re-lists of a dir
/// text_merge = false     # opt-in: 3-way-merge text on conflict (else a copy)
/// ignore_patterns = [".*.sw?", "~$*", …]  # kept local; REPLACES the default set
/// [tls]
/// ca_cert  = "/etc/wusel/my-ca.pem"  # extra trusted CA (self-hosted certs)
/// insecure = false                     # true = disable TLS verification (DANGER)
/// [auth]
/// keyring = true     # default; false = keep the app password in the 0600 file
/// [mount]
/// point = "/home/you/Wusel"            # where `mount` (with no path) attaches
/// [desktop]
/// exclude_from_indexers = true         # default; false = allow GNOME Tracker to index
/// ```
#[derive(Debug, Clone)]
pub struct Settings {
    pub revalidate_secs: u64,
    /// Rate-limit for push-triggered revalidation: a directory invalidated by a
    /// notify_push event is re-listed on next access only if its last listing is
    /// older than this. It collapses a burst of events (and a background indexer
    /// hammering the same directory) into at most one PROPFIND per window, while
    /// still reacting to real changes within a few seconds.
    pub push_floor_secs: u64,
    pub cache_max_bytes: Option<u64>,
    pub cache_max_age_secs: Option<u64>,
    pub tls: TlsSettings,
    /// Mountpoint override; falls back to the account's default mountpoint.
    pub mount_point: Option<PathBuf>,
    /// Opt-in: on an upload conflict, try a 3-way text merge before falling back
    /// to a conflict copy. Off by default — the reference client always makes a
    /// conflict copy.
    pub text_merge: bool,
    /// Glob patterns (matched on the basename) for ephemeral editor/OS files that
    /// are kept purely local and never uploaded. Defaults to
    /// [`crate::ignore::DEFAULT_IGNORE_PATTERNS`]; a config value replaces it.
    pub ignore_patterns: Vec<String>,
    /// Keep the app password in the OS keyring rather than the `0600` file.
    /// **On by default** — more secure, and fail-soft: if the keyring cannot be
    /// written and verified at login, the secret transparently stays in the file
    /// (see [`crate::credentials`]). Set `false` to opt out and always use the file
    /// (e.g. headless servers where no Secret Service is unlocked).
    pub auth_keyring: bool,
    /// Hide the mount from desktop file indexers. When on, the FUSE root exposes
    /// synthetic, local-only `.trackerignore`/`.nomedia` marker files (never
    /// uploaded to Nextcloud) that GNOME Tracker/LocalSearch honours to skip the
    /// whole tree. **On by default**: since opening a file caches it, an indexer
    /// walking the mount would hydrate *everything* in the background — a traffic
    /// storm. Set `false` to opt back in to indexing. (KDE Baloo ignores markers
    /// — it needs a config-based exclude, a later addition.)
    pub exclude_from_indexers: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            revalidate_secs: 30,
            push_floor_secs: 5,
            cache_max_bytes: Some(5 * 1024 * 1024 * 1024), // 5 GiB
            cache_max_age_secs: None,
            tls: TlsSettings::default(),
            mount_point: None,
            text_merge: false,
            ignore_patterns: crate::ignore::DEFAULT_IGNORE_PATTERNS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            auth_keyring: true,
            exclude_from_indexers: true,
        }
    }
}

/// TLS trust configuration. By default we validate against the OS trust store
/// (the same certificates a browser or `curl` uses). Self-hosters with a private
/// CA can either add it to the OS store or point `ca_cert` at its PEM. `insecure`
/// turns verification off entirely — a testing-only escape hatch.
#[derive(Debug, Clone, Default)]
pub struct TlsSettings {
    /// Extra CA (or self-signed server cert) to trust, as a PEM file path.
    pub ca_cert: Option<std::path::PathBuf>,
    /// Disable certificate verification. Dangerous; logged loudly on start.
    pub insecure: bool,
}

/// Load the default account's settings.
pub fn load_settings() -> Settings {
    load_settings_from(&config_path())
}

/// Load settings from a specific config file, falling back to defaults (and
/// warning on a malformed file rather than failing the mount).
fn load_settings_from(path: &std::path::Path) -> Settings {
    match std::fs::read_to_string(path) {
        Ok(text) => match parse_settings(&text) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(%e, "config.toml is malformed — using defaults");
                Settings::default()
            }
        },
        Err(_) => Settings::default(),
    }
}

#[derive(serde::Deserialize, Default)]
struct RawConfig {
    #[serde(default)]
    cache: RawCache,
    #[serde(default)]
    sync: RawSync,
    #[serde(default)]
    tls: RawTls,
    #[serde(default)]
    mount: RawMount,
    #[serde(default)]
    auth: RawAuth,
    #[serde(default)]
    desktop: RawDesktop,
}
#[derive(serde::Deserialize, Default)]
struct RawCache {
    max_size: Option<String>,
    max_age: Option<String>,
}
#[derive(serde::Deserialize, Default)]
struct RawSync {
    revalidate_secs: Option<u64>,
    push_floor_secs: Option<u64>,
    text_merge: Option<bool>,
    ignore_patterns: Option<Vec<String>>,
}
#[derive(serde::Deserialize, Default)]
struct RawTls {
    ca_cert: Option<String>,
    insecure: Option<bool>,
}
#[derive(serde::Deserialize, Default)]
struct RawMount {
    point: Option<String>,
}
#[derive(serde::Deserialize, Default)]
struct RawAuth {
    keyring: Option<bool>,
}
#[derive(serde::Deserialize, Default)]
struct RawDesktop {
    exclude_from_indexers: Option<bool>,
}

fn parse_settings(text: &str) -> crate::Result<Settings> {
    let raw: RawConfig =
        toml::from_str(text).map_err(|e| crate::Error::Other(format!("config.toml: {e}")))?;
    let mut s = Settings::default();
    if let Some(v) = raw.sync.revalidate_secs {
        s.revalidate_secs = v;
    }
    if let Some(v) = raw.sync.push_floor_secs {
        s.push_floor_secs = v;
    }
    if let Some(v) = raw.sync.ignore_patterns {
        s.ignore_patterns = v; // an explicit list REPLACES the built-in default
    }
    s.text_merge = raw.sync.text_merge.unwrap_or(false);
    s.auth_keyring = raw.auth.keyring.unwrap_or(true);
    // A malformed value must NOT silently mean "unlimited"/"never" (fail-open —
    // a typo like "5G" would quietly disable the cache budget). Warn, naming the
    // bad value, and keep the documented default instead.
    if let Some(sz) = raw.cache.max_size {
        match parse_size(&sz) {
            Ok(v) => s.cache_max_bytes = v,
            Err(()) => tracing::warn!(
                value = %sz,
                "cache.max_size is malformed (expected e.g. \"5GiB\", \"500MiB\", \"0\") — using the default"
            ),
        }
    }
    if let Some(age) = raw.cache.max_age {
        match parse_duration(&age) {
            Ok(v) => s.cache_max_age_secs = v,
            // The default here IS "never expire" (None) — the warning is the
            // point: the user asked for an age bound and did not get one.
            Err(()) => tracing::warn!(
                value = %age,
                "cache.max_age is malformed (expected e.g. \"30d\", \"12h\", \"0\") — using the default"
            ),
        }
    }
    s.tls.ca_cert = raw.tls.ca_cert.filter(|p| !p.is_empty()).map(Into::into);
    s.tls.insecure = raw.tls.insecure.unwrap_or(false);
    s.mount_point = raw
        .mount
        .point
        .filter(|p| !p.is_empty())
        .map(|p| expand_tilde(&p));
    s.exclude_from_indexers = raw.desktop.exclude_from_indexers.unwrap_or(true);
    Ok(s)
}

/// Expand a leading `~/` (and a bare `~`) against `$HOME`.
///
/// A config file is hand-written, and `point = "~/Wusel"` is the spelling every
/// user reaches for first. A shell expands it before the program ever sees it;
/// a TOML file does not, so without this the daemon would faithfully create a
/// directory *named* `~` in whatever the current working directory happens to
/// be and mount there — technically obedient, never what was meant. Only a
/// leading tilde is special: `~` elsewhere in a path is a legal filename
/// character and stays untouched.
fn expand_tilde(p: &str) -> PathBuf {
    let Some(rest) = p.strip_prefix('~') else {
        return PathBuf::from(p);
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return PathBuf::from(p); // `~user/…` — not ours to resolve
    }
    match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        Some(home) => PathBuf::from(home).join(rest.trim_start_matches('/')),
        None => PathBuf::from(p),
    }
}

/// `"5GiB"` → bytes; `"0"`/`"unlimited"`/empty → `Ok(None)` (no limit);
/// anything unparsable → `Err(())` so the caller can fall back to the default
/// instead of failing open. The multipliers are **binary** (1 GiB = 2³⁰), so
/// the IEC spellings (`GiB`, `MiB`, …) are canonical; the plain `GB`/`MB`
/// spellings are accepted as aliases for the same binary sizes.
///
/// A value whose byte count does not fit in a `u64` (`"17000000TiB"`) is treated
/// as malformed too: the multiplication is **checked**, because an unchecked one
/// panics in a debug build and — worse — silently wraps to a tiny budget in a
/// release build, which is exactly the fail-open behaviour this parser exists to
/// prevent.
fn parse_size(s: &str) -> Result<Option<u64>, ()> {
    let s = s.trim();
    if s.is_empty() || s == "0" || s.eq_ignore_ascii_case("unlimited") {
        return Ok(None);
    }
    let up = s.to_ascii_uppercase();
    // Longer suffixes first, or "5GIB".strip_suffix("B") would leave "5GI".
    let (num, mult) = [
        ("TIB", 1u64 << 40),
        ("GIB", 1 << 30),
        ("MIB", 1 << 20),
        ("KIB", 1 << 10),
        ("TB", 1 << 40),
        ("GB", 1 << 30),
        ("MB", 1 << 20),
        ("KB", 1 << 10),
        ("B", 1),
    ]
    .into_iter()
    .find_map(|(suf, m)| up.strip_suffix(suf).map(|n| (n, m)))
    .unwrap_or((up.as_str(), 1));
    num.trim()
        .parse::<u64>()
        .map_err(drop)
        .and_then(|v| v.checked_mul(mult).ok_or(()))
        .map(Some)
}

/// `"30d"`/`"12h"`/`"90s"` → seconds; `"0"`/empty → `Ok(None)` (never);
/// anything unparsable — including a duration too large for a `u64` of seconds —
/// → `Err(())` (see [`parse_size`] for why the multiplication is checked).
fn parse_duration(s: &str) -> Result<Option<u64>, ()> {
    let s = s.trim();
    if s.is_empty() || s == "0" {
        return Ok(None);
    }
    let (num, mult) = [("d", 86400u64), ("h", 3600), ("m", 60), ("s", 1)]
        .into_iter()
        .find_map(|(suf, m)| s.strip_suffix(suf).map(|n| (n, m)))
        .unwrap_or((s, 1));
    num.trim()
        .parse::<u64>()
        .map_err(drop)
        .and_then(|v| v.checked_mul(mult).ok_or(()))
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_scheme_chars() {
        assert_eq!(
            sanitize("https://cloud.example.org"),
            "https___cloud.example.org"
        );
    }

    #[test]
    fn parses_config_toml() {
        let s = parse_settings(
            "[cache]\nmax_size = \"2GB\"\nmax_age = \"7d\"\n[sync]\nrevalidate_secs = 30\n",
        )
        .unwrap();
        assert_eq!(s.revalidate_secs, 30);
        assert_eq!(s.cache_max_bytes, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(s.cache_max_age_secs, Some(7 * 86400));
    }

    #[test]
    fn empty_config_uses_defaults() {
        let s = parse_settings("").unwrap();
        assert_eq!(s.revalidate_secs, 30);
        assert_eq!(s.push_floor_secs, 5);
        assert!(s.cache_max_bytes.is_some());
        assert_eq!(s.cache_max_age_secs, None);
        // TLS defaults: verify against the OS store, no custom CA.
        assert!(!s.tls.insecure);
        assert!(s.tls.ca_cert.is_none());
        // Indexer exclusion is ON by default (protects against a traffic storm).
        assert!(s.exclude_from_indexers);
        // Keyring credential storage is ON by default (secure by default, fail-soft).
        assert!(s.auth_keyring);
    }

    #[test]
    fn keyring_can_be_opted_out() {
        let s = parse_settings("[auth]\nkeyring = false\n").unwrap();
        assert!(!s.auth_keyring, "[auth] keyring = false must opt out");
    }

    #[test]
    fn desktop_indexing_can_be_opted_back_in() {
        let s = parse_settings("[desktop]\nexclude_from_indexers = false\n").unwrap();
        assert!(!s.exclude_from_indexers);
    }

    #[test]
    fn parses_tls_section() {
        let s =
            parse_settings("[tls]\nca_cert = \"/etc/wusel/ca.pem\"\ninsecure = true\n").unwrap();
        assert_eq!(
            s.tls.ca_cert.as_deref(),
            Some(std::path::Path::new("/etc/wusel/ca.pem"))
        );
        assert!(s.tls.insecure);
    }

    #[test]
    fn unlimited_cache_size() {
        assert_eq!(parse_size("0"), Ok(None));
        assert_eq!(parse_size("unlimited"), Ok(None));
        // Binary units; the IEC spelling and the plain one are the same size.
        assert_eq!(parse_size("500MiB"), Ok(Some(500 * 1024 * 1024)));
        assert_eq!(parse_size("500MB"), Ok(Some(500 * 1024 * 1024)));
    }

    #[test]
    fn malformed_cache_values_fall_back_to_the_defaults() {
        // A typo must not fail open to "unlimited"/"never": the parser reports
        // it (Err) and the settings keep the documented defaults.
        assert_eq!(parse_size("5G"), Err(()));
        assert_eq!(parse_duration("30 days"), Err(()));

        let s = parse_settings("[cache]\nmax_size = \"5G\"\nmax_age = \"30 days\"\n").unwrap();
        assert_eq!(
            s.cache_max_bytes,
            Settings::default().cache_max_bytes,
            "malformed max_size keeps the default budget, not unlimited"
        );
        assert_eq!(
            s.cache_max_age_secs,
            Settings::default().cache_max_age_secs,
            "malformed max_age keeps the default"
        );
    }

    #[test]
    fn absurd_sizes_and_durations_do_not_overflow() {
        // `17000000TiB` does not fit in a u64 of bytes. Multiplying unchecked
        // panics in debug and wraps to a nonsense budget in release — neither is
        // an acceptable answer to a typo in config.toml. Overflow is just another
        // malformed value: report it and keep the documented default.
        assert_eq!(parse_size("17000000TiB"), Err(()));
        assert_eq!(parse_size("18446744073709551615KiB"), Err(()));
        assert_eq!(parse_duration("999999999999999999999d"), Err(()));
        assert_eq!(parse_duration("300000000000000000d"), Err(()));

        let s = parse_settings("[cache]\nmax_size = \"17000000TiB\"\n").unwrap();
        assert_eq!(
            s.cache_max_bytes,
            Settings::default().cache_max_bytes,
            "an overflowing budget keeps the default, it does not wrap"
        );
    }

    #[test]
    fn a_tilde_mountpoint_resolves_against_home() {
        // `point = "~/Wusel"` is the spelling everyone writes by hand. Without
        // expansion the daemon would create a directory literally named `~`.
        let _env = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::var_os("HOME").expect("HOME is set on any test host");
        let home = PathBuf::from(home);

        let s = parse_settings("[mount]\npoint = \"~/Wusel\"\n").unwrap();
        assert_eq!(s.mount_point.unwrap(), home.join("Wusel"));

        // A bare `~` is the home directory itself.
        let s = parse_settings("[mount]\npoint = \"~\"\n").unwrap();
        assert_eq!(s.mount_point.unwrap(), home);

        // An absolute path is untouched, and a tilde that is not the leading
        // path component is a perfectly ordinary filename character.
        for literal in ["/srv/cloud", "/srv/~backup", "~someone/else"] {
            let s = parse_settings(&format!("[mount]\npoint = \"{literal}\"\n")).unwrap();
            assert_eq!(s.mount_point.unwrap(), PathBuf::from(literal));
        }
    }

    #[test]
    fn paths_land_in_the_right_base_dirs() {
        // These read the process-global XDG_* variables, which other tests
        // (provider.rs) mutate — take the crate-wide env lock to avoid racing.
        let _env = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Credentials + config under the config dir; state under the state dir
        // (XDG_STATE_HOME), which must differ from the config dir.
        assert!(credentials_path().starts_with(config_dir()));
        assert!(config_path().starts_with(config_dir()));
        assert!(state_db_path().starts_with(state_dir()));
        assert_ne!(config_dir(), state_dir(), "config and state must differ");
    }

    #[test]
    fn default_account_uses_base_dirs_named_ones_nest() {
        // Reads XDG_*-derived paths — serialize against env-mutating tests.
        let _env = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // The default account is exactly the base layout (no profile machinery).
        let def = Account::new("");
        assert!(def.is_default());
        assert_eq!(def.credentials_path(), credentials_path());
        assert_eq!(def.state_db_path(), state_db_path());
        assert_eq!(def.blob_cache_dir(), cache_dir().join("blobs"));

        // A named account nests under accounts/<name>/ and stays isolated.
        let work = Account::new("Work Client!");
        assert!(!work.is_default());
        assert_eq!(
            work.name(),
            "Work_Client_",
            "name is sanitized to a safe segment"
        );
        assert!(work.config_dir().starts_with(config_dir().join("accounts")));
        assert!(work
            .credentials_path()
            .ends_with("accounts/Work_Client_/credentials.json"));
        assert_ne!(work.state_db_path(), state_db_path());
    }
}
