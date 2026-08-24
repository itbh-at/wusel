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

/// `$XDG_RUNTIME_DIR/wusel` — the per-user, per-session tmpfs the kernel clears
/// at logout. Home to the diagnostics socket, which is transient and
/// user-private by construction. Falls back to the temp dir only when
/// `XDG_RUNTIME_DIR` is unset, which on a real desktop session it is not.
pub fn runtime_dir() -> PathBuf {
    if let Ok(val) = std::env::var("XDG_RUNTIME_DIR") {
        if !val.is_empty() {
            return PathBuf::from(val).join("wusel");
        }
    }
    std::env::temp_dir().join("wusel")
}

/// The diagnostics socket for a mount, where it serves its state and
/// `wusel doctor` reads it. Keyed on the mountpoint — hashed, so the path stays
/// well inside the 108-byte unix-socket limit however deep the mountpoint —
/// so the mount and a separate `doctor` run derive the same path from the same
/// resolved mountpoint without sharing anything else.
pub fn diag_socket_for_mount(mountpoint: &std::path::Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    // `DefaultHasher::new` has fixed keys — deterministic within a build, which
    // is all that is needed: producer and consumer are the same binary.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    mountpoint.hash(&mut h);
    runtime_dir().join(format!("diag-{:016x}.sock", h.finish()))
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
    /// Where this account's state database actually opens.
    ///
    /// The resolved path, not the nominal one: on a machine with a network home
    /// directory these differ, and a caller that used the nominal path would
    /// address a database nobody is writing to. `cache clear` deleting the wrong
    /// file is the kind of bug that follows from the other arrangement.
    pub fn state_db_path(&self) -> PathBuf {
        self.db_location().path().to_path_buf()
    }

    /// Where the state database goes, and why.
    ///
    /// Resolved on each call rather than cached: it costs one small read of
    /// `/proc/self/mounts`, it happens a handful of times at start-up, and a
    /// cache would have to be invalidated when the account's configuration is
    /// re-read — a subtlety worth more than the microseconds it saves.
    #[must_use]
    pub fn db_location(&self) -> crate::storage::DbLocation {
        crate::storage::resolve(
            self.state_dir().join("state.sqlite"),
            self.settings().db_path,
            crate::storage::fallback_dir(self.name()).map(|d| d.join("state.sqlite")),
            crate::storage::mount_table().as_deref(),
        )
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
/// refresh_pinned = "ask" # manual | ask | auto — what to do when a pinned
///                        # file's server copy has moved on
/// upload = "async"       # async (default): flush returns once the change is
///                        # durable, upload in the background. sync: flush waits
///                        # for the upload and reports its real result.
/// ignore_patterns = [".*.sw?", "~$*", …]  # kept local; REPLACES the default set
/// [tls]
/// ca_cert    = "/etc/wusel/my-ca.pem"  # extra trusted CA (self-hosted certs)
/// insecure   = false                    # true = disable TLS verification (DANGER)
/// http1_only = false                    # true = force HTTP/1.1 (proxies that
///                                        #        mangle HTTP/2 upload bodies)
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
    /// How many FUSE dispatch threads serve kernel requests, and how many worker
    /// threads the engine's runtime uses for concurrent network I/O. Independent
    /// operations (reads, uploads, listings) run in parallel up to this many.
    ///
    /// The default is now several, not one. It used to be 1 — "exactly the
    /// pre-concurrency behaviour, opt in to more" — but that was a mistake on a
    /// real desktop: a file manager listing a folder issues many reads at once
    /// (it sniffs each file's content type), and with one thread they serialise
    /// behind whichever is slowest, so a single slow or stalled read stalls the
    /// whole listing. Concurrency is the point of the engine; making it opt-in
    /// defeated it. Clamped to a sane ceiling on load.
    pub dispatch_threads: usize,
    /// Opt-in: on an upload conflict, try a 3-way text merge before falling back
    /// to a conflict copy. Off by default — the reference client always makes a
    /// conflict copy.
    pub text_merge: bool,
    /// What to do when a pinned file's server copy has moved on. See
    /// [`RefreshPinned`].
    pub refresh_pinned: RefreshPinned,

    /// Whether `flush` waits for the upload (`sync`) or returns as soon as the
    /// change is durable and uploads in the background (`async`, the default).
    pub upload: UploadMode,
    /// What to serve when an out-of-date pinned file is opened. See
    /// [`OpenPinned`].
    pub open_pinned: OpenPinned,
    /// Where the state database goes, overriding both the default location and
    /// the relocation off a network filesystem. An explicit choice is always
    /// honoured — see [`crate::storage`].
    pub db_path: Option<PathBuf>,
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

/// The default dispatch-thread count: the machine's parallelism, floored at 4 so
/// a file manager's burst of reads never serialises, capped at 8 so the worker
/// pools and their SQLite connections stay modest. A user can still set any
/// value in `[mount] dispatch_threads` (clamped to 1..16 on load).
fn default_dispatch_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(4, 8)
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
            dispatch_threads: default_dispatch_threads(),
            text_merge: false,
            refresh_pinned: RefreshPinned::default(),
            upload: UploadMode::default(),
            open_pinned: OpenPinned::default(),
            db_path: None,
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
    /// Force HTTP/1.1 instead of negotiating HTTP/2. A robustness escape hatch:
    /// some reverse proxies mishandle HTTP/2 request bodies for WebDAV chunked
    /// uploads (Nextcloud logs "expected N bytes, got 0") — HTTP/1.1 sidesteps
    /// that entirely. Off by default (HTTP/2 is used where the server offers it).
    pub http1_only: bool,
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
    #[serde(default)]
    state: RawState,
}
#[derive(serde::Deserialize, Default)]
struct RawState {
    db_path: Option<String>,
}
#[derive(serde::Deserialize, Default)]
struct RawCache {
    max_size: Option<String>,
    max_age: Option<String>,
}
/// What to serve when a pinned file is *opened* and the local copy is out of
/// date.
///
/// A different question from [`RefreshPinned`], which decides whether we fetch
/// **unasked, in the background**. Opening a file is asking, and the answer used
/// to be fixed: the local copy no longer matches, so read it live. That is right
/// on a desk and wrong on a train — a pin exists so the file is *there*, and a
/// hotel connection can make "there" cost more than the outdated copy is worth.
///
/// The stale copy is never served silently: whenever it is handed out, the user
/// is told, because an application that opens it and saves produces a conflict
/// nobody saw coming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpenPinned {
    /// Always fetch the current version. What a VFS does by default, and the
    /// behaviour before this setting existed.
    #[default]
    Newest,
    /// Fetch the current version, but not over a metered connection — there,
    /// serve the copy that is already paid for.
    NewestUnmetered,
    /// Always serve the local copy. Bringing it up to date is then a deliberate
    /// act: "Update now", or `wusel update <path>`.
    Offline,
}

