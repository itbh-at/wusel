// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! wusel — daemon/CLI. The product that ties together the engine (`wusel-core`)
//! with a per-OS filesystem frontend. Today that is `wusel-fuse` on Linux; native
//! Windows (`wusel-cfapi`, Cloud Filter) and macOS (`wusel-fileprovider`, File
//! Provider) frontends are far-future experiments. File-manager integration
//! (`wusel-desktop`, libcloudproviders/D-Bus) is a separate, additive layer,
//! not a frontend.
//!
//! The CLI surface (parsed with `clap`, see [`Cli`]) is `login`, `mount`,
//! `service`, `pin`/`unpin`/`pins`, `accounts`, `account` and `desktop`
//! (notification diagnostics + file-manager provider registration). A global
//! `--account NAME` selects a profile; without it the implicit `default` account
//! is used — so a single-account user never has to think about profiles.

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};

use wusel_core::config::{self, Account};

mod doctor;
mod status;

/// wusel — a virtual Nextcloud filesystem.
#[derive(Parser)]
#[command(
    name = "wusel",
    about = "virtual Nextcloud filesystem",
    version,
    after_help = "Without --account, the default account is used. `mount` without a mountpoint \
                  uses config.toml's [mount] point or ~/Wusel[-<account>]."
)]
struct Cli {
    /// Account profile to act on (default: the implicit `default` account).
    #[arg(long, global = true, value_name = "NAME")]
    account: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Log in through Nextcloud Login Flow v2 and store the credentials.
    Login {
        /// The Nextcloud base URL, e.g. https://cloud.example.org
        server_url: String,
        /// Keep the app password in the OS keyring (default) or, with
        /// `--keyring false`, in the 0600 file. Unset uses `[auth] keyring`
        /// from config (default on). Either way it is fail-soft: an unusable
        /// keyring falls back to the file.
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        keyring: Option<bool>,
    },
    /// Mount the filesystem (build with `--features fuse`).
    Mount {
        /// Where to mount; defaults to config.toml's [mount] point or ~/Wusel.
        mountpoint: Option<String>,
    },
    /// Manage the systemd user service (Linux).
    Service {
        #[command(subcommand)]
        action: ServiceCmd,
    },
    /// Keep a file/dir offline (no path = the whole account).
    Pin {
        /// Path to pin; empty = the whole account ("download everything").
        path: Option<String>,
    },
    /// Remove a pin; its files become normal (evictable) cache again.
    Unpin {
        /// Path to unpin; empty = the whole-account pin.
        path: Option<String>,
    },
    /// Fetch the current version of pinned files that have gone out of date.
    Update {
        /// Path to bring up to date; empty = every pin in the account.
        path: Option<String>,
    },
    /// List the pins for the account.
    Pins,
    /// List configured accounts.
    Accounts,
    /// Manage named accounts.
    Account {
        #[command(subcommand)]
        action: AccountCmd,
    },
    /// Desktop-integration diagnostics (Linux).
    Desktop {
        #[command(subcommand)]
        action: DesktopCmd,
    },
    /// Cache maintenance (diagnostics).
    Cache {
        #[command(subcommand)]
        action: CacheCmd,
    },
    /// Run the GNOME Shell search provider (a D-Bus service, normally started on
    /// demand by GNOME Shell — see the file-manager integration docs).
    SearchProvider,
    /// Show what the mount is doing right now, by file name: uploads owed to the
    /// server (including any parked after a permanent failure), files coming
    /// down, and the rest of the work in flight. For the person whose files
    /// these are — unlike `doctor`, which is name-free and made to be shared.
    Status {
        /// Keep redrawing until interrupted. Individual reads are far too short
        /// to be caught by a single print.
        #[arg(long)]
        watch: bool,
    },
    /// Collect diagnostics for a support case: system, daemon, mount, the FUSE
    /// connection's waiting count, the daemon's internal state, and a redacted
    /// journal tail. Prints a report and, with `-o`, writes `<PREFIX>.txt` and
    /// `<PREFIX>.json`. Redacted by default, so it is safe to attach to a ticket.
    Doctor {
        /// Also write `<PREFIX>.txt` and `<PREFIX>.json` next to stdout.
        #[arg(short = 'o', long, value_name = "PREFIX")]
        output: Option<String>,
        /// Include full paths and file names — turns redaction off, for a
        /// consenting deep dive.
        #[arg(long)]
        include_listing: bool,
        /// Omit the journal tail from the report.
        #[arg(long)]
        no_logs: bool,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// Drop cached data so the next access loads fresh from the server: the
    /// whole account (metadata, content and pins — like a fresh connection;
    /// credentials and config are kept) or one path's subtree. Stop a running
    /// mount first. Separates "stale cache" from "server-side" problems.
    Clear {
        /// Remote path (e.g. "Photos"); no path = the whole account.
        path: Option<String>,
    },
}

#[derive(Subcommand)]
enum DesktopCmd {
    /// Send a test notification through the real backend, to verify the desktop's
    /// notification channel works (independent of any sync event).
    Notify {
        /// Which severity to send: info, warning, error, or all.
        #[arg(value_enum, default_value = "all")]
        severity: TestSeverity,
    },
    /// Install the file-manager cloud-provider registration for this account so a
    /// file manager (Nautilus) shows the mount. System-wide, so it needs root
    /// (packaging does this for the default account).
    InstallProvider {
        /// Target directory (default: the system applications dir).
        #[arg(long, value_name = "DIR")]
        dir: Option<String>,
    },
    /// Remove this account's file-manager cloud-provider registration.
    UninstallProvider {
        /// Directory it was installed into (default: the system applications dir).
        #[arg(long, value_name = "DIR")]
        dir: Option<String>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum TestSeverity {
    Info,
    Warning,
    Error,
    All,
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Mount now and at every login, in the background.
    Enable,
    /// Stop the mount and disable it at login.
    Disable,
    /// Show the service status (`systemctl --user status`).
    Status,
}

#[derive(Subcommand)]
enum AccountCmd {
    /// List configured accounts.
    List,
    /// Remove a named account (its credentials, state and cache).
    #[command(alias = "rm")]
    Remove {
        /// The account name to remove.
        name: String,
    },
}

fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let account = Account::new(cli.account.as_deref().unwrap_or(config::DEFAULT_ACCOUNT));

    // No subcommand → print help and exit cleanly (a friendly "just run it").
    let Some(command) = cli.command else {
        use clap::CommandFactory;
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };

    match command {
        Command::Login {
            server_url,
            keyring,
        } => {
            // The `--keyring[=bool]` flag wins; otherwise `[auth] keyring`
            // (default on). Storing is fail-soft either way.
            let use_keyring = keyring.unwrap_or(account.settings().auth_keyring);
            tokio::runtime::Runtime::new()?.block_on(cmd_login(&account, &server_url, use_keyring))
        }
        // The mountpoint is optional: without it we use config.toml's
        // `[mount] point` or the account's default mountpoint.
        Command::Mount { mountpoint } => cmd_mount(&account, mountpoint.as_deref()),
        Command::Service { action } => cmd_service(&account, action),
        Command::Pin { path } => cmd_pin(&account, path.as_deref().unwrap_or("")),
        Command::Unpin { path } => cmd_unpin(&account, path.as_deref().unwrap_or("")),
        Command::Update { path } => cmd_update(&account, path.as_deref().unwrap_or("")),
        Command::Pins => cmd_pins(&account),
        Command::Accounts => cmd_accounts_list(),
        Command::Account { action } => match action {
            AccountCmd::List => cmd_accounts_list(),
            AccountCmd::Remove { name } => cmd_account_remove(&name),
        },
        Command::Cache { action } => match action {
            CacheCmd::Clear { path } => cmd_cache_clear(&account, path.as_deref()),
        },
        Command::SearchProvider => cmd_search_provider(&account),
        Command::Status { watch } => status::run(&status::Options {
            account: account.name().to_string(),
            watch,
        }),
        Command::Doctor {
            output,
            include_listing,
            no_logs,
        } => doctor::run(&doctor::Options {
            account: account.name().to_string(),
            output: output.map(std::path::PathBuf::from),
            include_listing,
            no_logs,
        }),
        Command::Desktop { action } => match action {
            DesktopCmd::Notify { severity } => cmd_desktop_notify(severity),
            DesktopCmd::InstallProvider { dir } => {
                cmd_desktop_install_provider(&account, dir.as_deref())
            }
            DesktopCmd::UninstallProvider { dir } => {
                cmd_desktop_uninstall_provider(&account, dir.as_deref())
            }
        },
    }
}

/// Install the account's cloud-provider `.desktop` into a system data dir, where
/// (unlike `~/.local/share`) a file manager's libcloudproviders collector actually
/// looks. Needs root; on `EACCES` we print the exact `sudo` re-run.
fn cmd_desktop_install_provider(account: &Account, dir: Option<&str>) -> anyhow::Result<()> {
    let dir = dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(wusel_desktop::PROVIDER_INSTALL_DIR));
    match wusel_desktop::install_provider(account.name(), &dir) {
        Ok(path) => {
            println!(
                "✓ installed cloud-provider registration: {}",
                path.display()
            );
            println!("  A file manager shows this account once the mount is running.");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            bail!(
                "cannot write to {} — a system data dir needs root. Re-run with sudo:\n  \
                 sudo wusel{} desktop install-provider",
                dir.display(),
                account_flag(account),
            )
        }
        Err(e) => Err(e).with_context(|| format!("could not install into {}", dir.display())),
    }
}

/// Remove the account's cloud-provider `.desktop`. Needs root if it lives in a
/// system data dir.
fn cmd_desktop_uninstall_provider(account: &Account, dir: Option<&str>) -> anyhow::Result<()> {
    let dir = dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(wusel_desktop::PROVIDER_INSTALL_DIR));
    match wusel_desktop::uninstall_provider(account.name(), &dir) {
        Ok(true) => {
            println!(
                "✓ removed the cloud-provider registration from {}",
                dir.display()
            );
            Ok(())
        }
        Ok(false) => {
            println!("nothing to remove — no registration in {}", dir.display());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            bail!(
                "cannot remove from {} — a system data dir needs root. Re-run with sudo:\n  \
                 sudo wusel{} desktop uninstall-provider",
                dir.display(),
                account_flag(account),
            )
        }
        Err(e) => Err(e).with_context(|| format!("could not remove from {}", dir.display())),
    }
}

