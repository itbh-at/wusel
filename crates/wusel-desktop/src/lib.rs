// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! OS-integration backend behind [`wusel_core::desktop::Desktop`].
//!
//! Four channels. The first three are Linux/GNOME-specific and live in
//! [`mod@linux`], the fourth is plain `std::process` and works on any platform
//! this crate builds for:
//!
//! * **Notifications** — the engine's notices become freedesktop notifications
//!   (`org.freedesktop.Notifications.Notify`), localized and severity-styled.
//!   One session D-Bus connection via `zbus` (talking the protocol directly — no
//!   higher-level crate, so no second zbus stack).
//! * **File manager** — the mount registers as an `org.freedesktop.CloudProviders`
//!   provider (the GNOME/Nautilus + GTK integration), so a file manager shows it
//!   with a live sync status (idle / syncing / error). The daemon exports the
//!   `ObjectManager` + `Provider` + `Account` objects and owns the bus name; the
//!   `.desktop` registration file it points at lives in a *system* data dir and is
//!   installed separately ([`install_provider`]), because the collector scans only
//!   `$XDG_DATA_DIRS`, never `~/.local/share`.
//! * **GNOME Shell search** — [`run_search_provider`], its own D-Bus service.
//! * **Notify hook** (opt-in, `[desktop] notify_hook` in `config.toml`) — a script
//!   run for every notice. Deliberately **not** gated to Linux: it depends on
//!   nothing Linux-specific (a subprocess and some environment variables), and a
//!   headless box — the whole reason it exists — is exactly where the other three
//!   channels have nowhere to go. See [`run_hook`].
//!
//! It is a swappable module: KDE Dolphin (no libcloudproviders — its own KIO /
//! plugin mechanism), macOS (File Provider) and Windows (Cloud Filter) would each
//! be their own backend behind the same trait for their three platform-specific
//! channels — the notify hook needs no equivalent, it already works there. The
//! daemon injects the whole thing once via `Provider::set_desktop`.
//!
//! **Fail-soft throughout.** The trait methods never block the caller (they hand
//! work to a worker thread) and never fail: no session bus, no notification
//! daemon, no file-manager support, no hook configured, a hook that cannot run —
//! every one of those is dropped and logged, never propagated. Desktop
//! integration can never affect whether the filesystem works.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use wusel_core::desktop::{Desktop, Notice};
// `Status` is only needed by `HookOnlyDesktop`, which exists on non-Linux
// only (see below) — importing it unconditionally would warn as unused on a
// Linux build, where `mod linux` brings in its own copy instead.
#[cfg(not(target_os = "linux"))]
use wusel_core::desktop::Status;

/// The OS-integration backend for this platform, for `Provider::set_desktop`. On
/// Linux it drives notifications + the file-manager cloud-provider status for the
/// mount at `mount_path` under `account`, plus the notify-hook script at `hook`
/// (if configured). On any other platform, the notify hook is all there is to
/// offer — `account`/`mount_path` go unused there, kept only so every platform
/// takes the same signature.
#[cfg(target_os = "linux")]
pub fn backend(account: &str, mount_path: &Path, hook: Option<&Path>) -> Arc<dyn Desktop> {
    if let Some(h) = hook {
        check_hook(h);
    }
    linux::backend(account, mount_path, hook)
}

#[cfg(not(target_os = "linux"))]
pub fn backend(_account: &str, _mount_path: &Path, hook: Option<&Path>) -> Arc<dyn Desktop> {
    match hook {
        Some(h) => {
            check_hook(h);
            Arc::new(HookOnlyDesktop {
                hook: h.to_path_buf(),
                locale: wusel_core::desktop::ui_locale(),
            })
        }
        // Nothing configured, nothing this platform can offer on its own —
        // the true no-op, same as before this function took a `hook` argument.
        None => wusel_core::desktop::null(),
    }
}

/// The `Desktop` backend for a platform with no notification channel of its own
/// (today: everything but Linux) but a configured notify hook still deserving to
/// fire — it depends on nothing platform-specific. Status/file-changed have
/// nowhere to go here and stay the trait's no-op defaults. Only ever
/// constructed on a non-Linux platform (see `backend` above); cfg-gated so a
/// Linux build, which never uses it, does not warn about dead code.
#[cfg(not(target_os = "linux"))]
struct HookOnlyDesktop {
    hook: PathBuf,
    locale: String,
}

