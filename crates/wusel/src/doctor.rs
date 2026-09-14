// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! `wusel doctor` — one command that gathers the diagnostics a support case
//! needs, so nobody has to be talked through `cat /sys/fs/fuse/…` on the phone.
//!
//! It runs the probes we once ran by hand — the FUSE connection's `waiting`
//! count, the daemon's per-thread kernel wait-channels, the mount table — and
//! asks the running daemon for its internal state over the diagnostics socket
//! (see [`wusel_core::diag`]). Everything is best-effort: a probe that cannot
//! run records why and the rest go on, because the point is to collect what is
//! available even on a broken system.
//!
//! Privacy is the other half of the job. The engine snapshot is name-free by
//! construction; no directory of the mount is ever listed; secrets are never
//! read (only credential *metadata* — the server and login, never the
//! password); and free text has the home path and username redacted. The header
//! states exactly what is and is not redacted, and `--include-listing` turns the
//! redaction off for a consenting deep dive.
//!
//! Compiles everywhere; the `/proc` and `/sys` probes simply come back
//! "unavailable" off Linux, which is where the mount runs anyway.

use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;
use wusel_core::config::Account;
use wusel_core::diag::DiagReport;

/// How `doctor` was asked to run.
pub struct Options {
    pub account: String,
    /// `-o PREFIX`: also write `PREFIX.txt` and `PREFIX.json`.
    pub output: Option<PathBuf>,
    /// Turn redaction off — full paths and names, for a consenting deep dive.
    pub include_listing: bool,
    /// Omit the journal tail (which is English and technical, but may name
    /// paths).
    pub no_logs: bool,
}

/// Collect everything, print the human report, and write the files if asked.
///
/// # Errors
/// Only if writing an output file fails; collection itself never errors, it
/// records what it could not do.
pub fn run(opts: &Options) -> Result<()> {
    let report = Report::collect(opts);
    let text = report.to_text();
    println!("{text}");
    if let Some(prefix) = &opts.output {
        let txt = with_ext(prefix, "txt");
        let json = with_ext(prefix, "json");
        std::fs::write(&txt, text.as_bytes())?;
        std::fs::write(&json, serde_json::to_string_pretty(&report)?.as_bytes())?;
        eprintln!("\nwrote {} and {}", txt.display(), json.display());
    }
    Ok(())
}

fn with_ext(prefix: &Path, ext: &str) -> PathBuf {
    let mut s = prefix.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

// --- Redaction --------------------------------------------------------------

/// Rewrites free text so a bundle can be shared: the home path becomes `$HOME`
/// and the username `<user>`. Off when `--include-listing` was given.
struct Redactor {
    home: Option<String>,
    user: Option<String>,
    enabled: bool,
}

impl Redactor {
    fn new(enabled: bool) -> Self {
        let home = std::env::var("HOME").ok().filter(|s| s.len() > 1);
        let user = std::env::var("USER").ok().filter(|s| !s.is_empty());
        Self {
            home,
            user,
            enabled,
        }
    }

    /// Apply the substitutions. A no-op when redaction is off.
    fn s(&self, text: &str) -> String {
        if !self.enabled {
            return text.to_string();
        }
        let mut out = text.to_string();
        if let Some(home) = &self.home {
            out = out.replace(home, "$HOME");
        }
        if let Some(user) = &self.user {
            // Word-ish boundaries would be safer, but a plain replace is the
            // honest, predictable rule — and the header says it is applied.
            out = out.replace(user.as_str(), "<user>");
        }
        out
    }

    fn path(&self, p: &Path) -> String {
        self.s(&p.to_string_lossy())
    }
}

// --- The report -------------------------------------------------------------

#[derive(Serialize)]
struct Report {
    tool: Tool,
    findings: Vec<Finding>,
    system: System,
    daemon: Daemon,
    mount: Mount,
    engine: Engine,
    recheck: Recheck,
    config: ConfigInfo,
    connectivity: Connectivity,
    #[serde(skip_serializing_if = "Option::is_none")]
    logs: Option<Vec<String>>,
}

#[derive(Serialize)]
struct Tool {
    name: &'static str,
    version: &'static str,
    account: String,
    redacted: bool,
}

#[derive(Serialize, Clone, Debug)]
struct Finding {
    level: &'static str, // PASS | INFO | WARN | FAIL
    section: &'static str,
    message: String,
}

fn finding(level: &'static str, section: &'static str, message: String) -> Finding {
    Finding {
        level,
        section,
        message,
    }
}

#[derive(Serialize, Default)]
struct System {
    uname: Option<String>,
    distro: Option<String>,
    desktop: Option<String>,
    session_type: Option<String>,
}

#[derive(Serialize, Default)]
struct Daemon {
    /// Every `wusel` process, so a stray one next to the mount daemon shows up.
    processes: Vec<Process>,
    /// The unit actually asked about. Recorded rather than assumed: the service
    /// is a *template*, and asking about the wrong name is how this section came
    /// to report a running mount as dead (see [`unit_for`]).
    unit: Option<String>,
    systemd: Option<String>,
}

#[derive(Serialize, Default)]
struct Process {
    pid: i32,
    cmdline: String,
    state: Option<String>,
    threads: Option<u32>,
    rss_kb: Option<u64>,
    /// The per-thread kernel wait-channels — the table that told an idle daemon
    /// from a working one. Only for the mount daemon; a search-provider has
    /// nothing interesting here.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    thread_waits: Vec<ThreadWait>,
}

#[derive(Serialize)]
struct ThreadWait {
    comm: String,
    wchan: String,
    syscall: String,
}

#[derive(Serialize, Default)]
struct Mount {
    entries: Vec<MountEntry>,
    connections: Vec<FuseConnection>,
    responsive: Option<String>, // "ok (…)" | "TIMEOUT" | "error: …"
}

#[derive(Serialize)]
struct MountEntry {
    mountpoint: String,
    fstype: String,
    source: String,
    connection: u32,
}

#[derive(Serialize)]
struct FuseConnection {
    id: u32,
    waiting: Option<u64>,
    max_background: Option<u64>,
    congestion_threshold: Option<u64>,
}

#[derive(Serialize)]
struct Engine {
    available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<DiagReport>,
}

/// The second look at the mount, taken [`RECHECK_AFTER`] later.
///
/// The wedge check needs it. Its first version compared two counters from a
/// single instant — the kernel's `waiting` against the daemon's parked replies —
/// and those two are equal for every request that is merely *in flight*: the
/// frontend parks a reply handle the moment a request arrives, long before
/// anything can go wrong with it. So the check fired on any busy mount, and the
/// branch that was meant to say "ordinary load" could only be reached in the
/// narrow race where the daemon had not parked the reply yet.
///
/// What separates a wedge from load is time, not arithmetic. Work that moves
/// between two samples is load; the *same* job still handed out to a worker
/// after a pause, or a parked reply with nothing running behind it, is a wedge.
#[derive(Serialize, Default)]
struct Recheck {
    /// False when the first pass found nothing in flight and nothing waiting, so
    /// no second sample was needed — an idle mount stays instant.
    taken: bool,
    /// How long after the first pass this was sampled.
    after_ms: u64,
    /// Whether the daemon answered the second time, so a daemon that died
    /// between the samples is not read as "the work finished".
    engine_answered: bool,
    waiting: Option<u64>,
    replies_pending: Option<usize>,
    /// One entry per job still handed out to a worker; see [`outstanding_keys`].
    outstanding: Vec<String>,
}

#[derive(Serialize, Default)]
struct ConfigInfo {
    server: Option<String>,
    login: Option<String>,
    mountpoint: Option<String>,
    dispatch_threads: Option<usize>,
    refresh_pinned: Option<String>,
    open_pinned: Option<String>,
    db_path: Option<String>,
    db_on_network: Option<bool>,
    pins: Option<usize>,
}

#[derive(Serialize, Default)]
struct Connectivity {
    server_host: Option<String>,
    tcp_reachable: Option<bool>,
    detail: Option<String>,
}

impl Report {
    fn collect(opts: &Options) -> Self {
        let red = Redactor::new(!opts.include_listing);
        let account = Account::new(&opts.account);
        let settings = account.settings();
        let mountpoint = settings
            .mount_point
            .clone()
            .unwrap_or_else(|| account.default_mountpoint());

        let unit = unit_for(&opts.account);
        let system = probe_system();
        let daemon = probe_daemon(unit.as_deref());
        let mount = probe_mount(&mountpoint, &red);
        let engine = probe_engine(&mountpoint);
        let recheck = probe_recheck(&mountpoint, &mount, &engine);
        let config = probe_config(&account, &settings, &mountpoint, &red);
        let connectivity = probe_connectivity(&config);
        let logs = if opts.no_logs {
            None
        } else {
            Some(probe_logs(unit.as_deref(), &red))
        };

        let mut findings = derive_findings(&daemon, &mount, &engine, &recheck, &config);
        findings.extend(push_findings(&engine, &connectivity));

        Report {
            tool: Tool {
                name: "wusel doctor",
                version: env!("CARGO_PKG_VERSION"),
                account: opts.account.clone(),
                redacted: !opts.include_listing,
            },
            findings,
            system,
            daemon,
            mount,
            engine,
            recheck,
            config,
            connectivity,
            logs,
        }
    }
}

// --- Probes -----------------------------------------------------------------

fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn run_cmd(prog: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(prog).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn probe_system() -> System {
    let distro = read_trim("/etc/os-release").and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("PRETTY_NAME="))
            .map(|v| v.trim_matches('"').to_string())
    });
    System {
        uname: run_cmd("uname", &["-a"]),
        distro,
        desktop: std::env::var("XDG_CURRENT_DESKTOP").ok(),
        session_type: std::env::var("XDG_SESSION_TYPE").ok(),
    }
}

