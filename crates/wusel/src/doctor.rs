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

        let system = probe_system();
        let daemon = probe_daemon();
        let mount = probe_mount(&mountpoint, &red);
        let engine = probe_engine(&mountpoint);
        let config = probe_config(&account, &settings, &mountpoint, &red);
        let connectivity = probe_connectivity(&config);
        let logs = if opts.no_logs {
            None
        } else {
            Some(probe_logs(&red))
        };

        let findings = derive_findings(&daemon, &mount, &engine, &config);

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

fn probe_daemon() -> Daemon {
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
    // Best-effort systemd view; the unit is "wusel" for the default account.
    let systemd = run_cmd(
        "systemctl",
        &["--user", "show", "-p", "ActiveState,SubState,NRestarts", "wusel"],
    )
    .map(|s| s.replace('\n', ", "));
    Daemon {
        processes,
        systemd,
    }
}

/// Every `wusel` process id. `pidof` first; a `/proc` scan as the fallback.
fn wusel_pids() -> Vec<i32> {
    if let Some(out) = run_cmd("pidof", &["wusel"]) {
        let pids: Vec<i32> = out.split_whitespace().filter_map(|s| s.parse().ok()).collect();
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
            let connection = devno.split_once(':').and_then(|(_, m)| m.parse().ok()).unwrap_or(0);
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
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(if is_http { 80 } else { 443 })),
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
        format!("TCP connect to {host}:{port} ok ({} ms)", started.elapsed().as_millis())
    } else {
        format!("TCP connect to {host}:{port} failed")
    });
    conn
}

fn probe_logs(red: &Redactor) -> Vec<String> {
    run_cmd(
        "journalctl",
        &["--user", "-u", "wusel", "-n", "200", "--no-pager", "-o", "short-iso"],
    )
    .map(|s| s.lines().map(|l| red.s(l)).collect())
    .unwrap_or_else(|| vec!["(journalctl unavailable or no user journal for the unit)".into()])
}

// --- Findings: the PASS/WARN/FAIL layer -------------------------------------

fn derive_findings(daemon: &Daemon, mount: &Mount, engine: &Engine, config: &ConfigInfo) -> Vec<Finding> {
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
        0 => add!("FAIL", "daemon", "no `wusel mount` process is running".into()),
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

    // The headline check: unanswered FUSE requests.
    let waiting: u64 = mount.connections.iter().filter_map(|c| c.waiting).sum();
    if waiting > 0 {
        let parked = engine.report.as_ref().and_then(|r| r.replies_pending);
        match parked {
            Some(p) if p >= waiting as usize && waiting > 0 => add!(
                "FAIL",
                "mount",
                format!(
                    "{waiting} FUSE request(s) waiting and {p} repl(y/ies) parked — the mount looks wedged (a reply never sent). This is the class of failure doctor exists to catch."
                ),
            ),
            _ => add!(
                "WARN",
                "mount",
                format!("{waiting} FUSE request(s) waiting for the daemon — may be ordinary load, or a stall"),
            ),
        }
    } else {
        add!("PASS", "mount", "no FUSE requests are stuck waiting".into());
    }

    // Responsiveness. A TIMEOUT is the wedged signature and a real FAIL; a plain
    // read error usually just means the mount is not up, which the daemon check
    // already reports — so it is only informational here.
    match mount.responsive.as_deref() {
        Some(s) if s.starts_with("ok") => add!("PASS", "mount", format!("mountpoint responds ({s})")),
        Some(s) if s.contains("TIMEOUT") => add!("FAIL", "mount", format!("mountpoint wedged: {s}")),
        Some(s) => add!("INFO", "mount", format!("mountpoint not readable — likely not mounted: {s}")),
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
        add!("PASS", "engine", "the daemon served its internal state".into());
    } else {
        add!(
            "WARN",
            "engine",
            "the daemon's diagnostics socket was unavailable — internal state could not be read".into(),
        );
    }

    // Network home for the database.
    if config.db_on_network == Some(true) {
        add!(
            "WARN",
            "config",
            "the state database is on a network filesystem — SQLite cannot lock reliably there".into(),
        );
    }

    f
}

// --- Text rendering ---------------------------------------------------------