/// Fire test notification(s) straight through the platform backend, so the user
/// can confirm the desktop notification channel works without waiting for a real
/// sync event. Reports the D-Bus outcome per notice.
fn cmd_desktop_notify(severity: TestSeverity) -> anyhow::Result<()> {
    use wusel_core::desktop::Notice;

    // Representative notices, one per severity (see `Notice::severity`).
    let info = || Notice::ConnectionRestored {
        server: "cloud.example.org".into(),
    };
    let warning = || Notice::ConflictCopy {
        path: "Documents/report.odt".into(),
        copy: "Documents/report (conflicted copy 2026-07-23).odt".into(),
    };
    let error = || Notice::UploadFailed {
        path: "Documents/report.odt".into(),
        reason: "storage quota exceeded".into(),
    };

    let notices: Vec<(&str, Notice)> = match severity {
        TestSeverity::Info => vec![("info", info())],
        TestSeverity::Warning => vec![("warning", warning())],
        TestSeverity::Error => vec![("error", error())],
        TestSeverity::All => vec![("info", info()), ("warning", warning()), ("error", error())],
    };

    let mut failed = false;
    for (label, notice) in &notices {
        match wusel_desktop::notify(notice) {
            Ok(()) => println!("✓ sent {label} notification ({:?})", notice.severity()),
            Err(e) => {
                failed = true;
                eprintln!("✗ {label} notification failed: {e}");
            }
        }
    }
    if failed {
        bail!("at least one notification could not be delivered — see the errors above");
    }
    println!("All notifications were accepted by the notification daemon.");
    Ok(())
}