fn probe_daemon(unit: Option<&str>) -> Daemon {
    let mut processes = Vec::new();
    for pid in wusel_pids() {
        let cmdline = read_trim(format!("/proc/{pid}/cmdline"))
            .map(|s| s.replace('\0', " ").trim().to_string())
            .unwrap_or_default();
        let status = read_trim(format!("/proc/{pid}/status")).unwrap_or_default();
        let field = |name: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name))
                .map(|v| v.trim().to_string())
        };
        // The full thread table is only worth capturing for the mount daemon.
        let thread_waits = if cmdline.contains(" mount") {
            thread_waits(pid)
        } else {
            Vec::new()
        };
        processes.push(Process {
            pid,
            cmdline,
            state: field("State:"),
            threads: field("Threads:").and_then(|v| v.parse().ok()),
            rss_kb: field("VmRSS:").and_then(|v| v.split_whitespace().next()?.parse().ok()),
            thread_waits,
        });
    }
    // Best-effort systemd view, for the account's *instance*. Asking about
    // "wusel" instead reports the bare template — which is never active — so a
    // running mount looked stopped. See [`unit_for`].
    let systemd = unit.and_then(|unit| {
        run_cmd(
            "systemctl",
            &[
                "--user",
                "show",
                "-p",
                "ActiveState,SubState,NRestarts",
                unit,
            ],
        )
        .map(|s| s.replace('\n', ", "))
    });
    Daemon {
        processes,
        unit: unit.map(str::to_string),
        systemd,
    }
}

/// The systemd unit for an account, or `None` when the account cannot name one.
///
/// The service is a template — `wusel@<account>.service` — and every probe here
/// used the bare template name `wusel`. systemd answers about that quite
/// happily: `ActiveState=inactive, SubState=dead`, for a mount that is running.
/// The journal probe had the same bug and was worse, because it failed
/// *silently*: `journalctl --user -u wusel` matches nothing, so every support
/// bundle carried "-- No entries --" and the most useful part of the report was
/// missing without saying so.
///
/// The naming rule lives in one place ([`crate::instance_name`]) and is reused
/// rather than repeated, so an account systemd cannot name is rejected here the
/// same way `wusel service enable` rejects it.
fn unit_for(account: &str) -> Option<String> {
    crate::instance_name(account).ok()
}

/// Every `wusel` process id. `pidof` first; a `/proc` scan as the fallback.
fn wusel_pids() -> Vec<i32> {
    if let Some(out) = run_cmd("pidof", &["wusel"]) {
        let pids: Vec<i32> = out
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if !pids.is_empty() {
            return pids;
        }
    }
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        if read_trim(format!("/proc/{pid}/comm")).as_deref() == Some("wusel") {
            pids.push(pid);
        }
    }
    pids
}

fn thread_waits(pid: i32) -> Vec<ThreadWait> {
    let mut waits = Vec::new();
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return waits;
    };
    for t in tasks.flatten() {
        let dir = t.path();
        let comm = read_trim(dir.join("comm")).unwrap_or_default();
        let wchan = read_trim(dir.join("wchan")).unwrap_or_else(|| "?".into());
        // The syscall file is "<nr> <args…>"; the number is enough to correlate.
        let syscall = read_trim(dir.join("syscall"))
            .and_then(|s| s.split_whitespace().next().map(str::to_string))
            .unwrap_or_else(|| "?".into());
        waits.push(ThreadWait {
            comm,
            wchan,
            syscall,
        });
    }
    waits.sort_by(|a, b| a.comm.cmp(&b.comm));
    waits
}