/// What [`OpenPinned`] decided for one open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAction {
    /// Read from the server; the local copy is out of date.
    Fetch,
    /// Serve the outdated local copy, and say so.
    ServeLocal,
}

impl OpenPinned {
    /// Decide, given what is known about the connection's cost.
    ///
    /// Unknown metering counts as **metered**, exactly as in
    /// [`RefreshPinned::decide`]: an unknown cost is not a licence to spend, and
    /// somebody who chose `NewestUnmetered` said the connection's cost is what
    /// decides. The rule is the same in both — unknown means do not fetch — and
    /// only the effect of not fetching differs: there it skips a background
    /// update, here it makes the open serve the outdated copy. That is the price
    /// of caution on an unknown line, and the safe direction: stale bytes beat a
    /// surprise bill.
    #[must_use]
    pub fn decide(self, metered: Option<bool>) -> OpenAction {
        match self {
            Self::Newest => OpenAction::Fetch,
            Self::Offline => OpenAction::ServeLocal,
            Self::NewestUnmetered => match metered {
                Some(false) => OpenAction::Fetch,
                Some(true) | None => OpenAction::ServeLocal,
            },
        }
    }

    /// Parse the configured value; `None` for anything unrecognised, so the
    /// caller can warn rather than pick silently.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "newest" | "online" => Some(Self::Newest),
            "newest-unmetered" | "newest_unmetered" | "unmetered" => Some(Self::NewestUnmetered),
            "offline" | "local" => Some(Self::Offline),
            _ => None,
        }
    }
}