/// Runs through the Nextcloud Login Flow v2 and stores the credentials for
/// `account`. `use_keyring` prefers the OS keyring (fail-soft; falls back to the
/// `0600` file).
async fn cmd_login(account: &Account, server: &str, use_keyring: bool) -> anyhow::Result<()> {
    let client = build_http_client(&account.settings().tls)?;
    let init = wusel_core::auth::begin(&client, server)
        .await
        .context("could not start the login flow")?;

    println!(
        "Please open in your browser and confirm:\n  {}",
        init.login_url
    );
    println!("Waiting for confirmation (Ctrl-C to cancel) ...");

    loop {
        match wusel_core::auth::poll(&client, &init).await {
            Ok(creds) => {
                let path = account.credentials_path();
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)
                        .context("could not create the config directory")?;
                }
                let storage =
                    wusel_core::credentials::store(&path, account.name(), &creds, use_keyring)
                        .context("could not store credentials")?;
                println!("✓ Logged in as '{}' on {}", creds.login_name, creds.server);
                if !account.is_default() {
                    println!("  account: {}", account.name());
                }
                match storage {
                    wusel_core::credentials::Storage::Keyring => {
                        println!("  app password stored in the OS keyring");
                    }
                    wusel_core::credentials::Storage::File if use_keyring => {
                        println!(
                            "  app password stored in {} (0600) — the keyring was not usable",
                            path.display()
                        );
                    }
                    wusel_core::credentials::Storage::File => {
                        println!("  credentials stored at {} (0600)", path.display());
                    }
                }
                // Probe the server: confirm it is a Nextcloud and report its version.
                match wusel_core::capabilities::fetch(
                    &client,
                    &creds.server,
                    &creds.login_name,
                    &creds.app_password,
                )
                .await
                {
                    Ok(info) => match info.version {
                        Some(v) => println!("  server: Nextcloud {v}"),
                        None => println!("  server: reachable (version not reported)"),
                    },
                    Err(_) => eprintln!(
                        "  warning: could not read server capabilities — is {} a Nextcloud instance?",
                        creds.server
                    ),
                }
                return Ok(());
            }
            Err(wusel_core::Error::LoginPending) => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Mounts the filesystem for `account` — only with `--features fuse`.
#[cfg(feature = "fuse")]
fn cmd_mount(account: &Account, mountpoint: Option<&str>) -> anyhow::Result<()> {
    let creds_path = account.credentials_path();
    let creds = wusel_core::credentials::load(&creds_path, account.name()).with_context(|| {
        // Only claim the credentials are missing when they actually are. A file
        // that exists but cannot be used fails for a reason the cause below
        // states precisely — a keyring entry that is gone, one that cannot be
        // read, a corrupt file — and prefixing all of them with "no credentials
        // at …" buried that reason under a wrong summary.
        if creds_path.exists() {
            format!("could not load the credentials at {}", creds_path.display())
        } else {
            format!(
                "no credentials at {} — run `wusel login{} <server-url>` first",
                creds_path.display(),
                account_flag(account),
            )
        }
    })?;

    let settings = account.settings();

    // Resolve the mountpoint: explicit arg > config.toml > account default.
    let mountpoint: std::path::PathBuf = match mountpoint {
        Some(m) => std::path::PathBuf::from(m),
        None => settings
            .mount_point
            .clone()
            .unwrap_or_else(|| account.default_mountpoint()),
    };
    let mountpoint = mountpoint.as_path();

    // The daemon owns the mountpoint, so a systemd unit stays trivial.
    std::fs::create_dir_all(mountpoint).ok();
    let target = mountpoint
        .canonicalize()
        .unwrap_or_else(|_| mountpoint.to_path_buf());

    // The platform OS-integration backend (Linux notifications + the file-manager
    // cloud-provider status for this account's mountpoint, plus the notify-hook
    // script if configured; a no-op elsewhere or when D-Bus is absent —
    // fail-soft). Built *before* the first network call, not after the provider:
    // the very first thing that can go wrong is that the server cannot be
    // reached, and a start-up that says nothing is exactly the silence this
    // exists to break.
    let desktop = wusel_desktop::backend(account.name(), &target, settings.notify_hook.as_deref());
    // One shared answer to "can we reach the server?", fed by every request the
    // engine makes, and the only thing allowed to notify about it.
    let health = std::sync::Arc::new(wusel_core::health::Reachability::new(
        &creds.server,
        std::sync::Arc::clone(&desktop),
    ));

    let http = build_http_client(&settings.tls)?;
    let dav_user = resolve_dav_user(
        &http,
        &creds.server,
        &creds.login_name,
        &creds.app_password,
        Some(&health),
    );
    let dav = wusel_core::webdav::WebDavClient::new(
        http,
        &creds.server,
        &creds.login_name,
        &creds.app_password,
    )
    .with_health(std::sync::Arc::clone(&health));
    let dav = match dav_user {
        Some(uid) => dav.with_dav_user(&uid),
        None => dav,
    };

    let state = open_state(&account)?;

    let mut provider = wusel_core::provider::Provider::new(dav, state, account)
        .context("could not initialise the provider")?;

    // Instant cache invalidation over notify_push; degrades to TTL if absent. Its
    // retry loops keep talking to the server when nothing else does, which makes
    // them the mount's heartbeat: an otherwise idle daemon still learns that the
    // connection went away — and came back — and tells the user.
    let _push = wusel_core::push::spawn(
        &creds.server,
        &creds.login_name,
        &creds.app_password,
        settings.tls.clone(),
        provider.invalidation_handle(),
        provider.sync_trigger(),
        Some(std::sync::Arc::clone(&health)),
    );

    provider.set_desktop(desktop);

    // Refuse to mount where it would clobber another mount (a shared or nested
    // mountpoint between accounts, or a plain double-mount).
    #[cfg(target_os = "linux")]
    {
        let (all, ours) = read_active_mounts();
        if let Some(conflict) = wusel_core::mount::find_conflict(&target, &all, &ours) {
            bail!(
                "mountpoint {} overlaps an existing mount at {} — choose a different, \
                 empty directory; accounts must not share or nest mountpoints",
                target.display(),
                conflict.display(),
            );
        }
    }

    // Same instance in two accounts is allowed but rarely intended — warn.
    warn_if_duplicate_instance(account, &creds.server, &creds.login_name);

    // Record where we actually mount — which may be none of the paths another
    // command could derive, since `mountpoint` overrides config and default.
    // `cache clear` reads this to notice a live daemon (see
    // `live_mount_for_account`); the marker goes away again on clean exit.
    write_mount_marker(account, &target);
    let result = wusel_fuse::mount(&target, provider);
    remove_mount_marker(account);
    result
}

/// Warn (do not block) if another account already syncs the same server + user:
/// two mounts of one account race each other's uploads, exactly like two machines.
#[cfg(feature = "fuse")]
fn warn_if_duplicate_instance(account: &Account, server: &str, login: &str) {
    for name in config::list_accounts() {
        if name == account.name() {
            continue;
        }
        let other = Account::new(&name);
        // Only the metadata is needed here (server + user) — never touch the
        // keyring for a mere duplicate check.
        if let Ok((other_server, other_login)) =
            wusel_core::credentials::load_metadata(&other.credentials_path())
        {
            if other_server == server && other_login == login {
                tracing::warn!(
                    "account '{name}' already syncs {login}@{server} — mounting the same \
                     instance twice is redundant and makes the two mounts race each other's uploads"
                );
            }
        }
    }
}

/// Active mounts from `/proc/self/mountinfo`: (all mountpoints, ours = wusel).
/// Not gated on the `fuse` feature: `cmd_cache_clear` needs it too, and even a
/// non-fuse build must recognise a mount made by another build. Nor on the
/// platform: `/proc/self/mountinfo` simply does not exist off Linux, which
/// yields two empty lists — the honest answer there, since only Linux mounts.
fn read_active_mounts() -> (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>) {
    let mut all = Vec::new();
    let mut ours = Vec::new();
    if let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") {
        for line in text.lines() {
            // Fields: id parent dev root MOUNTPOINT opts... - fstype SOURCE superopts
            let Some(mp) = line.split(' ').nth(4) else {
                continue;
            };
            let path = std::path::PathBuf::from(unescape_mountpoint(mp));
            let source = line
                .rsplit_once(" - ")
                .and_then(|(_, tail)| tail.split_whitespace().nth(1));
            if source == Some("wusel") {
                ours.push(path.clone());
            }
            all.push(path);
        }
    }
    (all, ours)
}

/// Decode the octal escapes `mountinfo` uses (space/tab/newline/backslash).
fn unescape_mountpoint(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(not(feature = "fuse"))]
fn cmd_mount(_account: &Account, _mountpoint: Option<&str>) -> anyhow::Result<()> {
    bail!(
        "This binary was built without FUSE support.\n\
         Rebuild with:  cargo build -p wusel --features fuse"
    )
}

// --- systemd user service ---------------------------------------------------