fn probe_mount(mountpoint: &Path, red: &Redactor) -> Mount {
    let mut entries = Vec::new();
    if let Some(table) = read_trim("/proc/self/mountinfo") {
        for line in table.lines() {
            // mountinfo: "… major:minor root mountpoint opts - fstype source …"
            let Some((pre, post)) = line.split_once(" - ") else {
                continue;
            };
            let pre: Vec<&str> = pre.split_whitespace().collect();
            let post: Vec<&str> = post.split_whitespace().collect();
            let (Some(devno), Some(mp)) = (pre.get(2), pre.get(4)) else {
                continue;
            };
            let (Some(fstype), Some(source)) = (post.first(), post.get(1)) else {
                continue;
            };
            let is_wusel = *source == "wusel" || fstype.starts_with("fuse") && *source == "wusel";
            if !is_wusel {
                continue;
            }
            let connection = devno
                .split_once(':')
                .and_then(|(_, m)| m.parse().ok())
                .unwrap_or(0);
            entries.push(MountEntry {
                mountpoint: red.s(mp),
                fstype: (*fstype).to_string(),
                source: (*source).to_string(),
                connection,
            });
        }
    }
    let connections = entries
        .iter()
        .map(|e| e.connection)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|id| {
            let base = format!("/sys/fs/fuse/connections/{id}");
            FuseConnection {
                id,
                waiting: read_trim(format!("{base}/waiting")).and_then(|s| s.parse().ok()),
                max_background: read_trim(format!("{base}/max_background"))
                    .and_then(|s| s.parse().ok()),
                congestion_threshold: read_trim(format!("{base}/congestion_threshold"))
                    .and_then(|s| s.parse().ok()),
            }
        })
        .collect();

    Mount {
        entries,
        connections,
        responsive: Some(probe_responsiveness(mountpoint)),
        // (the redactor is used above; kept in scope for clarity)
    }
}

/// Stat the mountpoint on a throwaway thread, so a wedged mount times out here
/// instead of hanging `doctor` itself. The thread may stay parked in the kernel
/// forever — acceptable for a one-shot command that is about to exit.
fn probe_responsiveness(mountpoint: &Path) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    let mp = mountpoint.to_path_buf();
    std::thread::spawn(move || {
        let start = Instant::now();
        let r = std::fs::read_dir(&mp).map(|it| it.count());
        let _ = tx.send((r.is_ok(), start.elapsed()));
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok((true, dt)) => format!("ok ({} ms)", dt.as_millis()),
        Ok((false, _)) => "error: could not read the mountpoint".into(),
        Err(_) => "TIMEOUT — the mount did not answer in 5 s (it may be wedged)".into(),
    }
}

fn probe_engine(mountpoint: &Path) -> Engine {
    let path = wusel_core::config::diag_socket_for_mount(mountpoint);
    match read_socket(&path) {
        Ok(raw) => match DiagReport::from_json(&raw) {
            Ok(report) => Engine {
                available: true,
                note: None,
                report: Some(report),
            },
            Err(e) => Engine {
                available: false,
                note: Some(format!(
                    "the daemon answered but the payload did not parse ({e}); raw: {}",
                    raw.chars().take(200).collect::<String>()
                )),
                report: None,
            },
        },
        Err(e) => Engine {
            available: false,
            note: Some(format!(
                "no diagnostics socket at {} ({e}) — the daemon may be stopped, or too old to serve one",
                path.display()
            )),
            report: None,
        },
    }
}

/// How long to wait before the second sample. Long enough that ordinary work has
/// moved on — a range GET or a PROPFIND against a healthy server is far shorter —
/// and short enough that `doctor` stays a command you run without thinking about
/// it. It is only ever paid when the first pass found something in flight.
const RECHECK_AFTER: Duration = Duration::from_secs(2);

/// The identity of a job handed out to a worker: which object, which intent,
/// which step. Two samples sharing one of these are looking at the *same* job,
/// and that — not any count — is what tells a wedge from a busy mount.
///
/// Name-free, like the snapshot it reads: an inode number and two work kinds.
fn outstanding_keys(report: Option<&DiagReport>) -> Vec<String> {
    let Some(report) = report else {
        return Vec::new();
    };
    report
        .machine
        .objects
        .iter()
        .filter(|o| o.outstanding)
        .map(|o| format!("inode {} {}/{}", o.object, o.intent, o.step))
        .collect()
}

/// Sample the mount a second time, so the wedge check has a before and an after.
/// See [`Recheck`] for why one sample cannot answer the question.
fn probe_recheck(mountpoint: &Path, mount: &Mount, engine: &Engine) -> Recheck {
    let waiting: u64 = mount.connections.iter().filter_map(|c| c.waiting).sum();
    let busy = !outstanding_keys(engine.report.as_ref()).is_empty();
    // Nothing waiting and nothing running: there is no wedge to rule out, so a
    // healthy mount is not made two seconds slower to prove it.
    if waiting == 0 && !busy {
        return Recheck::default();
    }
    std::thread::sleep(RECHECK_AFTER);
    let again = probe_engine(mountpoint);
    let waiting_now = mount
        .connections
        .iter()
        .filter_map(|c| read_trim(format!("/sys/fs/fuse/connections/{}/waiting", c.id)))
        .filter_map(|s| s.parse::<u64>().ok())
        .sum();
    Recheck {
        taken: true,
        after_ms: u64::try_from(RECHECK_AFTER.as_millis()).unwrap_or(u64::MAX),
        engine_answered: again.report.is_some(),
        waiting: Some(waiting_now),
        replies_pending: again.report.as_ref().and_then(|r| r.replies_pending),
        outstanding: outstanding_keys(again.report.as_ref()),
    }
}

fn read_socket(path: &Path) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    Ok(buf)
}

fn probe_config(
    account: &Account,
    settings: &wusel_core::config::Settings,
    mountpoint: &Path,
    red: &Redactor,
) -> ConfigInfo {
    // Credential *metadata* only: the server and login, never the password.
    let (server, login) = wusel_core::credentials::load_metadata(&account.credentials_path())
        .map(|(s, l)| (Some(s), Some(red.s(&l))))
        .unwrap_or((None, None));
    let db = account.db_location();
    let db_on_network = matches!(db, wusel_core::storage::DbLocation::Risky { .. });
    let pins = wusel_core::pins::Pins::new(&account.config_dir())
        .all()
        .ok()
        .map(|v| v.len());
    ConfigInfo {
        server,
        login,
        mountpoint: Some(red.path(mountpoint)),
        dispatch_threads: Some(settings.dispatch_threads),
        refresh_pinned: Some(format!("{:?}", settings.refresh_pinned)),
        open_pinned: Some(format!("{:?}", settings.open_pinned)),
        db_path: Some(red.path(db.path())),
        db_on_network: Some(db_on_network),
        pins,
    }
}