/// What happens when a pinned file's server copy has moved on.
///
/// A pin promises "keep this offline". When the server version changes, that
/// promise needs a decision — and it is the user's, because the cost is theirs:
/// the file may be two gigabytes and the link may be a phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefreshPinned {
    /// Show the state and do nothing else. The user picks the moment, with
    /// "Update now".
    Manual,
    /// Ask — one debounced, aggregated notification with an action. The default,
    /// because `manual` is invisible to anyone who does not read emblems and
    /// `auto` spends someone else's bandwidth on their behalf.
    #[default]
    Ask,
    /// Fetch by itself, **but only when it is cheap**: an unmetered connection.
    /// Otherwise it behaves as [`RefreshPinned::Ask`] — pulling two gigabytes
    /// over a mobile link because a colleague touched a file is exactly the harm
    /// this project exists to avoid.
    Auto,
}

/// What to actually do about a pinned file that has gone out of date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshAction {
    /// Nothing beyond the emblem the file already carries.
    ShowOnly,
    /// Tell the user, with an action they can trigger.
    Ask,
    /// Fetch it now, without asking.
    Fetch,
}

impl RefreshPinned {
    /// Decide what to do, given what we know about the connection.
    ///
    /// `metered` is `None` when the answer is unknown, and unknown counts as
    /// *not cheap*: `auto` then behaves as `ask`. The alternative — assuming an
    /// unmetered link — is how a two-gigabyte refresh lands on somebody's phone
    /// plan because a colleague reorganised a shared folder.
    ///
    /// The design also names an idle machine as a condition for `auto`.
    /// Idleness is not detected yet, so it is not part of this decision; only
    /// the cost check is.
    #[must_use]
    pub fn decide(self, metered: Option<bool>) -> RefreshAction {
        match self {
            Self::Manual => RefreshAction::ShowOnly,
            Self::Ask => RefreshAction::Ask,
            Self::Auto => match metered {
                Some(false) => RefreshAction::Fetch,
                Some(true) | None => RefreshAction::Ask,
            },
        }
    }

    /// Parse a configured value. `None` for anything else, so the caller can
    /// warn rather than guess.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "manual" => Some(Self::Manual),
            "ask" => Some(Self::Ask),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

/// When a `flush` is answered relative to the upload it triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UploadMode {
    /// Answer `flush` as soon as the change is durable on local disk, and upload
    /// in the background. The default: a slow or offline server never blocks a
    /// save, and a file manager copying several files does not stall.
    #[default]
    Async,
    /// Hold `flush` until the upload actually lands, and report its real result.
    /// The pre-async behaviour, kept as a fallback for anyone who wants a save to
    /// mean "on the server" before it returns.
    Sync,
}

impl UploadMode {
    /// Parse a configured value; `None` for anything else, so the caller warns
    /// rather than guessing.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "async" => Some(Self::Async),
            "sync" => Some(Self::Sync),
            _ => None,
        }
    }
}