/// Escape a binary path for a systemd `ExecStart=` line: wrap it in double
/// quotes so spaces survive systemd's word splitting, and double any `%` —
/// systemd expands `%x` specifiers even inside quotes, and `%%` is the literal
/// percent. Inside double quotes systemd honours C-style backslash escapes, so
/// `\` and `"` themselves are escaped too.
///
/// A control character is **refused**, not escaped. A unit file is
/// line-oriented: a raw newline would end the `ExecStart=` line and turn the
/// rest of the path into directives of its own — a unit forged from the
/// binary's own path. systemd does understand `\n` inside quotes, so escaping
/// would be possible, but a path containing a newline (or a NUL, a tab, an
/// ESC …) is pathological; refusing with a clear message is the answer the user
/// can act on, and it cannot produce a subtly wrong unit.
fn systemd_exec_escape(path: &str) -> anyhow::Result<String> {
    if let Some(c) = path.chars().find(|c| c.is_control()) {
        bail!(
            "refusing to write a systemd unit: the path of this binary contains the \
             control character U+{:04X}, which cannot appear in an ExecStart= line. \
             Move the binary to a path without control characters and re-run.",
            c as u32,
        );
    }
    let mut out = String::with_capacity(path.len() + 2);
    out.push('"');
    for c in path.chars() {
        match c {
            '%' => out.push_str("%%"),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}

/// The templated unit's contents. `%i` is the account name, filled in by systemd
/// when the instance `wusel@<account>` is started. `exec` is this binary's
/// absolute path, so it works for packaged, `cargo install`ed and dev builds.
/// Keep this in sync with the packaged twin `packaging/rpm/wusel@.service`.
fn unit_contents(exec: &str) -> anyhow::Result<String> {
    let exec = systemd_exec_escape(exec)?;
    Ok(format!(
        "[Unit]\n\
         Description=wusel — virtual Nextcloud filesystem (%i)\n\
         StartLimitIntervalSec=60\n\
         StartLimitBurst=3\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec} mount --account %i\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         # No sandboxing directives: mounting uses the setuid fusermount3 helper,\n\
         # which NoNewPrivileges=yes blocks. The hardening options that would\n\
         # apply here (LockPersonality, RestrictRealtime, ProtectKernelModules, …)\n\
         # all imply NoNewPrivileges=yes, so any of them breaks the mount with\n\
         # fusermount3: Operation not permitted.\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    ))
}

/// The unit template's file name; the same in every unit directory.
const UNIT_FILE: &str = "wusel@.service";

/// Where a *packaged* unit can live, in systemd's own precedence order (all of
/// them below `$XDG_CONFIG_HOME/systemd/user`, which is why a file we write
/// there shadows every one of them). `/etc` and `/run` are the admin's, the
/// `lib` dirs the distribution package's — for us they are the same case: a
/// unit somebody else installed and maintains.
const PACKAGED_UNIT_DIRS: [&str; 4] = [
    "/etc/systemd/user",
    "/run/systemd/user",
    "/usr/local/lib/systemd/user",
    "/usr/lib/systemd/user",
];

/// The packaged unit, if the system has one (RPM/DEB install it into
/// `/usr/lib/systemd/user`).
fn packaged_unit() -> Option<std::path::PathBuf> {
    PACKAGED_UNIT_DIRS
        .iter()
        .map(|d| std::path::Path::new(d).join(UNIT_FILE))
        .find(|p| p.exists())
}

/// The systemd unit instance for an account (`wusel@<account>.service`).
fn service_instance(account: &Account) -> anyhow::Result<String> {
    instance_name(account.name())
}

/// Turn an account name into a unit instance, rejecting what systemd cannot
/// name.
///
/// The name goes into the unit name verbatim, so it has to *be* a valid
/// instance: systemd's unit names are ASCII letters, digits and `:-_.` only.
/// `Account::new` already maps path-hostile characters to `_`, but its filter
/// is `char::is_alphanumeric` — Unicode-aware — so `--account Müller` survives
/// it and yields `wusel@Müller.service`, which `systemctl` rejects with a
/// message far from its cause. We could instead escape the name the way
/// `systemd-escape` does, but the unit would then have to read the instance
/// back with `%I` (unescaped) instead of `%i`, splitting it from the packaged
/// twin in `packaging/rpm/wusel@.service`. Rejecting right where the name
/// becomes a unit name is the smaller, clearer contract.
fn instance_name(name: &str) -> anyhow::Result<String> {
    if name.is_empty() {
        bail!("the account name must not be empty");
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')))
    {
        bail!(
            "account '{name}' cannot be used as a systemd service: '{c}' is not allowed in a \
             unit instance name (only ASCII letters, digits and '-', '_', '.', ':' are). \
             Rename the account — `wusel accounts` lists them — and enable the service for \
             the new name."
        );
    }
    Ok(format!("wusel@{name}.service"))
}

/// What `service enable` should do about the unit file. Split out from
/// [`install_unit`] so the decision is testable without touching `/usr`.
#[derive(Debug, PartialEq, Eq)]
enum UnitAction {
    /// Nothing packaged (a dev build, `cargo install`): write our own unit.
    Write,
    /// A packaged unit exists and nothing shadows it — leave it alone.
    UsePackaged,
    /// A packaged unit exists *and* an older user-level copy shadows it. We do
    /// not silently refresh that copy: it keeps pointing at whichever binary
    /// once wrote it, so the user is told how to remove it.
    KeepOverride,
}

/// The rule behind [`UnitAction`]: never write into `$XDG_CONFIG_HOME` when the
/// system already provides a unit. That directory wins over every system
/// location, so a copy written there from a dev build permanently pins
/// `ExecStart` to that build's path — and deleting the build then breaks the
/// service with nothing pointing at the override.
fn unit_action(packaged: bool, override_exists: bool) -> UnitAction {
    match (packaged, override_exists) {
        (false, _) => UnitAction::Write,
        (true, false) => UnitAction::UsePackaged,
        (true, true) => UnitAction::KeepOverride,
    }
}

/// One line naming the unit file that is actually in effect — what `service
/// status` prints above `systemctl status`, so a shadowing override is visible
/// instead of having to be guessed.
fn unit_in_effect(
    user_override: Option<&std::path::Path>,
    packaged: Option<&std::path::Path>,
) -> String {
    match (user_override, packaged) {
        (Some(u), Some(p)) => format!(
            "Unit file: {} (your own copy — it shadows the packaged {}).\n  \
             To go back to the packaged unit: rm {} && systemctl --user daemon-reload",
            u.display(),
            p.display(),
            u.display(),
        ),
        (Some(u), None) => format!(
            "Unit file: {} (written by `wusel service enable`).",
            u.display()
        ),
        (None, Some(p)) => format!("Unit file: {} (packaged).", p.display()),
        (None, None) => "Unit file: none installed — run `wusel service enable` first.".to_string(),
    }
}

/// Manages the systemd *user* service for an account (`wusel@<account>`).
/// Linux-only in practice (fails cleanly elsewhere: `systemctl` is absent).
fn cmd_service(account: &Account, action: ServiceCmd) -> anyhow::Result<()> {
    let instance = service_instance(account)?;
    match action {
        ServiceCmd::Enable => {
            if !account.credentials_path().exists() {
                bail!(
                    "no credentials for account '{}' — run `wusel login{} <server-url>` first",
                    account.name(),
                    account_flag(account),
                );
            }
            install_unit()?;
            systemctl(&["enable", "--now", &instance])?;
            println!("Enabled {instance}: it mounts now and at every login.");
            println!("Logs: journalctl --user -u {instance} -f");
        }
        ServiceCmd::Disable => {
            systemctl(&["disable", "--now", &instance])?;
            println!("Disabled {instance} (unmounted and off at login).");
        }
        ServiceCmd::Status => {
            // Which unit file is in effect — a user-level copy shadowing the
            // packaged one is otherwise invisible until the service misbehaves.
            let user = systemd_user_dir().join(UNIT_FILE);
            println!(
                "{}",
                unit_in_effect(
                    user.exists().then_some(user.as_path()),
                    packaged_unit().as_deref(),
                )
            );
            // `status` prints to stdout itself; ignore its non-zero exit codes.
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "--no-pager", "status", &instance])
                .status();
        }
    }
    Ok(())
}

/// Make sure a unit template is in place — writing one into the user's systemd
/// dir only when the system does not already provide one (see [`unit_action`]).
fn install_unit() -> anyhow::Result<()> {
    let dir = systemd_user_dir();
    let path = dir.join(UNIT_FILE);
    let packaged = packaged_unit();
    match unit_action(packaged.is_some(), path.exists()) {
        // Nothing to write, nothing to reload — the packaged unit is already
        // known to systemd.
        UnitAction::UsePackaged => {
            let packaged = packaged.expect("UsePackaged implies a packaged unit");
            println!("Using the packaged unit {}.", packaged.display());
        }
        UnitAction::KeepOverride => {
            let packaged = packaged.expect("KeepOverride implies a packaged unit");
            println!(
                "warning: {} shadows the packaged unit {}.\n  \
                 It keeps pointing at whichever binary installed it, so a packaged update \
                 will not reach the service. Remove it to use the packaged unit:\n    \
                 rm {} && systemctl --user daemon-reload",
                path.display(),
                packaged.display(),
                path.display(),
            );
        }
        UnitAction::Write => {
            let exec = std::env::current_exe().context("could not determine own binary path")?;
            std::fs::create_dir_all(&dir).context("could not create the systemd user dir")?;
            std::fs::write(&path, unit_contents(&exec.display().to_string())?)
                .with_context(|| format!("could not write {}", path.display()))?;
            systemctl(&["daemon-reload"])?;
        }
    }
    Ok(())
}

/// `$XDG_CONFIG_HOME/systemd/user` or `~/.config/systemd/user`.
fn systemd_user_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".config")
        });
    base.join("systemd").join("user")
}