#[cfg(not(target_os = "linux"))]
impl Desktop for HookOnlyDesktop {
    fn notify(&self, notice: &Notice) {
        run_hook(Some(&self.hook), notice, &self.locale);
    }
    fn set_status(&self, _status: Status) {}
}

/// Log once, at start-up, whether the configured notify-hook actually looks
/// runnable — a missing file or a missing executable bit. Advisory only: it does
/// not stop the daemon (the file could appear later) and is not the only check —
/// [`exec_hook`] still handles a failure at run time the same way, since the file
/// can change between here and then.
fn check_hook(hook: &Path) {
    #[cfg(unix)]
    let runnable = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(hook).map(|m| m.permissions().mode() & 0o111 != 0)
    };
    #[cfg(not(unix))]
    let runnable = std::fs::metadata(hook).map(|_| true);
    match runnable {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            hook = %hook.display(),
            "notify_hook is not executable — notices will not reach it \
             until `chmod +x` is run"
        ),
        Err(e) => tracing::warn!(
            %e,
            hook = %hook.display(),
            "notify_hook does not exist yet — notices will not reach it \
             until it does"
        ),
    }
}

/// Fire the notify-hook for one notice, if configured. Hands off to its own
/// thread immediately — like every `Desktop` method, this must never make the
/// caller wait on an external script.
fn run_hook(hook: Option<&Path>, notice: &Notice, locale: &str) {
    let Some(hook) = hook else { return };
    let message = notice.localize(locale);
    let json = notice.to_json().to_string();
    let hook = hook.to_path_buf();
    if let Err(e) = std::thread::Builder::new()
        .name("wusel-notify-hook".into())
        .spawn(move || exec_hook(&hook, &message.title, &message.body, &json))
    {
        tracing::debug!(%e, "could not start the notify-hook thread");
    }
}

/// Run one notify-hook invocation to completion, on the calling (dedicated)
/// thread. `WUSEL_NOTICE_TITLE`/`_BODY` are the localized text (what a desktop
/// notification would also show, where one exists); `WUSEL_NOTICE_JSON` is the
/// unlocalized structured payload from [`Notice::to_json`], for a script that
/// wants to act on `kind` rather than parse a sentence.
fn exec_hook(hook: &Path, title: &str, body: &str, json: &str) {
    let mut child = match std::process::Command::new(hook)
        .env("WUSEL_NOTICE_TITLE", title)
        .env("WUSEL_NOTICE_BODY", body)
        .env("WUSEL_NOTICE_JSON", json)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            // Not found, not executable, a permissions problem — all the same to
            // the caller: the hook did not run. Warn, not debug (see
            // `check_hook` — this is the same failure, just discovered late).
            tracing::warn!(%e, hook = %hook.display(), "notify_hook could not start");
            return;
        }
    };
    // A broken or hanging script must not leak this thread forever: give it a
    // generous but bounded window, then kill it. Polling rather than a blocking
    // `wait()` is what makes the kill possible at all.
    const HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    tracing::debug!(
                        hook = %hook.display(),
                        %status,
                        "notify_hook exited with a non-zero status"
                    );
                }
                return;
            }
            Ok(None) if start.elapsed() > HOOK_TIMEOUT => {
                tracing::warn!(hook = %hook.display(), "notify_hook timed out — killing it");
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => {
                tracing::debug!(%e, hook = %hook.display(), "notify_hook: wait failed");
                return;
            }
        }
    }
}

/// Deliver a single notification **synchronously**, for the `wusel desktop
/// notify` diagnostic — a direct end-to-end check of the notification channel,
/// independent of any engine event. Returns the D-Bus outcome (`Ok` once the
/// notification daemon accepted the call, else the error) so the CLI can report
/// precisely whether desktop notifications work at all.
#[cfg(target_os = "linux")]
pub fn notify(notice: &wusel_core::desktop::Notice) -> Result<(), String> {
    linux::notify_once(notice)
}

#[cfg(not(target_os = "linux"))]
pub fn notify(_notice: &wusel_core::desktop::Notice) -> Result<(), String> {
    Err("desktop notifications are only implemented on Linux".to_string())
}