fn probe_connectivity(config: &ConfigInfo) -> Connectivity {
    let Some(server) = &config.server else {
        return Connectivity::default();
    };
    // Parse host:port out of the URL without pulling in a URL crate.
    let rest = server
        .strip_prefix("https://")
        .or_else(|| server.strip_prefix("http://"))
        .unwrap_or(server);
    let is_http = server.starts_with("http://");
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse().unwrap_or(if is_http { 80 } else { 443 }),
        ),
        None => (authority.to_string(), if is_http { 80 } else { 443 }),
    };
    let mut conn = Connectivity {
        server_host: Some(host.clone()),
        ..Connectivity::default()
    };
    // A timed TCP connect: DNS + reachability + port open. Not a full HTTP or
    // auth check — deliberately, since doctor holds no credentials.
    let started = Instant::now();
    let reachable = (host.as_str(), port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .map(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(5)).is_ok())
        .unwrap_or(false);
    conn.tcp_reachable = Some(reachable);
    conn.detail = Some(if reachable {
        format!(
            "TCP connect to {host}:{port} ok ({} ms)",
            started.elapsed().as_millis()
        )
    } else {
        format!("TCP connect to {host}:{port} failed")
    });
    conn
}

fn probe_logs(unit: Option<&str>, red: &Redactor) -> Vec<String> {
    let Some(unit) = unit else {
        return vec!["(this account cannot name a systemd unit — no journal to read)".into()];
    };
    let lines: Vec<String> = run_cmd(
        "journalctl",
        &[
            "--user",
            "-u",
            unit,
            "-n",
            "200",
            "--no-pager",
            "-o",
            "short-iso",
        ],
    )
    .map(|s| s.lines().map(|l| red.s(l)).collect())
    .unwrap_or_default();
    // `journalctl` answers a unit it has never heard of with a literal
    // "-- No entries --" and exit 0 — indistinguishable, to a naive check, from
    // a daemon that has simply been quiet. That is precisely how the wrong unit
    // name stayed invisible, so the sentinel is treated as nothing.
    let meaningful = lines
        .iter()
        .any(|l| !matches!(l.trim(), "" | "-- No entries --"));
    if !meaningful {
        // Say which unit came up empty. The previous version asked about a name
        // that can never match and reported the result as though the daemon had
        // simply been quiet.
        return vec![format!(
            "(no journal entries for {unit} — the unit may never have been started, or the daemon runs outside systemd)"
        )];
    }
    lines
}

// --- Findings: the PASS/WARN/FAIL layer -------------------------------------

fn derive_findings(
    daemon: &Daemon,
    mount: &Mount,
    engine: &Engine,
    recheck: &Recheck,
    config: &ConfigInfo,
) -> Vec<Finding> {
    let mut f = Vec::new();
    macro_rules! add {
        ($level:expr, $section:expr, $message:expr $(,)?) => {
            f.push(finding($level, $section, $message))
        };
    }

    // Daemon presence.
    let mount_daemons = daemon
        .processes
        .iter()
        .filter(|p| p.cmdline.contains(" mount"))
        .count();
    match mount_daemons {
        0 => add!(
            "FAIL",
            "daemon",
            "no `wusel mount` process is running".into()
        ),
        1 => add!("PASS", "daemon", "the mount daemon is running".into()),
        n => add!(
            "WARN",
            "daemon",
            format!("{n} `wusel mount` processes are running — one is expected"),
        ),
    }

    // Stacked mounts.
    if mount.entries.len() > 1 {
        add!(
            "WARN",
            "mount",
            format!(
                "{} wusel mounts on the mountpoint — a leftover mount may be stacked",
                mount.entries.len()
            ),
        );
    }

    // The headline check: unanswered FUSE requests — and whether they are
    // unanswered because the mount is wedged or because it is working. Only the
    // two samples can tell those apart; see [`Recheck`].
    let waiting: u64 = mount.connections.iter().filter_map(|c| c.waiting).sum();
    let before = outstanding_keys(engine.report.as_ref());
    let parked_before = engine
        .report
        .as_ref()
        .and_then(|r| r.replies_pending)
        .unwrap_or(0);
    let secs = recheck.after_ms as f64 / 1000.0;
    if waiting == 0 {
        add!("PASS", "mount", "no FUSE requests are stuck waiting".into());
    } else if !recheck.taken || !recheck.engine_answered {
        // Either the first pass ruled a second sample out (it cannot, with
        // `waiting > 0`) or the daemon stopped answering between the two. Both
        // leave the question open, and saying so beats guessing either way.
        add!(
            "WARN",
            "mount",
            format!(
                "{waiting} FUSE request(s) waiting, and no second sample of the daemon's state to compare against — load and a stall cannot be told apart"
            ),
        );
    } else if recheck.waiting == Some(0) {
        add!(
            "PASS",
            "mount",
            format!("{waiting} FUSE request(s) were waiting and none {secs:.0} s later — the mount is working through them"),
        );
    } else {
        let stuck: Vec<&str> = recheck
            .outstanding
            .iter()
            .filter(|k| before.contains(k))
            .map(String::as_str)
            .collect();
        let parked_after = recheck.replies_pending.unwrap_or(0);
        if !stuck.is_empty() {
            add!(
                "FAIL",
                "mount",
                format!(
                    "the same job is still outstanding {secs:.0} s later ({}) while {waiting} FUSE request(s) wait — the mount is wedged. This is the class of failure doctor exists to catch.",
                    stuck.join(", ")
                ),
            );
        } else if before.is_empty()
            && recheck.outstanding.is_empty()
            && parked_before > 0
            && parked_after > 0
        {
            // The other shape of the same bug: the flow is long gone, yet a reply
            // is still parked and the kernel is still waiting for it. That is a
            // reply that was never sent, and the locked page behind it will not
            // come back on its own.
            add!(
                "FAIL",
                "mount",
                format!(
                    "{waiting} FUSE request(s) waiting and {parked_after} repl(y/ies) still parked {secs:.0} s later, with no job running behind them — a reply was never sent. This is the class of failure doctor exists to catch."
                ),
            );
        } else {
            add!(
                "PASS",
                "mount",
                format!("{waiting} FUSE request(s) waiting, but the work moved on within {secs:.0} s — ordinary load, not a stall"),
            );
        }
    }

    // Responsiveness. A TIMEOUT is the wedged signature and a real FAIL; a plain
    // read error usually just means the mount is not up, which the daemon check
    // already reports — so it is only informational here.
    match mount.responsive.as_deref() {
        Some(s) if s.starts_with("ok") => {
            add!("PASS", "mount", format!("mountpoint responds ({s})"))
        }
        Some(s) if s.contains("TIMEOUT") => {
            add!("FAIL", "mount", format!("mountpoint wedged: {s}"))
        }
        Some(s) => add!(
            "INFO",
            "mount",
            format!("mountpoint not readable — likely not mounted: {s}")
        ),
        None => {}
    }

    // Engine introspection availability.
    if engine.available {
        let stuck = engine
            .report
            .as_ref()
            .map(|r| r.machine.objects.iter().filter(|o| o.outstanding).count())
            .unwrap_or(0);
        if stuck > 0 {
            add!(
                "INFO",
                "engine",
                format!("{stuck} object(s) with a job in flight — see the engine section"),
            );
        }
        add!(
            "PASS",
            "engine",
            "the daemon served its internal state".into()
        );
    } else {
        add!(
            "WARN",
            "engine",
            "the daemon's diagnostics socket was unavailable — internal state could not be read"
                .into(),
        );
    }

    // Network home for the database.
    if config.db_on_network == Some(true) {
        add!(
            "WARN",
            "config",
            "the state database is on a network filesystem — SQLite cannot lock reliably there"
                .into(),
        );
    }

    f
}