/// Run `systemctl --user <args>`, failing on a non-zero exit.
fn systemctl(args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("could not run systemctl (is this a systemd user session?)")?;
    if !status.success() {
        bail!("`systemctl --user {}` failed", args.join(" "));
    }
    Ok(())
}

// --- Pinning ("always keep offline") ----------------------------------------

/// Build a `Provider` for an account (no FUSE needed) — used by pin/unpin.
fn build_provider(account: &Account) -> anyhow::Result<wusel_core::provider::Provider> {
    let creds = wusel_core::credentials::load(&account.credentials_path(), account.name())
        .with_context(|| {
            format!(
                "no credentials — run `wusel login{} <server-url>` first",
                account_flag(account)
            )
        })?;
    let http = build_http_client(&account.settings().tls)?;
    // No reachability tracking here: this builds a provider for a one-shot
    // command (search, diagnostics), where a failure is reported in the terminal
    // the user is looking at — a desktop notification would be redundant noise.
    let dav_user = resolve_dav_user(
        &http,
        &creds.server,
        &creds.login_name,
        &creds.app_password,
        None,
    );
    let dav = wusel_core::webdav::WebDavClient::new(
        http,
        &creds.server,
        &creds.login_name,
        &creds.app_password,
    );
    let dav = match dav_user {
        Some(uid) => dav.with_dav_user(&uid),
        None => dav,
    };
    let state = open_state(account)?;
    wusel_core::provider::Provider::new(dav, state, account)
        .context("could not initialise the provider")
}

/// Resolve the account's canonical **user id** for building DAV paths.
///
/// The login flow stores the `loginName` — the credential the user signed in
/// with, which some providers make an email — and that works for Basic auth and
/// for `/dav/files/<user>/`, but Nextcloud's chunked-upload endpoint
/// `/dav/uploads/<user>/` rejects a login alias with 403. So the path segment
/// must be the real user id (see [`wusel_core::webdav::WebDavClient::with_dav_user`]).
///
/// Best-effort: if the lookup fails (offline, an older server), return `None`
/// and let the caller keep the login name — the previous behaviour, which still
/// serves reads and small uploads.
///
/// This is usually the mount's **first** request, so it is also where an outage
/// is first seen: the outcome goes to `health` like any other, and the user is
/// told once it is clear the server is really gone rather than briefly busy.
fn resolve_dav_user(
    http: &reqwest::Client,
    server: &str,
    login: &str,
    password: &str,
    health: Option<&wusel_core::health::Reachability>,
) -> Option<String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    match rt.block_on(wusel_core::capabilities::whoami(
        http, server, login, password,
    )) {
        Ok(id) => {
            if let Some(health) = health {
                health.ok();
            }
            if id != login {
                tracing::info!(user_id = %id, "resolved the DAV user id (the login name is an alias)");
            }
            Some(id)
        }
        Err(e) => {
            if let Some(health) = health {
                health.failed(&e);
            }
            tracing::warn!(error = %e, "could not resolve the user id; using the login name for DAV paths");
            None
        }
    }
}

/// The account's pins, taking them out of an older database on the way if that
/// is where they still are.
///
/// Every command that touches pins goes through here, so the migration cannot
/// depend on which one the user happened to run first.
fn open_pins(account: &Account) -> anyhow::Result<wusel_core::pins::Pins> {
    let pins = wusel_core::pins::Pins::new(&account.config_dir());
    let db = account.state_db_path();
    if !pins.file().exists() && db.exists() {
        if let Ok(state) = wusel_core::state::StateDb::open_existing(&db) {
            let legacy = state.legacy_pins().unwrap_or_default();
            match pins.migrate_from(&legacy) {
                Ok(0) => {}
                Ok(n) => tracing::info!(count = n, "moved the pins out of the state database"),
                Err(e) => tracing::warn!(%e, "could not move the pins out of the database"),
            }
        }
    }
    Ok(pins)
}

/// Open the state database, saying out loud when it is not where the user would
/// expect it to be.
///
/// The announcement belongs here rather than in the engine: a relocation is
/// something a *person* needs to know about — their metadata now lives outside
/// their home directory and does not travel with their roaming profile — and
/// the engine has no channel to a person.
fn open_state(account: &Account) -> anyhow::Result<wusel_core::state::StateDb> {
    let location = account.db_location();
    if let Some(message) = location.message() {
        tracing::warn!("{message}");
    }
    location
        .prepare()
        .context("could not create the state directory")?;
    wusel_core::state::StateDb::open(location.path()).context("could not open the state database")
}

fn pin_label(path: &str) -> String {
    if path.trim_matches('/').is_empty() {
        "(root — everything)".to_string()
    } else {
        format!("'{}'", path.trim_matches('/'))
    }
}