/// Run the GNOME Shell search provider (`org.gnome.Shell.SearchProvider2`) until
/// the process is killed. `search` answers a query (typically via Nextcloud
/// Unified Search) and `activate` opens a chosen result. This is a D-Bus
/// service GNOME Shell activates on demand — independent of the mount daemon.
/// Blocks; returns only on a setup error.
#[cfg(target_os = "linux")]
pub fn run_search_provider<S, A>(search: S, activate: A) -> Result<(), String>
where
    S: Fn(&str) -> Vec<wusel_core::search::SearchHit> + Send + Sync + 'static,
    A: Fn(&wusel_core::search::SearchHit) + Send + Sync + 'static,
{
    linux::run_search_provider(Box::new(search), Box::new(activate))
}

#[cfg(not(target_os = "linux"))]
pub fn run_search_provider<S, A>(_search: S, _activate: A) -> Result<(), String>
where
    S: Fn(&str) -> Vec<wusel_core::search::SearchHit> + Send + Sync + 'static,
    A: Fn(&wusel_core::search::SearchHit) + Send + Sync + 'static,
{
    Err("the search provider is only implemented on Linux".to_string())
}

// --- Cloud-provider registration file (org.freedesktop.CloudProviders) --------
//
// libcloudproviders' collector scans ONLY the system data dirs (`$XDG_DATA_DIRS`,
// e.g. /usr/share/applications) for `.desktop` files that
// `Implements=org.freedesktop.CloudProviders` — it never looks in the user's
// `~/.local/share`. So the registration file must be installed system-wide: by the
// package, or by `wusel desktop install-provider` run with root. The running
// daemon only owns the bus name and exports the objects the file points at. (This
// is the GNOME/Nautilus + GTK integration; KDE Dolphin does not consume
// libcloudproviders — it would be its own backend, e.g. KIO / a Dolphin plugin.)

/// Default system directory the registration `.desktop` is installed into. Must be
/// one of `$XDG_DATA_DIRS` for a file manager to find it.
pub const PROVIDER_INSTALL_DIR: &str = "/usr/share/applications";

/// Reduce a name to a valid D-Bus name element (`[A-Za-z0-9_]`, not starting with
/// a digit), so an arbitrary account name yields a valid bus name / object path.
fn sanitize(account: &str) -> String {
    let mut out: String = account
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// The D-Bus bus name this account's provider owns (one provider per account, so
/// several accounts coexist as separate `.desktop` files / sidebar entries).
fn bus_name(account: &str) -> String {
    format!("org.freedesktop.CloudProviders.wusel.{}", sanitize(account))
}

/// The provider's ObjectManager path — the base under which the Provider and
/// Account objects are exported.
fn object_base(account: &str) -> String {
    format!(
        "/org/freedesktop/CloudProviders/wusel/{}",
        sanitize(account)
    )
}

/// The registration file's name (reverse-DNS, unique per account).
fn desktop_filename(account: &str) -> String {
    format!("{}.desktop", bus_name(account))
}

/// The registration file's contents: the `Implements` marker plus the bus name and
/// object path the collector connects to. `NoDisplay=true` keeps it out of menus.
fn desktop_contents(account: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=wusel\n\
         NoDisplay=true\n\
         Implements=org.freedesktop.CloudProviders\n\
         \n\
         [org.freedesktop.CloudProviders]\n\
         BusName={}\n\
         ObjectPath={}\n",
        bus_name(account),
        object_base(account),
    )
}

/// Install the cloud-provider registration file for `account` into `dir` (usually
/// [`PROVIDER_INSTALL_DIR`]), so a file manager discovers the mount. Returns the
/// written path. Needs write access to a system data dir (root) — packaging does
/// this for the default account; named accounts use `desktop install-provider`.
pub fn install_provider(account: &str, dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(desktop_filename(account));
    std::fs::write(&path, desktop_contents(account))?;
    // Best-effort: nudge the desktop database (the collector also watches the dir
    // live, so this is only belt-and-suspenders; harmless if the tool is absent).
    let _ = std::process::Command::new("update-desktop-database")
        .arg(dir)
        .status();
    Ok(path)
}