/// Findings about live updates — the notify_push connection — from the state
/// the daemon serves.
///
/// Separate from [`derive_findings`] because it judges a different thing: not
/// whether the mount is healthy, but whether the server side of instant
/// updates is — which no probe from here can see, since a mount without push
/// works exactly like one with it, only staler. It leans on the connectivity
/// probe for the one distinction that matters: an endpoint that keeps failing
/// while the server answers is a reverse-proxy problem, not a network one, and
/// the advice is the opposite.
fn push_findings(engine: &Engine, connectivity: &Connectivity) -> Vec<Finding> {
    use wusel_core::diag::PushPhase;
    let mut f = Vec::new();
    let Some(push) = engine.report.as_ref().and_then(|r| r.push.as_ref()) else {
        return f;
    };
    let last = push.last_error.as_deref().unwrap_or("no error recorded");
    let endpoint = push.endpoint.as_deref().unwrap_or("<endpoint unknown>");
    let (level, message) = match push.phase {
        PushPhase::Connected => (
            "PASS",
            format!(
                "live updates are connected (notify_push, up for {} s)",
                push.since_secs
            ),
        ),
        PushPhase::Unavailable => {
            let why = match &push.last_error {
                Some(e) => format!("the capability lookup was refused: {e}"),
                None => "the server does not offer notify_push".to_string(),
            };
            (
                "INFO",
                format!("no live updates — {why}; changes are picked up by polling"),
            )
        }
        PushPhase::Discovering if push.failures > 0 => (
            "WARN",
            format!(
                "the capability lookup has failed {} time(s) and is being retried — last: {last}",
                push.failures
            ),
        ),
        PushPhase::Discovering => (
            "INFO",
            "still asking the server whether it offers notify_push".to_string(),
        ),
        PushPhase::Connecting | PushPhase::Reconnecting if push.failures > 0 => {
            // The whole reason this finding exists. A server that answers on
            // its port while its WebSocket endpoint does not is the reverse
            // proxy nine times out of ten — and from the user's side that looks
            // like a flaky network, which is what they would otherwise chase.
            let hint = if endpoint_is_loopback(endpoint) {
                // The server hands out an address that only the server itself
                // can reach — the app's `base_endpoint` was never set to the
                // public URL. No proxy will fix that; the setting will.
                " The server advertises a loopback address, which only the server itself can \
                 reach: notify_push's `base_endpoint` is not set to the public URL. On the \
                 server, run `occ notify_push:setup`, or set it by hand with \
                 `occ config:app:set notify_push base_endpoint --value https://<server>/push`."
            } else if connectivity.tcp_reachable == Some(true) {
                " The server itself answers, so this is almost always the reverse proxy in \
                 front of Nextcloud not passing WebSocket upgrades through to notify_push — \
                 run `occ notify_push:self-test` on the server. Until it is fixed the mount \
                 falls back to polling."
            } else {
                ""
            };
            (
                "WARN",
                format!(
                    "the notify_push endpoint {endpoint} cannot be held: {} failure(s) since \
                     the last connection, last: {last}.{hint}",
                    push.failures
                ),
            )
        }
        PushPhase::Connecting | PushPhase::Reconnecting => (
            "INFO",
            format!("reconnecting to {endpoint} after a clean close"),
        ),
        PushPhase::Stopped => (
            "INFO",
            match &push.last_error {
                Some(e) => format!("the notify_push listener has stopped: {e}"),
                None => "the notify_push listener has stopped".to_string(),
            },
        ),
        PushPhase::Unknown => (
            "INFO",
            "the daemon reports a notify_push phase this doctor does not know — the two are \
             different versions"
                .to_string(),
        ),
    };
    f.push(finding(level, "push", message));
    f
}

/// Whether a WebSocket URL points at the machine it was served from —
/// `ws://127.0.0.1:7867/ws`, `localhost`, `::1`. The classic notify_push
/// misconfiguration: the app works on the server, so its self-test passes, and
/// every client in the world is told to connect to itself.
fn endpoint_is_loopback(endpoint: &str) -> bool {
    let rest = endpoint.split("://").nth(1).unwrap_or(endpoint);
    let authority = rest.split('/').next().unwrap_or(rest);
    // `[::1]:7867` keeps its brackets; `host:port` loses the port.
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or(bracketed)
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    host == "localhost" || host == "::1" || host.starts_with("127.") || host == "0.0.0.0"
}

// --- Text rendering ---------------------------------------------------------

