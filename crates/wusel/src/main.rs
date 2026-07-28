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
        format!(
            "no credentials at {} — run `wusel login{} <server-url>` first",
            creds_path.display(),
            account_flag(account),
        )
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

    let http = build_http_client(&settings.tls)?;
    let dav = wusel_core::webdav::WebDavClient::new(
        http,
        &creds.server,
        &creds.login_name,
        &creds.app_password,
    );

    let db_path = account.state_db_path();
    if let Some(dir) = db_path.parent() {
        std::fs::create_dir_all(dir).context("could not create the state directory")?;
    }
    let state =
        wusel_core::state::StateDb::open(&db_path).context("could not open the state database")?;

    let mut provider = wusel_core::provider::Provider::new(dav, state, account)
        .context("could not initialise the provider")?;

    // Instant cache invalidation over notify_push; degrades to TTL if absent.
    let _push = wusel_core::push::spawn(
        &creds.server,
        &creds.login_name,
        &creds.app_password,
        settings.tls.clone(),
        provider.invalidation_handle(),
        provider.sync_trigger(),
    );

    // The daemon owns the mountpoint, so a systemd unit stays trivial.
    std::fs::create_dir_all(mountpoint).ok();
    let target = mountpoint
        .canonicalize()
        .unwrap_or_else(|_| mountpoint.to_path_buf());

    // Plug in the platform OS-integration backend (Linux notifications + the
    // file-manager cloud-provider status for this account's mountpoint; a no-op
    // elsewhere or when D-Bus is absent — fail-soft).
    provider.set_desktop(wusel_desktop::backend(account.name(), &target));

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

    wusel_fuse::mount(&target, provider)
}

/// Warn (do not block) if another account already syncs the same server + user:
/// harmless while read-only, but a footgun once writing lands.
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
                     instance twice is redundant and will churn once writing lands"
                );
            }
        }
    }
}

/// Active mounts from `/proc/self/mountinfo`: (all mountpoints, ours = wusel).
/// Gated on Linux only (not on the `fuse` feature): `cmd_cache_clear` needs it
/// too, and even a non-fuse build must recognise a mount made by another build.
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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
fn systemd_exec_escape(path: &str) -> String {
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
    out
}

/// The templated unit's contents. `%i` is the account name, filled in by systemd
/// when the instance `wusel@<account>` is started. `exec` is this binary's
/// absolute path, so it works for packaged, `cargo install`ed and dev builds.
/// Keep this in sync with the packaged twin `packaging/rpm/wusel@.service`.
fn unit_contents(exec: &str) -> String {
    let exec = systemd_exec_escape(exec);
    format!(
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
    )
}

/// Manages the systemd *user* service for an account (`wusel@<account>`).
/// Linux-only in practice (fails cleanly elsewhere: `systemctl` is absent).
fn cmd_service(account: &Account, action: ServiceCmd) -> anyhow::Result<()> {
    let instance = format!("wusel@{}.service", account.name());
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
            // `status` prints to stdout itself; ignore its non-zero exit codes.
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "--no-pager", "status", &instance])
                .status();
        }
    }
    Ok(())
}

/// Write the templated unit into the user's systemd dir and reload.
fn install_unit() -> anyhow::Result<()> {
    let exec = std::env::current_exe().context("could not determine own binary path")?;
    let dir = systemd_user_dir();
    std::fs::create_dir_all(&dir).context("could not create the systemd user dir")?;
    let path = dir.join("wusel@.service");
    std::fs::write(&path, unit_contents(&exec.display().to_string()))
        .with_context(|| format!("could not write {}", path.display()))?;
    systemctl(&["daemon-reload"])?;
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
    let dav = wusel_core::webdav::WebDavClient::new(
        http,
        &creds.server,
        &creds.login_name,
        &creds.app_password,
    );
    let db_path = account.state_db_path();
    if let Some(dir) = db_path.parent() {
        std::fs::create_dir_all(dir).context("could not create the state directory")?;
    }
    let state =
        wusel_core::state::StateDb::open(&db_path).context("could not open the state database")?;
    wusel_core::provider::Provider::new(dav, state, account)
        .context("could not initialise the provider")
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
    // the session. (Only checkable on Linux — the only platform that mounts —
    // via /proc/self/mountinfo; elsewhere the printed hint below remains.)
    #[cfg(target_os = "linux")]
    {
        let mount = account_mount_point(account);
        let mount = std::fs::canonicalize(&mount).unwrap_or(mount);
        let (_, ours) = read_active_mounts();
        if ours.contains(&mount) {
            bail!(
                "account '{}' is currently mounted at {} — unmount first \
                 (`wusel service disable{}` or stop the running `wusel mount`)",
                account.name(),
                mount.display(),
                account_flag(account),
            );
        }
    }
    // Normalise: `cache clear /` means the whole account too.
    let path = path.map(|p| p.trim_matches('/')).filter(|p| !p.is_empty());
    match path {
        None => {
            // Whole account: state DB (metadata + pins) and the cache directory
            // (content blobs + scratch) — the next mount starts like a fresh
            // connection.
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
                "Metadata, content cache and pins are gone — the next mount starts like a \
                 fresh connection. Credentials and config are kept."
            );
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
    println!(
        "Pinned {} — {count} file(s) downloaded and kept offline.",
        pin_label(&remote)
    );
    Ok(())
}

/// Remove a pin; its files become normal (evictable) cache entries again.
fn cmd_unpin(account: &Account, path: &str) -> anyhow::Result<()> {
    let (account, remote) = resolve_pin_target(account, path)?;
    let mut provider = build_provider(&account)?;
    provider.unpin(&remote).context("could not unpin")?;
    println!("Unpinned {}.", pin_label(&remote));
    Ok(())
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
    if let Err(e) = std::process::Command::new("xdg-open").arg(target).spawn() {
        tracing::debug!(%e, target, "xdg-open failed");
    }
}

/// List the pins for an account (reads only the state DB).
fn cmd_pins(account: &Account) -> anyhow::Result<()> {
    let db_path = account.state_db_path();
    if !db_path.exists() {
        println!("No pins.");
        return Ok(());
    }
    let state =
        wusel_core::state::StateDb::open(&db_path).context("could not open the state database")?;
    let pins = state.pins().context("could not read pins")?;
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
        let u = unit_contents("/usr/bin/wusel");
        assert!(u.contains("ExecStart=\"/usr/bin/wusel\" mount --account %i"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(u.contains("Restart=on-failure"));
    }

    #[test]
    fn exec_path_survives_spaces_and_percent() {
        // Spaces must be quoted (systemd splits ExecStart into words) and `%`
        // doubled (it introduces a systemd specifier, even inside quotes).
        let u = unit_contents("/opt/my tools/100% wusel/wusel");
        assert!(u.contains("ExecStart=\"/opt/my tools/100%% wusel/wusel\" mount --account %i"));
    }

    #[test]
    fn exec_escape_handles_quotes_and_backslashes() {
        assert_eq!(
            systemd_exec_escape(r#"/odd/pa"th/w\usel"#),
            r#""/odd/pa\"th/w\\usel""#
        );
    }
}