/// Remove the registration file for `account` from `dir`. Returns whether a file
/// was actually present.
pub fn uninstall_provider(account: &str, dir: &Path) -> std::io::Result<bool> {
    let path = dir.join(desktop_filename(account));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::Arc;
    use std::thread::JoinHandle;

    use super::run_hook;

    use wusel_core::desktop::{self, Desktop, Notice, Severity, Status};
    use zbus::blocking::Connection;
    use zbus::zvariant::Value;

    // --- The org.freedesktop.CloudProviders D-Bus objects we export -------------
    //
    // Exact wire format from libcloudproviders' data/cloud-providers-dbus.xml.

    struct Provider {
        name: String,
    }

    #[zbus::interface(name = "org.freedesktop.CloudProviders.Provider")]
    impl Provider {
        #[zbus(property)]
        fn name(&self) -> &str {
            &self.name
        }
    }

    struct Account {
        name: String,
        path: String,
        icon: String,
        /// Shared with the worker, which flips it on `set_status`. The getter reads
        /// it live, so a `Get`/`GetAll` always returns the current status.
        status: Arc<AtomicI32>,
        status_details: String,
    }

    #[zbus::interface(name = "org.freedesktop.CloudProviders.Account")]
    impl Account {
        #[zbus(property)]
        fn name(&self) -> &str {
            &self.name
        }
        #[zbus(property)]
        fn path(&self) -> &str {
            &self.path
        }
        #[zbus(property)]
        fn icon(&self) -> &str {
            &self.icon
        }
        #[zbus(property)]
        fn status(&self) -> i32 {
            self.status.load(Ordering::Relaxed)
        }
        #[zbus(property)]
        fn status_details(&self) -> &str {
            &self.status_details
        }
    }

    /// `CloudProvidersAccountStatus`: 0 invalid, 1 idle, 2 syncing, 3 error.
    fn wire_status(s: Status) -> i32 {
        match s {
            Status::Idle => 1,
            Status::Syncing => 2,
            Status::Error => 3,
        }
    }

    /// What the trait methods hand to the worker, so the caller (the single FUSE
    /// thread) never blocks on a D-Bus round-trip.
    enum Msg {
        Notify(Notice),
        Status(Status),
        FileChanged(String),
    }

    struct LinuxDesktop {
        tx: Sender<Msg>,
        _worker: JoinHandle<()>,
    }

    impl Desktop for LinuxDesktop {
        fn notify(&self, notice: &Notice) {
            let _ = self.tx.send(Msg::Notify(notice.clone()));
        }
        fn set_status(&self, status: Status) {
            let _ = self.tx.send(Msg::Status(status));
        }
        fn file_changed(&self, abs_path: &str) {
            let _ = self.tx.send(Msg::FileChanged(abs_path.to_string()));
        }

        fn is_metered(&self) -> Option<bool> {
            metered_now()
        }
    }

    /// Ask NetworkManager whether the active connection costs money.
    ///
    /// `NMMetered` is deliberately five-valued, and we keep all five apart:
    /// `YES`/`GUESS_YES` mean treat it as metered, `NO`/`GUESS_NO` mean it is
    /// free, and `UNKNOWN` means **unknown** — never "free". Collapsing unknown
    /// into free is how a two-gigabyte refresh lands on somebody's phone plan.
    ///
    /// A read of one property on the system bus, done fresh each time rather
    /// than cached: the answer changes when the laptop leaves the office, which
    /// is exactly when it matters.
    fn metered_now() -> Option<bool> {
        use zbus::blocking::Connection;
        let conn = Connection::system().ok()?;
        let reply = conn
            .call_method(
                Some("org.freedesktop.NetworkManager"),
                "/org/freedesktop/NetworkManager",
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.freedesktop.NetworkManager", "Metered"),
            )
            .ok()?;
        let value: zbus::zvariant::OwnedValue = reply.body().deserialize().ok()?;
        let raw: u32 = u32::try_from(value).ok()?;
        match raw {
            1 | 3 => Some(true),  // YES, GUESS_YES
            2 | 4 => Some(false), // NO, GUESS_NO
            _ => None,            // UNKNOWN — and unknown is not free
        }
    }

    pub fn backend(account: &str, mount_path: &Path, hook: Option<&Path>) -> Arc<dyn Desktop> {
        let (tx, rx) = channel();
        let account = account.to_string();
        let mount = mount_path.display().to_string();
        let hook = hook.map(Path::to_path_buf);
        match std::thread::Builder::new()
            .name("wusel-desktop".into())
            .spawn(move || worker(rx, &account, &mount, hook))
        {
            Ok(worker) => Arc::new(LinuxDesktop {
                tx,
                _worker: worker,
            }),
            Err(e) => {
                tracing::warn!(%e, "could not start the desktop backend — no OS integration");
                desktop::null()
            }
        }
    }

    /// Connect to the session (user) bus. When `DBUS_SESSION_BUS_ADDRESS` is not
    /// in the environment — a daemon started over SSH or from cron on a desktop
    /// machine — fall back to systemd's per-user bus at `$XDG_RUNTIME_DIR/bus`,
    /// which is one bus per user and thus shared with the graphical session. So
    /// notifications and the file-manager status work out of the box from an SSH
    /// shell, no manual export needed.
    fn connect_session_bus() -> zbus::Result<Connection> {
        let primary = match Connection::session() {
            Ok(c) => return Ok(c),
            Err(e) => e,
        };
        let Ok(run) = std::env::var("XDG_RUNTIME_DIR") else {
            return Err(primary);
        };
        let addr = format!("unix:path={run}/bus");
        match zbus::blocking::connection::Builder::address(addr.as_str()).and_then(|b| b.build()) {
            Ok(c) => {
                tracing::info!(
                    %addr,
                    "session bus found via XDG_RUNTIME_DIR (DBUS_SESSION_BUS_ADDRESS was not set)"
                );
                Ok(c)
            }
            // The original error names the real problem (no bus at all); the
            // fallback failing is the expected case on headless systems.
            Err(_) => Err(primary),
        }
    }

    fn worker(rx: Receiver<Msg>, account: &str, mount: &str, hook: Option<PathBuf>) {
        let locale = desktop::ui_locale();
        // One session-bus connection for the backend's lifetime; its object server
        // dispatches incoming D-Bus calls on zbus's own task, so it keeps serving
        // the cloud-provider objects even while this thread blocks on `rx.recv()`.
        let conn = match connect_session_bus() {
            Ok(c) => c,
            Err(e) => {
                // Warn, not debug: without this line the integration is just
                // silently absent, which reads as "broken" on the desktop.
                tracing::warn!(
                    %e,
                    "no session D-Bus — desktop integration disabled \
                     (notifications, file-manager status)"
                );
                // The notify hook does not depend on D-Bus — a headless server is
                // exactly where it matters most, so notices still reach it even
                // though the desktop channel is down. `Status`/`FileChanged` stay
                // drained: both are purely D-Bus concerns (cloud-provider status,
                // file-manager invalidation), nothing a hook script would act on.
                while let Ok(msg) = rx.recv() {
                    if let Msg::Notify(notice) = msg {
                        run_hook(hook.as_deref(), &notice, &locale);
                    }
                }
                return;
            }
        };

        // Register as a cloud provider (best-effort). `status` starts idle. The bus
        // name + object path must match the installed `.desktop` (see the shared
        // `bus_name`/`object_base` helpers); the daemon does NOT write that file —
        // it lives in a system data dir, installed by the package or
        // `wusel desktop install-provider`.
        let base_path = super::object_base(account);
        let account_path = format!("{base_path}/account");
        let status = Arc::new(AtomicI32::new(wire_status(Status::Idle)));
        let provider_ok = register_cloud_provider(
            &conn,
            &super::bus_name(account),
            &base_path,
            &account_path,
            account,
            mount,
            status.clone(),
        );

        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::Notify(notice) => {
                    match show(&conn, &notice, &locale) {
                        Ok(()) => tracing::debug!("desktop notification sent"),
                        Err(e) => tracing::debug!(%e, "desktop notification not shown"),
                    }
                    run_hook(hook.as_deref(), &notice, &locale);
                }
                Msg::Status(s) => {
                    status.store(wire_status(s), Ordering::Relaxed);
                    if provider_ok {
                        if let Err(e) = emit_status_changed(&conn, &account_path, wire_status(s)) {
                            tracing::debug!(%e, "cloud-provider status signal failed");
                        }
                    }
                }
                Msg::FileChanged(path) => {
                    if let Err(e) = emit_file_changed(&conn, &path) {
                        tracing::debug!(%e, %path, "file-changed signal failed");
                    }
                }
            }
        }
    }

    /// Emit our `FileChanged` signal so a file-manager extension re-reads that
    /// file's emblem live. A private, sender-agnostic interface the extension
    /// matches by name (no bus-name/object-path coupling).
    fn emit_file_changed(conn: &Connection, abs_path: &str) -> zbus::Result<()> {
        conn.emit_signal(
            None::<&str>,
            "/at/itbh/Wusel",
            "at.itbh.Wusel",
            "FileChanged",
            &(abs_path,),
        )
    }

    /// Export the ObjectManager + Provider + Account objects and own the bus name,
    /// so a file manager discovers this mount. Best-effort — returns whether it
    /// took, and never propagates a failure to the caller.
    fn register_cloud_provider(
        conn: &Connection,
        bus_name: &str,
        base_path: &str,
        account_path: &str,
        account: &str,
        mount: &str,
        status: Arc<AtomicI32>,
    ) -> bool {
        // Sidebar name = the mount folder's own name + " (Nextcloud)". So the
        // default `~/Wusel` reads "Wusel (Nextcloud)", a custom mountpoint uses
        // its name, and separate accounts (their own mountpoints, e.g.
        // `~/Wusel-work`) are already distinguished by that basename.
        let base = std::path::Path::new(mount)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .unwrap_or("Wusel");
        let display = format!("{base} (Nextcloud)");
        let _ = account; // distinction comes from the mountpoint name
        let provider_path = format!("{base_path}/provider");
        let server = conn.object_server();
        let result = (|| -> zbus::Result<()> {
            server.at(base_path, zbus::fdo::ObjectManager)?;
            server.at(
                provider_path.as_str(),
                Provider {
                    name: display.clone(),
                },
            )?;
            server.at(
                account_path,
                Account {
                    name: display,
                    path: mount.to_string(),
                    icon: "at.itbh.Wusel".to_string(),
                    status,
                    status_details: String::new(),
                },
            )?;
            conn.request_name(bus_name)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                tracing::info!(
                    bus_name,
                    "registered as a cloud provider (file-manager status)"
                );
                true
            }
            Err(e) => {
                tracing::debug!(%e, "cloud-provider registration failed — file-manager status disabled");
                false
            }
        }
    }

    /// Emit `org.freedesktop.DBus.Properties.PropertiesChanged` for the account's
    /// `Status`, so the file manager updates without polling.
    fn emit_status_changed(conn: &Connection, account_path: &str, status: i32) -> zbus::Result<()> {
        let mut changed: HashMap<&str, Value> = HashMap::new();
        changed.insert("Status", Value::I32(status));
        let invalidated: Vec<&str> = Vec::new();
        conn.emit_signal(
            None::<&str>,
            account_path,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &(
                "org.freedesktop.CloudProviders.Account",
                changed,
                invalidated,
            ),
        )
    }

    /// Open a session connection and deliver one notification, synchronously —
    /// the diagnostic path behind `wusel desktop notify`. Same D-Bus call as the
    /// worker's, so a success here proves the production path works too.
    pub fn notify_once(notice: &Notice) -> Result<(), String> {
        let conn = connect_session_bus().map_err(|e| format!("no session D-Bus: {e}"))?;
        show(&conn, notice, &desktop::ui_locale())
            .map_err(|e| format!("the Notify D-Bus call failed: {e}"))
    }

    /// Render one notice as a localized, severity-styled freedesktop notification —
    /// a single `org.freedesktop.Notifications.Notify` D-Bus call.
    fn show(conn: &Connection, notice: &Notice, locale: &str) -> zbus::Result<()> {
        let msg = notice.localize(locale);
        // Per severity, three levers so info/warning/error are told apart at a glance:
        //   * urgency hint (0 low / 1 normal / 2 critical) — critical stays on screen.
        //   * a distinct standard icon (dialog-information / -warning / -error).
        //   * timeout: 0 (never expire) for errors, -1 (daemon default) otherwise.
        let (urgency, icon, timeout) = match notice.severity() {
            Severity::Success => (0u8, "dialog-information", -1i32),
            Severity::Warning => (1u8, "dialog-warning", -1i32),
            Severity::Error => (2u8, "dialog-error", 0i32),
        };
        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert("urgency", Value::U8(urgency));
        // GNOME Shell shows the `app_icon` argument only when it can resolve the
        // sending app; an unknown "wusel" falls back to a generic icon (so all
        // severities looked alike). The `image-path` hint is honoured per
        // notification — a themed icon name is allowed — and takes precedence, so
        // the severity icon actually shows. We set both for other daemons too.
        hints.insert("image-path", Value::new(icon));
        let actions: Vec<&str> = Vec::new();
        conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            // (app_name, replaces_id, app_icon, summary, body, actions, hints, timeout)
            &(
                "wusel",
                0u32,
                icon,
                msg.title.as_str(),
                msg.body.as_str(),
                actions,
                hints,
                timeout,
            ),
        )?;
        Ok(())
    }

    // --- GNOME Shell search provider (org.gnome.Shell.SearchProvider2) ---------

    use wusel_core::search::SearchHit;
    use zbus::zvariant::OwnedValue;

    type SearchFn = Box<dyn Fn(&str) -> Vec<SearchHit> + Send + Sync>;
    type ActivateFn = Box<dyn Fn(&SearchHit) + Send + Sync>;

    struct SearchProvider {
        search: SearchFn,
        activate: ActivateFn,
        /// Results from the last query, keyed by the id we handed GNOME Shell, so
        /// `GetResultMetas`/`ActivateResult` can resolve an id back to its hit.
        cache: std::sync::Mutex<HashMap<String, SearchHit>>,
    }

    /// A metadata value for `GetResultMetas` (never fails for our string inputs).
    fn ov(s: String) -> OwnedValue {
        OwnedValue::try_from(Value::from(s)).expect("string → OwnedValue")
    }

    #[zbus::interface(name = "org.gnome.Shell.SearchProvider2")]
    impl SearchProvider {
        #[zbus(name = "GetInitialResultSet")]
        fn get_initial_result_set(&self, terms: Vec<String>) -> Vec<String> {
            let term = terms.join(" ");
            // GNOME Shell searches per keystroke; a 1–2 char term matches half the
            // server and is slow, so wait for a meaningful query.
            if term.trim().chars().count() < 3 {
                return Vec::new();
            }
            let hits = (self.search)(term.trim());
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.clear();
            let mut ids = Vec::with_capacity(hits.len());
            for (i, hit) in hits.into_iter().enumerate() {
                let id = i.to_string();
                cache.insert(id.clone(), hit);
                ids.push(id);
            }
            ids
        }

        #[zbus(name = "GetSubsearchResultSet")]
        fn get_subsearch_result_set(
            &self,
            _previous: Vec<String>,
            terms: Vec<String>,
        ) -> Vec<String> {
            // A fresh query is cheap and always correct; no need to filter locally.
            self.get_initial_result_set(terms)
        }

        #[zbus(name = "GetResultMetas")]
        fn get_result_metas(&self, identifiers: Vec<String>) -> Vec<HashMap<String, OwnedValue>> {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            identifiers
                .iter()
                .filter_map(|id| {
                    let hit = cache.get(id)?;
                    let mut meta = HashMap::new();
                    meta.insert("id".to_string(), ov(id.clone()));
                    meta.insert("name".to_string(), ov(hit.title.clone()));
                    if !hit.subline.is_empty() {
                        meta.insert("description".to_string(), ov(hit.subline.clone()));
                    }
                    // A themed icon name is a valid `g_icon_new_for_string` input.
                    meta.insert("gicon".to_string(), ov("text-x-generic".to_string()));
                    Some(meta)
                })
                .collect()
        }

        #[zbus(name = "ActivateResult")]
        fn activate_result(&self, identifier: String, _terms: Vec<String>, _timestamp: u32) {
            let hit = {
                let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
                cache.get(&identifier).cloned()
            };
            if let Some(hit) = hit {
                (self.activate)(&hit);
            }
        }

        #[zbus(name = "LaunchSearch")]
        fn launch_search(&self, _terms: Vec<String>, _timestamp: u32) {
            // No standalone search UI to open; results already appear in the
            // overview. (Could open the Nextcloud web search later.)
        }
    }

    pub fn run_search_provider(search: SearchFn, activate: ActivateFn) -> Result<(), String> {
        let provider = SearchProvider {
            search,
            activate,
            cache: std::sync::Mutex::new(HashMap::new()),
        };
        // Keep the connection alive for the process's lifetime; its object server
        // dispatches calls on zbus's own task.
        let _conn = zbus::blocking::connection::Builder::session()
            .map_err(|e| format!("no session bus: {e}"))?
            .name("at.itbh.Wusel.SearchProvider")
            .map_err(|e| format!("could not own the bus name: {e}"))?
            .serve_at("/at/itbh/Wusel/SearchProvider", provider)
            .map_err(|e| format!("could not export the search provider: {e}"))?
            .build()
            .map_err(|e| format!("could not connect to the session bus: {e}"))?;
        tracing::info!("search provider ready on org.gnome.Shell.SearchProvider2");
        loop {
            std::thread::park();
        }
    }
}