impl Report {
    fn to_text(&self) -> String {
        let mut o = String::new();
        let line = "=".repeat(72);
        o.push_str(&format!(
            "{line}\n{} {}\n{line}\n",
            self.tool.name, self.tool.version
        ));
        o.push_str(&format!("account: {}\n", self.tool.account));
        if self.tool.redacted {
            o.push_str(
                "redaction: ON — $HOME and username masked; engine state is name-free;\n  \
                 no mount listing collected; no secrets read. Logs are technical but may\n  \
                 name paths; re-run with --no-logs to omit them, or --include-listing to\n  \
                 turn redaction off for a deep dive.\n",
            );
        } else {
            o.push_str("redaction: OFF (--include-listing) — full paths and names included.\n");
        }

        o.push_str("\nSUMMARY\n");
        let (mut pass, mut warn, mut fail) = (0, 0, 0);
        for x in &self.findings {
            match x.level {
                "PASS" => pass += 1,
                "WARN" => warn += 1,
                "FAIL" => fail += 1,
                _ => {}
            }
        }
        o.push_str(&format!("  {fail} FAIL   {warn} WARN   {pass} PASS\n"));
        for x in &self.findings {
            o.push_str(&format!(
                "  [{:>4}] {}: {}\n",
                x.level, x.section, x.message
            ));
        }

        o.push_str(&section("SYSTEM"));
        push_kv(&mut o, "uname", self.system.uname.as_deref());
        push_kv(&mut o, "distro", self.system.distro.as_deref());
        push_kv(&mut o, "desktop", self.system.desktop.as_deref());
        push_kv(&mut o, "session", self.system.session_type.as_deref());

        o.push_str(&section("DAEMON"));
        push_kv(&mut o, "unit", self.daemon.unit.as_deref());
        push_kv(&mut o, "systemd", self.daemon.systemd.as_deref());
        for p in &self.daemon.processes {
            o.push_str(&format!(
                "  pid {} [{}]  state={} threads={} rss={}\n    {}\n",
                p.pid,
                short_cmd(&p.cmdline),
                p.state.as_deref().unwrap_or("?"),
                p.threads
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into()),
                p.rss_kb
                    .map(|k| format!("{k} kB"))
                    .unwrap_or_else(|| "?".into()),
                p.cmdline,
            ));
            if !p.thread_waits.is_empty() {
                o.push_str("    threads (comm / wchan / syscall):\n");
                for t in &p.thread_waits {
                    o.push_str(&format!(
                        "      {:<18} {:<28} {}\n",
                        t.comm, t.wchan, t.syscall
                    ));
                }
            }
        }

        o.push_str(&section("MOUNT"));
        for e in &self.mount.entries {
            o.push_str(&format!(
                "  {}  fstype={} source={} connection={}\n",
                e.mountpoint, e.fstype, e.source, e.connection
            ));
        }
        for c in &self.mount.connections {
            o.push_str(&format!(
                "  /sys/fs/fuse/connections/{}: waiting={} max_background={} congestion_threshold={}\n",
                c.id,
                opt(c.waiting),
                opt(c.max_background),
                opt(c.congestion_threshold),
            ));
        }
        push_kv(&mut o, "responsive", self.mount.responsive.as_deref());

        o.push_str(&section("ENGINE (from the daemon's diagnostics socket)"));
        if let Some(r) = &self.engine.report {
            o.push_str(&format!(
                "  schema={} refreshing={} pools(db_readers={}, net={}, file={}) buffers(open={}, dirty={})\n",
                r.schema, r.refreshing, r.pools.db_readers, r.pools.net, r.pools.file,
                r.machine.buffers_open, r.machine.buffers_dirty,
            ));
            if let Some(p) = r.replies_pending {
                o.push_str(&format!("  replies_pending={p}\n"));
            }
            if !r.hydrating.is_empty() {
                // Background whole-file downloads. They never become flows, so
                // they are absent from the object list below — without this line
                // the engine reads as idle while it is pulling megabytes.
                o.push_str(&format!(
                    "  hydrating {} file(s) in the background: file ids {}\n",
                    r.hydrating.len(),
                    r.hydrating
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                ));
            }
            if r.machine.objects.is_empty() {
                o.push_str("  no objects with work in flight\n");
            }
            for ob in &r.machine.objects {
                o.push_str(&format!(
                    "  inode {} intent={} step={} outstanding={} waiters={} queued={} abort={}\n",
                    ob.object, ob.intent, ob.step, ob.outstanding, ob.waiters, ob.queued, ob.abort,
                ));
            }
        } else if let Some(note) = &self.engine.note {
            o.push_str(&format!("  unavailable: {note}\n"));
        }
        if self.recheck.taken {
            o.push_str(&format!(
                "  second sample {} ms later: waiting={} replies_pending={}\n",
                self.recheck.after_ms,
                opt(self.recheck.waiting),
                opt(self.recheck.replies_pending),
            ));
            if self.recheck.outstanding.is_empty() {
                o.push_str("    still outstanding: (nothing)\n");
            }
            for k in &self.recheck.outstanding {
                o.push_str(&format!("    still outstanding: {k}\n"));
            }
        }

        o.push_str(&section("CONFIG"));
        push_kv(&mut o, "server", self.config.server.as_deref());
        push_kv(&mut o, "login", self.config.login.as_deref());
        push_kv(&mut o, "mountpoint", self.config.mountpoint.as_deref());
        push_kv(
            &mut o,
            "dispatch_threads",
            self.config
                .dispatch_threads
                .map(|n| n.to_string())
                .as_deref(),
        );
        push_kv(
            &mut o,
            "refresh_pinned",
            self.config.refresh_pinned.as_deref(),
        );
        push_kv(&mut o, "open_pinned", self.config.open_pinned.as_deref());
        push_kv(&mut o, "db_path", self.config.db_path.as_deref());
        push_kv(
            &mut o,
            "db_on_network",
            self.config.db_on_network.map(|b| b.to_string()).as_deref(),
        );
        push_kv(
            &mut o,
            "pins",
            self.config.pins.map(|n| n.to_string()).as_deref(),
        );

        o.push_str(&section("CONNECTIVITY"));
        push_kv(
            &mut o,
            "server_host",
            self.connectivity.server_host.as_deref(),
        );
        push_kv(&mut o, "detail", self.connectivity.detail.as_deref());

        o.push_str(&section("PUSH (notify_push live updates, from the daemon)"));
        match self.engine.report.as_ref().and_then(|r| r.push.as_ref()) {
            Some(p) => {
                o.push_str(&format!(
                    "  phase={} since={} s connects={} failures={}\n",
                    p.phase.as_str(),
                    p.since_secs,
                    p.connects,
                    p.failures,
                ));
                push_kv(&mut o, "endpoint", p.endpoint.as_deref());
                if let Some(e) = &p.last_error {
                    o.push_str(&format!("  last_error: {e}\n"));
                }
            }
            None => {
                o.push_str("  (unavailable: no daemon state, or a daemon without this report)\n")
            }
        }

        if let Some(logs) = &self.logs {
            o.push_str(&section(&format!(
                "LOGS (journalctl --user -u {}, last 200)",
                self.daemon.unit.as_deref().unwrap_or("<no unit>")
            )));
            for l in logs {
                o.push_str("  ");
                o.push_str(l);
                o.push('\n');
            }
        }

        o
    }
}