#[derive(serde::Deserialize, Default)]
struct RawSync {
    revalidate_secs: Option<u64>,
    push_floor_secs: Option<u64>,
    text_merge: Option<bool>,
    refresh_pinned: Option<String>,
    open_pinned: Option<String>,
    upload: Option<String>,
    ignore_patterns: Option<Vec<String>>,
}
#[derive(serde::Deserialize, Default)]
struct RawTls {
    ca_cert: Option<String>,
    insecure: Option<bool>,
    http1_only: Option<bool>,
}
#[derive(serde::Deserialize, Default)]
struct RawMount {
    point: Option<String>,
    dispatch_threads: Option<usize>,
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
    // An empty string is not a path: it would resolve to the current working
    // directory, which for a daemon is wherever it happened to be started.
    s.db_path = raw
        .state
        .db_path
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    // A typo must not silently pick a policy the user did not choose — least of
    // all `auto`, which spends their bandwidth. Warn, name the bad value, and
    // keep the documented default.
    // Same rule as below: name the bad value, keep the documented default.
    if let Some(v) = raw.sync.open_pinned {
        match OpenPinned::parse(&v) {
            Some(p) => s.open_pinned = p,
            None => tracing::warn!(
                value = %v,
                "config.toml: unknown [sync] open_pinned; keeping 'newest' \
                 (newest | newest-unmetered | offline)"
            ),
        }
    }
    if let Some(v) = raw.sync.upload {
        match UploadMode::parse(&v) {
            Some(m) => s.upload = m,
            None => tracing::warn!(
                value = %v,
                "config.toml: unknown [sync] upload; keeping 'async' (async | sync)"
            ),
        }
    }
    if let Some(v) = raw.sync.refresh_pinned {
        match RefreshPinned::parse(&v) {
            Some(mode) => s.refresh_pinned = mode,
            None => tracing::warn!(
                value = %v,
                "config.toml: [sync] refresh_pinned must be manual, ask or auto — keeping \"ask\""
            ),
        }
    }
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
    s.tls.http1_only = raw.tls.http1_only.unwrap_or(false);
    s.mount_point = raw
        .mount
        .point
        .filter(|p| !p.is_empty())
        .map(|p| expand_tilde(&p));
    if let Some(n) = raw.mount.dispatch_threads {
        // Clamp to a sane range: 0 is meaningless, and an unbounded value would
        // spawn a thread storm. Keep the default (1) for anything outside it.
        s.dispatch_threads = n.clamp(1, 16);
    }
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
    fn the_diag_socket_path_is_deterministic_and_within_the_unix_limit() {
        let a = diag_socket_for_mount(std::path::Path::new("/home/gnome/Wusel"));
        let b = diag_socket_for_mount(std::path::Path::new("/home/gnome/Wusel"));
        assert_eq!(
            a, b,
            "same mountpoint, same socket — producer and consumer agree"
        );
        assert_ne!(
            a,
            diag_socket_for_mount(std::path::Path::new("/home/gnome/Other")),
            "different mountpoints do not collide"
        );
        // However deep the mountpoint, the hashed file name is a fixed short
        // length — `diag-` + 16 hex + `.sock` — so the socket path stays well
        // inside the 108-byte unix limit under any real `$XDG_RUNTIME_DIR`.
        let deep = diag_socket_for_mount(std::path::Path::new(
            "/run/user/1000/very/deep/nested/mount/point/that/keeps/going/Wusel",
        ));
        assert_eq!(
            deep.file_name().unwrap().len(),
            "diag-0123456789abcdef.sock".len(),
            "the hashed name is constant length regardless of mountpoint depth"
        );
    }

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
        let s = parse_settings(
            "[tls]\nca_cert = \"/etc/wusel/ca.pem\"\ninsecure = true\nhttp1_only = true\n",
        )
        .unwrap();
        assert_eq!(
            s.tls.ca_cert.as_deref(),
            Some(std::path::Path::new("/etc/wusel/ca.pem"))
        );
        assert!(s.tls.insecure);
        assert!(s.tls.http1_only);
    }

    #[test]
    fn http1_only_defaults_off() {
        assert!(
            !parse_settings("[tls]\ninsecure = false\n")
                .unwrap()
                .tls
                .http1_only
        );
        assert!(!parse_settings("").unwrap().tls.http1_only);
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

#[cfg(test)]
mod open_pinned_tests {
    use super::{OpenAction, OpenPinned};

    #[test]
    fn newest_always_goes_to_the_server() {
        for metered in [Some(true), Some(false), None] {
            assert_eq!(OpenPinned::Newest.decide(metered), OpenAction::Fetch);
        }
    }

    #[test]
    fn offline_always_serves_what_is_here() {
        for metered in [Some(true), Some(false), None] {
            assert_eq!(OpenPinned::Offline.decide(metered), OpenAction::ServeLocal);
        }
    }

    #[test]
    fn unmetered_fetches_only_on_a_connection_that_costs_nothing() {
        assert_eq!(
            OpenPinned::NewestUnmetered.decide(Some(false)),
            OpenAction::Fetch
        );
        assert_eq!(
            OpenPinned::NewestUnmetered.decide(Some(true)),
            OpenAction::ServeLocal
        );
    }

    /// Somebody who chose this said the connection's cost decides. An unknown
    /// cost is therefore not a licence to spend — even though erring this way
    /// means serving something outdated.
    #[test]
    fn an_unknown_connection_is_not_treated_as_free() {
        assert_eq!(
            OpenPinned::NewestUnmetered.decide(None),
            OpenAction::ServeLocal
        );
    }

    #[test]
    fn the_spellings_a_person_would_write_are_understood() {
        assert_eq!(OpenPinned::parse("newest"), Some(OpenPinned::Newest));
        assert_eq!(
            OpenPinned::parse("newest-unmetered"),
            Some(OpenPinned::NewestUnmetered)
        );
        assert_eq!(OpenPinned::parse(" Offline "), Some(OpenPinned::Offline));
        // A typo must not silently pick one — the caller warns and keeps the
        // default, which is the behaviour that existed before the setting.
        assert_eq!(OpenPinned::parse("offlien"), None);
        assert_eq!(OpenPinned::default(), OpenPinned::Newest);
    }
}

#[cfg(test)]
mod refresh_pinned_tests {
    use super::{RefreshAction, RefreshPinned};

    #[test]
    fn manual_only_shows_and_ask_only_asks() {
        // Neither spends anything, so the connection is irrelevant to them.
        for metered in [Some(true), Some(false), None] {
            assert_eq!(
                RefreshPinned::Manual.decide(metered),
                RefreshAction::ShowOnly
            );
            assert_eq!(RefreshPinned::Ask.decide(metered), RefreshAction::Ask);
        }
    }

    #[test]
    fn auto_fetches_only_on_a_connection_known_to_be_free() {
        assert_eq!(
            RefreshPinned::Auto.decide(Some(false)),
            RefreshAction::Fetch
        );
    }

    #[test]
    fn auto_degrades_to_asking_when_the_link_is_metered() {
        assert_eq!(RefreshPinned::Auto.decide(Some(true)), RefreshAction::Ask);
    }

    #[test]
    fn auto_degrades_to_asking_when_the_link_is_unknown() {
        // The dangerous case, and the reason this is a three-valued question:
        // treating "do not know" as "free" is how a large refresh lands on
        // somebody's mobile plan.
        assert_eq!(RefreshPinned::Auto.decide(None), RefreshAction::Ask);
    }

    #[test]
    fn a_misspelt_mode_is_rejected_rather_than_guessed() {
        assert_eq!(RefreshPinned::parse("auto"), Some(RefreshPinned::Auto));
        assert_eq!(RefreshPinned::parse("  ASK "), Some(RefreshPinned::Ask));
        assert_eq!(RefreshPinned::parse("automatic"), None);
        assert_eq!(RefreshPinned::parse(""), None);
    }

    #[test]
    fn the_default_asks() {
        // `manual` is invisible to anyone who does not read emblems, and `auto`
        // spends someone else's bandwidth on their behalf.
        assert_eq!(RefreshPinned::default(), RefreshPinned::Ask);
    }
}