impl Report {
    fn to_text(&self) -> String {
        let mut o = String::new();
        let line = "=".repeat(72);
        o.push_str(&format!("{line}\n{} {}\n{line}\n", self.tool.name, self.tool.version));
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
            o.push_str(&format!("  [{:>4}] {}: {}\n", x.level, x.section, x.message));
        }

        o.push_str(&section("SYSTEM"));
        push_kv(&mut o, "uname", self.system.uname.as_deref());
        push_kv(&mut o, "distro", self.system.distro.as_deref());
        push_kv(&mut o, "desktop", self.system.desktop.as_deref());
        push_kv(&mut o, "session", self.system.session_type.as_deref());

        o.push_str(&section("DAEMON"));
        push_kv(&mut o, "systemd", self.daemon.systemd.as_deref());
        for p in &self.daemon.processes {
            o.push_str(&format!(
                "  pid {} [{}]  state={} threads={} rss={}\n    {}\n",
                p.pid,
                short_cmd(&p.cmdline),
                p.state.as_deref().unwrap_or("?"),
                p.threads.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
                p.rss_kb.map(|k| format!("{k} kB")).unwrap_or_else(|| "?".into()),
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

        o.push_str(&section("CONFIG"));
        push_kv(&mut o, "server", self.config.server.as_deref());
        push_kv(&mut o, "login", self.config.login.as_deref());
        push_kv(&mut o, "mountpoint", self.config.mountpoint.as_deref());
        push_kv(
            &mut o,
            "dispatch_threads",
            self.config.dispatch_threads.map(|n| n.to_string()).as_deref(),
        );
        push_kv(&mut o, "refresh_pinned", self.config.refresh_pinned.as_deref());
        push_kv(&mut o, "open_pinned", self.config.open_pinned.as_deref());
        push_kv(&mut o, "db_path", self.config.db_path.as_deref());
        push_kv(
            &mut o,
            "db_on_network",
            self.config.db_on_network.map(|b| b.to_string()).as_deref(),
        );
        push_kv(&mut o, "pins", self.config.pins.map(|n| n.to_string()).as_deref());

        o.push_str(&section("CONNECTIVITY"));
        push_kv(&mut o, "server_host", self.connectivity.server_host.as_deref());
        push_kv(&mut o, "detail", self.connectivity.detail.as_deref());

        if let Some(logs) = &self.logs {
            o.push_str(&section("LOGS (journalctl --user -u wusel, last 200)"));
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
        assert!(!masked.contains("/home/alice"), "home path is masked: {masked}");
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

    #[test]
    fn findings_flag_a_wedged_mount_from_waiting_and_parked_replies() {
        // waiting > 0 and at least as many parked replies => the wedged-mount
        // FAIL, the exact signature of the bug doctor was built for.
        let daemon = Daemon {
            processes: vec![Process {
                cmdline: "/usr/bin/wusel mount --account default".into(),
                ..Default::default()
            }],
            systemd: None,
        };
        let mount = Mount {
            connections: vec![FuseConnection {
                id: 67,
                waiting: Some(3),
                max_background: None,
                congestion_threshold: None,
            }],
            responsive: Some("ok (1 ms)".into()),
            ..Default::default()
        };
        let engine = Engine {
            available: true,
            note: None,
            report: Some(sample_report(3)),
        };
        let findings = derive_findings(&daemon, &mount, &engine, &ConfigInfo::default());
        assert!(
            findings
                .iter()
                .any(|f| f.level == "FAIL" && f.section == "mount" && f.message.contains("wedged")),
            "expected a wedged-mount FAIL, got {findings:?}"
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
            &ConfigInfo::default(),
        );
        assert!(findings.iter().any(|f| f.level == "FAIL" && f.section == "daemon"));
    }

    fn sample_report(replies_pending: usize) -> DiagReport {
        DiagReport {
            schema: wusel_core::diag::SCHEMA,
            machine: wusel_core::diag::MachineReport {
                objects: vec![],
                buffers_open: 0,
                buffers_dirty: 0,
            },
            refreshing: 0,
            pools: wusel_core::diag::PoolsReport {
                db_readers: 2,
                net: 4,
                file: 2,
            },
            replies_pending: Some(replies_pending),
        }
    }
}