/// Drop cached data so the next access loads fresh from the server. Diagnostic
/// aid: with end users it separates "stale cache" from "server-side" problems.
/// Credentials and config are never touched.
fn cmd_cache_clear(account: &Account, path: Option<&str>) -> anyhow::Result<()> {
    // Refuse while the account is mounted: the running daemon holds the state
    // DB and blob cache open, and deleting them under a live mount corrupts
    // the session. (Only detectable on Linux — the only platform that mounts —
    // via /proc/self/mountinfo; elsewhere the printed hint below remains.)
    {
        let configured = account_mount_point(account);
        let configured = std::fs::canonicalize(&configured).unwrap_or(configured);
        let marker = read_mount_marker(account);
        let (_, ours) = read_active_mounts();
        if let Some(live) = live_mount_for_account(&configured, marker.as_deref(), &ours) {
            bail!(
                "account '{}' is currently mounted at {} — unmount first \
                 (`wusel service disable{}` or stop the running `wusel mount`)",
                account.name(),
                live.display(),
                account_flag(account),
            );
        }
    }
    // Normalise: `cache clear /` means the whole account too.
    let path = path.map(|p| p.trim_matches('/')).filter(|p| !p.is_empty());
    match path {
        None => {
            // Whole account: the state DB (metadata) and the cache directory
            // (content blobs + scratch) — the next mount starts like a fresh
            // connection. Pins are NOT touched: they are what the user said,
            // not something we fetched, and somebody clearing space before a
            // trip is exactly who must not lose them.
            let mut freed = 0u64;
            let db = account.state_db_path();
            for suffix in ["", "-wal", "-shm"] {
                let mut p = db.as_os_str().to_owned();
                p.push(suffix);
                let p = std::path::PathBuf::from(p);
                freed += p.metadata().map(|m| m.len()).unwrap_or(0);
                let _ = std::fs::remove_file(&p);
            }
            let cache = account.cache_dir();
            freed += dir_size(&cache);
            let _ = std::fs::remove_dir_all(&cache);
            println!(
                "Cleared all cached data for account '{}' ({} freed).",
                account.name(),
                fmt_bytes(freed)
            );
            println!(
                "Metadata and content cache are gone — the next mount starts like a \
                 fresh connection. Credentials, config and pins are kept."
            );
            match open_pins(account).and_then(|p| p.all().map_err(Into::into)) {
                Ok(pins) if !pins.is_empty() => println!(
                    "{} pin(s) remain; their files download again on first use \
                     (`wusel pins` to see them, `wusel unpin` to drop one).",
                    pins.len()
                ),
                _ => {}
            }
        }
        Some(p) => {
            let db_path = account.state_db_path();
            if !db_path.exists() {
                println!("Nothing cached yet.");
                return Ok(());
            }
            let mut state = wusel_core::state::StateDb::open(&db_path)
                .context("could not open the state database")?;
            let Some(node) = state.node_by_path(p)? else {
                bail!("'{p}' is not in the cache (nothing to clear)");
            };
            // Content first (the ids are gone from the DB after the forget).
            let blob_dir = account.blob_cache_dir();
            let mut files = 0u64;
            let mut freed = 0u64;
            for (_, file_id) in state.descendant_file_ids(p)? {
                let mut removed_any = false;
                for ext in ["", ".etag", ".pin", ".ra"] {
                    let blob = blob_dir.join(format!("{file_id}{ext}"));
                    if let Ok(m) = blob.metadata() {
                        freed += m.len();
                        removed_any |= std::fs::remove_file(&blob).is_ok();
                    }
                }
                files += removed_any as u64;
            }
            if node.is_dir {
                let dropped = state.forget_children(node.inode)?;
                println!(
                    "Forgot {dropped} cached entr{} under '{p}' and removed {files} cached \
                     file(s) ({}).",
                    if dropped == 1 { "y" } else { "ies" },
                    fmt_bytes(freed)
                );
            } else {
                println!(
                    "Removed the cached content of '{p}' ({}).",
                    fmt_bytes(freed)
                );
            }
            println!("The next access loads fresh from the server.");
        }
    }
    println!("(Run this while the account is not mounted.)");
    Ok(())
}

/// Recursive on-disk size of a directory (0 if absent).
fn dir_size(dir: &std::path::Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Human-readable byte count (binary units).
fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// A pin/unpin target's account and remote (account-relative) path.
///
/// Two forms are accepted so the same command serves both the CLI and the
/// file-manager context menu:
/// * a *relative* path (or empty = the whole account) is a remote path on the
///   given `account` — the CLI form (`wusel pin Photos`);
/// * an *absolute* path is an on-disk path a file manager passed; we find the
///   account whose mountpoint contains it and derive the remote path.
fn resolve_pin_target(account: &Account, path: &str) -> anyhow::Result<(Account, String)> {
    if !path.starts_with('/') {
        return Ok((
            Account::new(account.name()),
            path.trim_matches('/').to_string(),
        ));
    }
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| std::path::PathBuf::from(path));
    for name in config::list_accounts() {
        let acc = Account::new(&name);
        let mount = account_mount_point(&acc);
        let mount = std::fs::canonicalize(&mount).unwrap_or(mount);
        if let Ok(rel) = abs.strip_prefix(&mount) {
            return Ok((acc, rel.to_string_lossy().trim_matches('/').to_string()));
        }
    }
    bail!("'{path}' is not inside a wusel mount (nothing to pin)");
}

/// The live wusel mount belonging to this account, if any — the decision behind
/// the `cache clear` guard, kept pure so it is testable without a real mount.
///
/// Two candidate paths, because the daemon need not sit where the config says:
/// `wusel mount /srv/cloud` overrides both `[mount] point` and the default. So
/// we consult the *marker* the mounting daemon wrote (its real mountpoint, see
/// [`mount_marker_path`]) as well as the `configured` path from
/// [`account_mount_point`], and refuse if either is currently a wusel mount.
///
/// `ours` — the active wusel mountpoints from `/proc/self/mountinfo` — stays the
/// single authority on *liveness*; the marker only says where to look. That is
/// what makes a **stale marker harmless**: after a crash (SIGKILL, OOM, power
/// loss) the file survives, but the kernel no longer reports a mount there, so
/// the guard proceeds instead of blocking `cache clear` forever. The reverse
/// error — a marker lost while the daemon runs — cannot happen, since only the
/// daemon itself removes it, on clean exit.
fn live_mount_for_account(
    configured: &std::path::Path,
    marker: Option<&std::path::Path>,
    ours: &[std::path::PathBuf],
) -> Option<std::path::PathBuf> {
    marker
        .into_iter()
        .chain(std::iter::once(configured))
        .find(|cand| ours.iter().any(|m| m.as_path() == *cand))
        .map(|p| p.to_path_buf())
}

/// Where a mounting daemon records the mountpoint it actually used, so other
/// commands of the same account can find it even when it came from the command
/// line. It lives in the account's state dir (per-account by construction, and
/// already the daemon's own directory) and holds a single path.
fn mount_marker_path(account: &Account) -> std::path::PathBuf {
    account.state_dir().join("mountpoint")
}

/// The mountpoint a running daemon recorded for this account, if any. Never an
/// error: an absent or unreadable marker simply means "nothing recorded".
fn read_mount_marker(account: &Account) -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string(mount_marker_path(account)).ok()?;
    let path = text.trim();
    (!path.is_empty()).then(|| std::path::PathBuf::from(path))
}

/// Record/clear this daemon's actual mountpoint. Both are fail-soft: the marker
/// is an aid for other commands, never a precondition for mounting, so a
/// read-only or full state dir must not keep the user from mounting.
#[cfg(feature = "fuse")]
fn write_mount_marker(account: &Account, target: &std::path::Path) {
    let path = mount_marker_path(account);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, target.to_string_lossy().as_bytes()) {
        tracing::debug!(%e, path = %path.display(), "could not record the mountpoint marker");
    }
}

#[cfg(feature = "fuse")]
fn remove_mount_marker(account: &Account) {
    let _ = std::fs::remove_file(mount_marker_path(account));
}

/// Where an account mounts: the `config.toml` override, else its default.
fn account_mount_point(account: &Account) -> std::path::PathBuf {
    account
        .settings()
        .mount_point
        .unwrap_or_else(|| account.default_mountpoint())
}

/// Pin a path and hydrate it now (a directory recursively).
fn cmd_pin(account: &Account, path: &str) -> anyhow::Result<()> {
    let (account, remote) = resolve_pin_target(account, path)?;
    let mut provider = build_provider(&account)?;
    let count = provider.pin(&remote).context("could not pin")?;
    announce_emblem(&account, &remote);
    println!(
        "Pinned {} — {count} file(s) downloaded and kept offline.",
        pin_label(&remote)
    );
    Ok(())
}