fn section(title: &str) -> String {
    format!("\n{}\n{title}\n{}\n", "-".repeat(72), "-".repeat(72))
}

fn push_kv(o: &mut String, key: &str, val: Option<&str>) {
    o.push_str(&format!("  {key}: {}\n", val.unwrap_or("(unavailable)")));
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "?".into())
}

fn short_cmd(cmdline: &str) -> String {
    cmdline
        .split_whitespace()
        .skip(1)
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_masks_home_and_user_and_is_reversible_by_flag() {
        // Build a redactor with known values rather than the environment's, so
        // the test does not depend on who runs it.
        let red = Redactor {
            home: Some("/home/alice".into()),
            user: Some("alice".into()),
            enabled: true,
        };
        let masked = red.s("error at /home/alice/Wusel/Secret.kdbx for user alice");
        assert!(
            !masked.contains("/home/alice"),
            "home path is masked: {masked}"
        );
        assert!(masked.contains("$HOME/Wusel"));
        assert!(masked.contains("<user>"));

        let off = Redactor {
            enabled: false,
            ..red_clone(&red)
        };
        assert_eq!(
            off.s("/home/alice/x"),
            "/home/alice/x",
            "with redaction off the text is untouched"
        );
    }

    fn red_clone(r: &Redactor) -> Redactor {
        Redactor {
            home: r.home.clone(),
            user: r.user.clone(),
            enabled: r.enabled,
        }
    }

    /// A mount daemon, so the daemon check does not drown out the mount finding.
    fn a_daemon() -> Daemon {
        Daemon {
            processes: vec![Process {
                cmdline: "/usr/bin/wusel mount --account default".into(),
                ..Default::default()
            }],
            unit: Some("wusel@default.service".into()),
            systemd: None,
        }
    }

    fn a_mount(waiting: u64) -> Mount {
        Mount {
            connections: vec![FuseConnection {
                id: 67,
                waiting: Some(waiting),
                max_background: None,
                congestion_threshold: None,
            }],
            responsive: Some("ok (1 ms)".into()),
            ..Default::default()
        }
    }

    fn mount_finding(findings: &[Finding]) -> &Finding {
        findings
            .iter()
            .find(|f| f.section == "mount" && !f.message.contains("mountpoint"))
            .expect("a finding about waiting requests")
    }

    #[test]
    fn a_job_still_outstanding_in_both_samples_is_a_wedged_mount() {
        // The real signature: the *same* inode, intent and step still handed out
        // to a worker two seconds later, with the kernel still waiting.
        let engine = Engine {
            available: true,
            note: None,
            report: Some(sample_report(1, &[fetching(26298)])),
        };
        let recheck = Recheck {
            taken: true,
            after_ms: 2000,
            engine_answered: true,
            waiting: Some(1),
            replies_pending: Some(1),
            outstanding: outstanding_keys(engine.report.as_ref()),
        };
        let findings = derive_findings(
            &a_daemon(),
            &a_mount(1),
            &engine,
            &recheck,
            &ConfigInfo::default(),
        );
        let f = mount_finding(&findings);
        assert_eq!(f.level, "FAIL", "expected a wedge, got {f:?}");
        assert!(f.message.contains("inode 26298 fetch/FetchBytes"), "{f:?}");
    }

    #[test]
    fn work_that_moves_between_the_samples_is_load_not_a_wedge() {
        // The false positive this check was rewritten for: a busy mount always
        // has `waiting` and `replies_pending` in step, because a reply is parked
        // the moment a request arrives. Only the *identity* of the work separates
        // the two, and here it changed — a different inode each sample.
        let engine = Engine {
            available: true,
            note: None,
            report: Some(sample_report(1, &[fetching(12901)])),
        };
        let recheck = Recheck {
            taken: true,
            after_ms: 2000,
            engine_answered: true,
            waiting: Some(1),
            replies_pending: Some(1),
            outstanding: vec!["inode 29845 lookup/LookRemote".into()],
        };
        let findings = derive_findings(
            &a_daemon(),
            &a_mount(1),
            &engine,
            &recheck,
            &ConfigInfo::default(),
        );
        let f = mount_finding(&findings);
        assert_eq!(f.level, "PASS", "expected ordinary load, got {f:?}");
    }

    #[test]
    fn a_parked_reply_with_nothing_running_behind_it_is_a_wedged_mount() {
        // The other shape: the flow ended without its reply being sent. The
        // machine is idle in both samples, yet the kernel still waits and the
        // frontend still holds the handle.
        let engine = Engine {
            available: true,
            note: None,
            report: Some(sample_report(2, &[])),
        };
        let recheck = Recheck {
            taken: true,
            after_ms: 2000,
            engine_answered: true,
            waiting: Some(2),
            replies_pending: Some(2),
            outstanding: Vec::new(),
        };
        let findings = derive_findings(
            &a_daemon(),
            &a_mount(2),
            &engine,
            &recheck,
            &ConfigInfo::default(),
        );
        let f = mount_finding(&findings);
        assert_eq!(f.level, "FAIL", "expected a lost reply, got {f:?}");
        assert!(f.message.contains("never sent"), "{f:?}");
    }

    #[test]
    fn requests_that_drain_between_the_samples_pass() {
        let engine = Engine {
            available: true,
            note: None,
            report: Some(sample_report(3, &[fetching(7)])),
        };
        let recheck = Recheck {
            taken: true,
            after_ms: 2000,
            engine_answered: true,
            waiting: Some(0),
            replies_pending: Some(0),
            outstanding: Vec::new(),
        };
        let findings = derive_findings(
            &a_daemon(),
            &a_mount(3),
            &engine,
            &recheck,
            &ConfigInfo::default(),
        );
        assert_eq!(mount_finding(&findings).level, "PASS");
    }

    #[test]
    fn a_daemon_that_stops_answering_leaves_the_question_open() {
        // Not a wedge and not a clean bill: with no second sample of the engine
        // there is nothing to compare, and saying so beats guessing.
        let engine = Engine {
            available: true,
            note: None,
            report: Some(sample_report(1, &[fetching(5)])),
        };
        let recheck = Recheck {
            taken: true,
            after_ms: 2000,
            engine_answered: false,
            waiting: Some(1),
            replies_pending: None,
            outstanding: Vec::new(),
        };
        let findings = derive_findings(
            &a_daemon(),
            &a_mount(1),
            &engine,
            &recheck,
            &ConfigInfo::default(),
        );
        assert_eq!(mount_finding(&findings).level, "WARN");
    }

    #[test]
    fn the_probes_ask_about_the_account_instance_not_the_template() {
        // The service is a template. Asking systemd about the bare name reports
        // a running mount as `inactive, dead`, and asking journalctl about it
        // matches nothing at all — which is how every bundle came to carry
        // "-- No entries --" without saying that it had asked the wrong thing.
        assert_eq!(
            unit_for("default").as_deref(),
            Some("wusel@default.service")
        );
        assert_eq!(unit_for("work").as_deref(), Some("wusel@work.service"));
        // An account systemd cannot name has no unit, rather than a wrong one.
        assert_eq!(unit_for("Müller"), None);
    }

    #[test]
    fn an_empty_journal_names_the_unit_it_asked_about() {
        // Silence must be attributable. The old text ("no user journal for the
        // unit") read like a fact about the daemon; it was a fact about the
        // question.
        let lines = probe_logs(
            Some("wusel@nonexistent-test.service"),
            &Redactor::new(false),
        );
        assert!(
            lines[0].contains("wusel@nonexistent-test.service"),
            "the unit is named: {lines:?}"
        );
    }

    #[test]
    fn findings_flag_a_missing_daemon() {
        let findings = derive_findings(
            &Daemon::default(),
            &Mount::default(),
            &Engine {
                available: false,
                note: None,
                report: None,
            },
            &Recheck::default(),
            &ConfigInfo::default(),
        );
        assert!(findings
            .iter()
            .any(|f| f.level == "FAIL" && f.section == "daemon"));
    }

    fn engine_with_push(push: wusel_core::diag::PushReport) -> Engine {
        let mut report = sample_report(0, &[]);
        report.push = Some(push);
        Engine {
            available: true,
            note: None,
            report: Some(report),
        }
    }

    fn a_push(phase: wusel_core::diag::PushPhase) -> wusel_core::diag::PushReport {
        wusel_core::diag::PushReport {
            phase,
            since_secs: 12,
            endpoint: Some("wss://cloud.example.org/push/ws".into()),
            failures: 0,
            connects: 0,
            last_error: None,
        }
    }

    /// The case doctor grew this check for: HTTP works, the mount works, only
    /// the WebSocket keeps failing. That is the proxy, and the finding says so —
    /// but only when the server demonstrably answers, since otherwise the hint
    /// would send somebody with a real outage to their proxy config.
    #[test]
    fn a_failing_endpoint_behind_a_reachable_server_points_at_the_proxy() {
        let engine = engine_with_push(wusel_core::diag::PushReport {
            failures: 6,
            last_error: Some("websocket upgrade refused with 502 Bad Gateway".into()),
            ..a_push(wusel_core::diag::PushPhase::Reconnecting)
        });
        let reachable = Connectivity {
            tcp_reachable: Some(true),
            ..Connectivity::default()
        };
        let f = push_findings(&engine, &reachable);
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].level, f[0].section), ("WARN", "push"));
        assert!(f[0].message.contains("502"), "quotes the daemon's error");
        assert!(f[0].message.contains("reverse proxy"));
        assert!(f[0].message.contains("self-test"));

        let f = push_findings(&engine, &Connectivity::default());
        assert_eq!(f[0].level, "WARN");
        assert!(
            !f[0].message.contains("reverse proxy"),
            "no proxy hint without a server that answers"
        );
    }

    /// The other classic, seen in the wild: the server advertises
    /// `ws://127.0.0.1:8080/push/ws`. Connection refused on every client, while
    /// the server's own self-test passes — so the finding must name the setting,
    /// not the proxy.
    #[test]
    fn a_loopback_endpoint_names_the_base_endpoint_setting() {
        let engine = engine_with_push(wusel_core::diag::PushReport {
            endpoint: Some("ws://127.0.0.1:8080/push/ws".into()),
            failures: 3,
            last_error: Some("[connect] … Connection refused (os error 111)".into()),
            ..a_push(wusel_core::diag::PushPhase::Reconnecting)
        });
        let reachable = Connectivity {
            tcp_reachable: Some(true),
            ..Connectivity::default()
        };
        let f = push_findings(&engine, &reachable);
        assert_eq!(f[0].level, "WARN");
        assert!(f[0].message.contains("base_endpoint"), "{}", f[0].message);
        assert!(!f[0].message.contains("reverse proxy"));
    }

    #[test]
    fn loopback_detection_reads_the_host_only() {
        assert!(endpoint_is_loopback("ws://127.0.0.1:8080/push/ws"));
        assert!(endpoint_is_loopback("wss://localhost/push/ws"));
        assert!(endpoint_is_loopback("ws://[::1]:7867/ws"));
        assert!(!endpoint_is_loopback("wss://cloud.example.org/push/ws"));
        assert!(!endpoint_is_loopback(
            "wss://cloud.example.org:8443/push/ws"
        ));
        // A path or query mentioning localhost is not a loopback host.
        assert!(!endpoint_is_loopback(
            "wss://cloud.example.org/push/ws?via=localhost"
        ));
    }

    #[test]
    fn no_push_on_offer_is_information_not_a_warning() {
        let engine = engine_with_push(a_push(wusel_core::diag::PushPhase::Unavailable));
        let f = push_findings(&engine, &Connectivity::default());
        assert_eq!(f[0].level, "INFO");
        assert!(f[0].message.contains("polling"));
    }

    #[test]
    fn a_connected_listener_passes() {
        let engine = engine_with_push(a_push(wusel_core::diag::PushPhase::Connected));
        let f = push_findings(&engine, &Connectivity::default());
        assert_eq!(f[0].level, "PASS");
    }

    /// A daemon older than this field, or none at all: no finding rather than a
    /// guess either way.
    #[test]
    fn a_report_without_push_state_says_nothing_about_it() {
        let engine = Engine {
            available: true,
            note: None,
            report: Some(sample_report(0, &[])),
        };
        assert!(push_findings(&engine, &Connectivity::default()).is_empty());
    }

    fn fetching(object: u64) -> wusel_core::diag::ObjectReport {
        wusel_core::diag::ObjectReport {
            object,
            intent: "fetch".into(),
            step: "FetchBytes".into(),
            outstanding: true,
            waiters: 1,
            queued: 0,
            abort: false,
        }
    }

    fn sample_report(
        replies_pending: usize,
        objects: &[wusel_core::diag::ObjectReport],
    ) -> DiagReport {
        DiagReport {
            schema: wusel_core::diag::SCHEMA,
            machine: wusel_core::diag::MachineReport {
                objects: objects.to_vec(),
                buffers_open: 0,
                buffers_dirty: 0,
            },
            refreshing: 0,
            hydrating: Vec::new(),
            pools: wusel_core::diag::PoolsReport {
                db_readers: 2,
                net: 4,
                file: 2,
            },
            replies_pending: Some(replies_pending),
            push: None,
        }
    }
}