/// Bring pinned files that have gone out of date back in step with the server.
///
/// Not "unpin and pin again": that would drop the eviction marker first, so a
/// failed re-download would leave the file outdated *and* unprotected. This
/// re-fetches in place, so a failure leaves exactly what was there.
fn cmd_update(account: &Account, path: &str) -> anyhow::Result<()> {
    let (account, remote) = resolve_pin_target(account, path)?;
    let mut provider = build_provider(&account)?;
    let count = provider
        .refresh(&remote)
        .context("could not bring the pinned copy up to date")?;
    if count == 0 {
        println!("{} is already up to date.", pin_label(&remote));
    } else {
        println!(
            "Updated {} — {count} file(s) re-downloaded.",
            pin_label(&remote)
        );
    }
    Ok(())
}

/// Remove a pin; its files become normal (evictable) cache entries again.
fn cmd_unpin(account: &Account, path: &str) -> anyhow::Result<()> {
    let (account, remote) = resolve_pin_target(account, path)?;
    let mut provider = build_provider(&account)?;
    provider.unpin(&remote).context("could not unpin")?;
    announce_emblem(&account, &remote);
    println!("Unpinned {}.", pin_label(&remote));
    Ok(())
}

/// Tell the desktop this path's emblem is out of date.
///
/// Whoever *performs* the change announces it, and pinning is performed here —
/// `wusel pin` and the Nautilus menu entry both run this binary, in their own
/// process, while the daemon holds the mount. The daemon's own channel cannot
/// help: nothing in it knows this happened.
///
/// Best-effort by design. A file manager that is not running, a session bus
/// that is not there, a headless machine — none of that should make an unpin
/// fail. The state is already correct either way; this is only how quickly it
/// is *shown*.
fn announce_emblem(account: &Account, remote: &str) {
    let settings = account.settings();
    let mount = settings
        .mount_point
        .clone()
        .unwrap_or_else(|| account.default_mountpoint());
    let desktop = wusel_desktop::backend(account.name(), &mount, settings.notify_hook.as_deref());
    desktop.file_changed(&mount.join(remote).to_string_lossy());
}

/// Run the GNOME Shell search provider: forward queries to Nextcloud Unified
/// Search and open a chosen result in the local mount (else the web UI). Blocks
/// until the process is stopped; GNOME Shell D-Bus-activates it on demand.
fn cmd_search_provider(account: &Account) -> anyhow::Result<()> {
    let creds = wusel_core::credentials::load(&account.credentials_path(), account.name())
        .with_context(|| {
            format!(
                "no credentials — run `wusel login{} <url>` first",
                account_flag(account)
            )
        })?;
    let settings = account.settings();
    let http = build_http_client(&settings.tls)?;
    let mount = account_mount_point(account);
    let rt = std::sync::Arc::new(tokio::runtime::Runtime::new()?);

    let (server, login, password) = (
        creds.server.clone(),
        creds.login_name.clone(),
        creds.app_password.clone(),
    );
    let search = move |term: &str| match rt.block_on(wusel_core::search::unified_search(
        &http, &server, &login, &password, term,
    )) {
        Ok(hits) => hits,
        Err(e) => {
            tracing::debug!(%e, "unified search failed");
            Vec::new()
        }
    };

    let web_base = creds.server.trim_end_matches('/').to_string();
    let activate = move |hit: &wusel_core::search::SearchHit| {
        // Local-first: open the mount copy if the path resolves and exists.
        if let Some(rel) = &hit.rel_path {
            let local = mount.join(rel);
            if local.exists() {
                open_uri(&local.to_string_lossy());
                return;
            }
        }
        let url = if hit.resource_url.starts_with("http") {
            hit.resource_url.clone()
        } else {
            format!("{web_base}/{}", hit.resource_url.trim_start_matches('/'))
        };
        open_uri(&url);
    };

    wusel_desktop::run_search_provider(search, activate).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Open a path or URL with the desktop's default handler (fire-and-forget).
fn open_uri(target: &str) {
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(target);
    if let Err(e) = spawn_and_reap(cmd) {
        tracing::debug!(%e, target, "xdg-open failed");
    }
}

/// Spawn a fire-and-forget helper **and reap it**. Returns the child's pid.
///
/// A child nobody waits for stays in the process table as a zombie until its
/// parent exits. For a one-shot CLI that is invisible; `search-provider`,
/// though, is a long-lived D-Bus service, so every activated search result
/// would leave one behind for the whole session. We therefore hand the child to
/// a tiny thread that blocks in `wait()` — the caller stays fire-and-forget
/// (the D-Bus thread must not wait on `xdg-open`), and the entry is collected
/// as soon as the helper exits. A thread per activation is affordable: it is a
/// user-initiated, rare event, and the thread lives only as long as the helper.
fn spawn_and_reap(mut cmd: std::process::Command) -> std::io::Result<u32> {
    let mut child = cmd.spawn()?;
    let pid = child.id();
    if let Err(e) = std::thread::Builder::new()
        .name("wusel-reap".into())
        .spawn(move || {
            let _ = child.wait();
        })
    {
        // Out of threads: the helper still runs, we just cannot collect it.
        tracing::debug!(%e, pid, "could not spawn the reaper thread");
    }
    Ok(pid)
}

/// List the pins for an account.
///
/// Reads the pins file and nothing else — no database, so it answers on a
/// machine whose cache has been cleared, and it is the same answer a running
/// daemon would give.
fn cmd_pins(account: &Account) -> anyhow::Result<()> {
    let pins = open_pins(account)?.all().context("could not read pins")?;
    if pins.is_empty() {
        println!("No pins.");
        return Ok(());
    }
    println!("Pinned:");
    for (path, is_dir) in pins {
        let label = if path.is_empty() {
            "(root — everything)".to_string()
        } else if is_dir {
            format!("{path}/")
        } else {
            path
        };
        println!("  {label}");
    }
    Ok(())
}

/// Lists the accounts that have stored credentials.
fn cmd_accounts_list() -> anyhow::Result<()> {
    let names = config::list_accounts();
    if names.is_empty() {
        println!("No accounts yet — run `wusel login <server-url>` to add one.");
        return Ok(());
    }
    println!("Configured accounts:");
    for name in names {
        println!("  {name}");
    }
    Ok(())
}

/// Removes a named account: its credentials, state and cache. The files on the
/// server are untouched; a running mount must be stopped separately.
fn cmd_account_remove(name: &str) -> anyhow::Result<()> {
    let account = Account::new(name);
    if account.is_default() {
        bail!("the default account cannot be removed here — remove ~/.config/wusel by hand if you really mean to");
    }
    if !account.credentials_path().exists() {
        bail!("no such account: {}", account.name());
    }
    for dir in [
        account.config_dir(),
        account.state_dir(),
        account.cache_dir(),
    ] {
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("could not remove {}", dir.display()))?;
        }
    }
    println!(
        "Removed account '{}' (credentials, state and cache).",
        account.name()
    );
    println!("Note: stop a running instance first — this does not unmount it.");
    Ok(())
}

/// The `--account NAME` fragment for help text (empty for the default account).
fn account_flag(account: &Account) -> String {
    if account.is_default() {
        String::new()
    } else {
        format!(" --account {}", account.name())
    }
}

/// Builds the shared HTTP client from the TLS settings, warning loudly if
/// certificate verification has been disabled.
fn build_http_client(tls: &config::TlsSettings) -> anyhow::Result<reqwest::Client> {
    if tls.insecure {
        tracing::warn!(
            "TLS certificate verification is DISABLED (tls.insecure = true) — \
             connections are not authenticated. Use only on trusted networks."
        );
    }
    wusel_core::tls::client(tls).context("could not build the HTTP client")
}

fn init_tracing() {
    // Default: INFO — the production narrative (mount/unmount, uploads, push,
    // warnings/errors) without per-operation noise. Diagnostics are opt-in via
    // RUST_LOG (debug: requests + FUSE narrative; trace: per-read cache hits) —
    // see the troubleshooting docs.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "wusel=info,wusel_core=info,wusel_fuse=info,wusel_desktop=info".into()
            }),
        )
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_is_templated_and_uses_the_given_binary() {
        let u = unit_contents("/usr/bin/wusel").unwrap();
        assert!(u.contains("ExecStart=\"/usr/bin/wusel\" mount --account %i"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(u.contains("Restart=on-failure"));
    }

    #[test]
    fn exec_path_survives_spaces_and_percent() {
        // Spaces must be quoted (systemd splits ExecStart into words) and `%`
        // doubled (it introduces a systemd specifier, even inside quotes).
        let u = unit_contents("/opt/my tools/100% wusel/wusel").unwrap();
        assert!(u.contains("ExecStart=\"/opt/my tools/100%% wusel/wusel\" mount --account %i"));
    }

    #[test]
    fn a_control_character_in_the_exec_path_cannot_inject_a_directive() {
        // A unit file is line-oriented: a raw newline in the path ends the
        // ExecStart= line and everything after it becomes a directive of its
        // own. (Only reachable by someone who already controls the binary's
        // path, but the unit must never be forgeable from it.)
        let err = unit_contents("/opt/w\nExecStart=/bin/false")
            .expect_err("a path with a newline must be refused");
        assert!(err.to_string().contains("control character"), "{err}");
        // Every control character, not just the newline (NUL, tab, ESC …).
        assert!(systemd_exec_escape("/opt/w\u{0}usel").is_err());
        assert!(systemd_exec_escape("/opt/w\tusel").is_err());
        // …and an ordinary path still passes.
        assert_eq!(
            systemd_exec_escape("/usr/bin/wusel").unwrap(),
            "\"/usr/bin/wusel\""
        );
    }

    #[test]
    fn an_account_name_that_is_no_valid_unit_instance_is_rejected() {
        // A unit name systemd cannot parse must produce a clear error here, not
        // an opaque `systemctl` failure later.
        assert!(instance_name("work/2").is_err());
        assert!(instance_name("").is_err());
        // The case that really reaches us: `Account::new` filters with the
        // Unicode-aware `char::is_alphanumeric`, so a non-ASCII name survives
        // it — but systemd's unit names are ASCII-only.
        assert_eq!(Account::new("Müller").name(), "Müller");
        assert!(service_instance(&Account::new("Müller")).is_err());
        // Path-hostile characters are already mapped to `_` upstream; the
        // instance name follows that sanitized name.
        assert_eq!(
            service_instance(&Account::new("work/2")).unwrap(),
            "wusel@work_2.service"
        );
        assert_eq!(
            service_instance(&Account::new("work-2")).unwrap(),
            "wusel@work-2.service"
        );
    }

    #[test]
    fn a_packaged_unit_is_not_shadowed_by_an_override() {
        // With an RPM-packaged unit present, `service enable` must not drop a
        // user-level copy on top of it: that copy would pin ExecStart to the
        // binary that happened to run the command (a dev build).
        assert_eq!(unit_action(true, false), UnitAction::UsePackaged);
        assert_eq!(unit_action(true, true), UnitAction::KeepOverride);
        // Nothing packaged (cargo install, dev build): write our own.
        assert_eq!(unit_action(false, false), UnitAction::Write);
    }

    #[test]
    fn the_status_output_names_the_unit_in_effect() {
        let user = p("/home/u/.config/systemd/user/wusel@.service");
        let pkg = p("/usr/lib/systemd/user/wusel@.service");
        let shadowed = unit_in_effect(Some(&user), Some(&pkg));
        assert!(shadowed.contains("/home/u/.config/systemd/user/wusel@.service"));
        assert!(shadowed.contains("/usr/lib/systemd/user/wusel@.service"));
        assert!(unit_in_effect(None, Some(&pkg)).contains("packaged"));
        assert!(unit_in_effect(None, None).contains("service enable"));
    }

    #[test]
    fn a_spawned_helper_is_reaped() {
        // `open_uri` fires xdg-open and forgets it. In the long-lived
        // search-provider daemon an unreaped child stays a zombie in the
        // process table for the life of the session — one per activated hit.
        let pid = spawn_and_reap(std::process::Command::new("true")).expect("spawn");
        for _ in 0..200 {
            if !process_exists(pid) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("pid {pid} is still in the process table — the child was never reaped");
    }

    /// Is `pid` still a process (a zombie counts — it is reaped only by a
    /// `wait`)? `ps -p` is the portable answer on both macOS and Linux.
    fn process_exists(pid: u32) -> bool {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn exec_escape_handles_quotes_and_backslashes() {
        assert_eq!(
            systemd_exec_escape(r#"/odd/pa"th/w\usel"#).unwrap(),
            r#""/odd/pa\"th/w\\usel""#
        );
    }

    // --- `cache clear` mount guard ------------------------------------------

    fn p(s: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(s)
    }

    #[test]
    fn cache_clear_refuses_while_mounted_at_the_configured_point() {
        // The plain case: no marker (an older daemon, or none was writable) and
        // the mount sits where the config says.
        let ours = vec![p("/home/u/Wusel")];
        assert_eq!(
            live_mount_for_account(&p("/home/u/Wusel"), None, &ours),
            Some(p("/home/u/Wusel"))
        );
    }

    #[test]
    fn cache_clear_refuses_while_mounted_at_an_explicit_point() {
        // `wusel mount /srv/cloud` overrides config and default, so the live
        // daemon sits at /srv/cloud while the configured point is ~/Wusel.
        // Clearing the cache would pull the state DB out from under it.
        let ours = vec![p("/srv/cloud")];
        assert_eq!(
            live_mount_for_account(&p("/home/u/Wusel"), Some(&p("/srv/cloud")), &ours),
            Some(p("/srv/cloud"))
        );
    }

    #[test]
    fn cache_clear_proceeds_when_not_mounted() {
        let ours = vec![p("/home/other/Wusel-work")];
        assert_eq!(
            live_mount_for_account(&p("/home/u/Wusel"), None, &ours),
            None
        );
    }

    #[test]
    fn a_stale_marker_does_not_block_cache_clear() {
        // The daemon was killed, so its marker survived — but the kernel lists
        // no wusel mount any more. Liveness comes from the mount table, so the
        // clear proceeds instead of being blocked forever.
        assert_eq!(
            live_mount_for_account(&p("/home/u/Wusel"), Some(&p("/srv/cloud")), &[]),
            None
        );
    }

    #[test]
    fn a_stale_marker_still_protects_a_mount_at_the_configured_point() {
        // Marker left over from an earlier `mount /srv/cloud`, while a fresh
        // daemon runs at the configured point: the configured path is checked
        // too, so we still refuse.
        let ours = vec![p("/home/u/Wusel")];
        assert_eq!(
            live_mount_for_account(&p("/home/u/Wusel"), Some(&p("/srv/cloud")), &ours),
            Some(p("/home/u/Wusel"))
        );
    }
}
