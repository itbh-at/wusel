// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! `wusel status` — what the mount is doing right now, by file name.
//!
//! The counterpart to [`crate::doctor`], and deliberately a separate command
//! rather than a flag on it, because the two answer different questions for
//! different readers:
//!
//! * `doctor` collects a **support bundle**. It goes to somebody else, so its
//!   engine view is name-free by construction (see [`wusel_core::diag`]) and
//!   speaks in inode numbers.
//! * `status` answers **"what is happening to my files?"** for the person whose
//!   files they are. Names are the entire point, and the output is for a
//!   terminal, not a ticket — there is no redaction mode and no way to write it
//!   to a file.
//!
//! That split is also why nothing here changes the wire format's privacy rule.
//! The socket still serves inodes and file ids; this command joins them against
//! the state database in the **user's own process**, on the user's own machine.
//! The names never cross the socket.
//!
//! Two sources, because the work has two very different lifetimes:
//!
//! * **Owed work** — `pending_uploads` in the state database. Durable, survives
//!   a restart, and readable even with no daemon running. This is where a
//!   *parked* upload shows up: a permanent failure is recorded and deliberately
//!   not retried, so without this the file looks saved and is not on the server.
//! * **Work in flight** — the daemon's live snapshot: the state machine's
//!   occupancy plus the background hydrations, which never become flows and are
//!   therefore invisible in the occupancy (see
//!   [`wusel_core::content::ContentSource::hydrating`]).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use wusel_core::config::Account;
use wusel_core::diag::DiagReport;
use wusel_core::state::{NodeRow, StateDb, UploadState};

/// How `status` was asked to run.
pub struct Options {
    pub account: String,
    /// Redraw until interrupted. Individual reads are far too short-lived to be
    /// caught by a one-shot print — see [`WATCH_INTERVAL`].
    pub watch: bool,
}

/// How often `--watch` redraws. A read of one kernel-sized range is much
/// shorter than this, so watching is a sampling of the traffic and not a
/// complete log of it — the summary line says so rather than implying otherwise.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// Print the status once, or keep redrawing it with `--watch`.
///
/// # Errors
/// If the account has no state database at all — there is nothing to report and
/// saying so plainly beats printing an empty report that looks like "idle".
pub fn run(opts: &Options) -> Result<()> {
    let account = Account::new(&opts.account);
    let settings = account.settings();
    let mountpoint = settings
        .mount_point
        .clone()
        .unwrap_or_else(|| account.default_mountpoint());
    let db_path = account.db_location().path().to_path_buf();
    if !db_path.exists() {
        anyhow::bail!(
            "no state database at {} — this account has never been mounted",
            db_path.display()
        );
    }

    if !opts.watch {
        print!("{}", Status::collect(&opts.account, &mountpoint, &db_path));
        return Ok(());
    }
    loop {
        let text = Status::collect(&opts.account, &mountpoint, &db_path);
        // Home the cursor and clear what is below it, rather than clearing the
        // screen first: clearing leaves a visible blank frame between redraws.
        print!("\x1b[H\x1b[J{text}");
        let _ = std::io::stdout().flush();
        std::thread::sleep(WATCH_INTERVAL);
    }
}

/// One rendered sample.
struct Status {
    account: String,
    /// Why there is no live view, if there is none. The owed-uploads half is
    /// still reported — that is the half that matters when the daemon is down.
    daemon: Option<String>,
    uploads: Vec<Upload>,
    downloads: Vec<Download>,
    other: Vec<Flow>,
    buffers: Option<(usize, usize)>,
}

struct Upload {
    state: UploadState,
    path: String,
    attempts: u32,
    last_error: Option<String>,
}

struct Download {
    path: String,
    size: u64,
    /// A whole-file background download, as opposed to a read being served live
    /// to whoever asked for it. Both are "downloading" to a user; only one of
    /// them has somebody waiting.
    background: bool,
}

struct Flow {
    intent: String,
    step: String,
    path: String,
    waiters: usize,
    queued: usize,
}

impl Status {
    fn collect(account: &str, mountpoint: &Path, db_path: &Path) -> String {
        let db = StateDb::open_existing(db_path).ok();
        let report = read_report(mountpoint);

        let mut status = Status {
            account: account.to_string(),
            daemon: match &report {
                Ok(_) => None,
                Err(e) => Some(e.clone()),
            },
            uploads: Vec::new(),
            downloads: Vec::new(),
            other: Vec::new(),
            buffers: None,
        };

        // Owed uploads first, and from the database rather than the daemon:
        // they are durable, and they are the half that still means something
        // when nothing is mounted.
        if let Some(db) = &db {
            if let Ok(pending) = db.pending_uploads() {
                status.uploads = pending
                    .into_iter()
                    .map(|p| Upload {
                        state: p.state,
                        // The recorded target, not a re-derived one: a rename
                        // moves it, and this is where the bytes are going.
                        path: p.remote_path,
                        attempts: p.attempts,
                        last_error: p.last_error,
                    })
                    .collect();
            }
        }

        let Ok(report) = report else {
            return status.to_text();
        };
        status.buffers = Some((report.machine.buffers_open, report.machine.buffers_dirty));

        // Resolve every id the report mentions in one pass, so a busy mount
        // costs a handful of indexed lookups rather than one per line.
        let names = Names::resolve(db.as_ref(), &report);

        for id in &report.hydrating {
            let node = names.by_file_id.get(id);
            status.downloads.push(Download {
                path: names.name_of(node, &format!("file id {id}")),
                size: node.map_or(0, |n| n.size),
                background: true,
            });
        }
        for o in &report.machine.objects {
            let node = names.by_inode.get(&o.object);
            let path = names.name_of(node, &format!("inode {}", o.object));
            // A fetch that is actually moving bytes reads as a download; one
            // still resolving its row does not, and calling it one would be a
            // small lie in the place the user is looking hardest.
            if o.intent == "fetch" && o.step == "FetchBytes" {
                status.downloads.push(Download {
                    path,
                    size: node.map_or(0, |n| n.size),
                    background: false,
                });
            } else {
                status.other.push(Flow {
                    intent: o.intent.clone(),
                    step: o.step.clone(),
                    path,
                    waiters: o.waiters,
                    queued: o.queued,
                });
            }
        }
        status.to_text()
    }

    fn to_text(&self) -> String {
        let mut o = String::new();
        o.push_str(&format!("wusel status — account {}\n", self.account));
        if let Some(why) = &self.daemon {
            o.push_str(&format!("  no live view: {why}\n"));
        }

        if self.uploads.is_empty() {
            o.push_str("\nUPLOADS\n  nothing owed to the server\n");
        } else {
            o.push_str(&format!("\nUPLOADS ({})\n", self.uploads.len()));
            for u in &self.uploads {
                let label = match u.state {
                    UploadState::Uploading => "sending",
                    UploadState::Pending => "waiting",
                    UploadState::Error => "PARKED",
                };
                o.push_str(&format!("  {label:<10} {}\n", u.path));
                if let Some(e) = &u.last_error {
                    // A parked upload is the one thing here the user has to act
                    // on: the file reads as saved and is not on the server.
                    o.push_str(&format!(
                        "  {:<10}   {e} (after {} attempt(s))\n",
                        "", u.attempts
                    ));
                }
            }
        }

        if self.daemon.is_none() {
            if self.downloads.is_empty() {
                o.push_str("\nDOWNLOADS\n  nothing coming down\n");
            } else {
                o.push_str(&format!("\nDOWNLOADS ({})\n", self.downloads.len()));
                for d in &self.downloads {
                    let label = if d.background { "caching" } else { "reading" };
                    o.push_str(&format!(
                        "  {label:<10} {}{}\n",
                        d.path,
                        if d.size > 0 {
                            format!(" ({})", human_size(d.size))
                        } else {
                            String::new()
                        }
                    ));
                }
            }

            if !self.other.is_empty() {
                o.push_str(&format!("\nOTHER WORK ({})\n", self.other.len()));
                for f in &self.other {
                    let mut waiting = String::new();
                    if f.waiters > 1 || f.queued > 0 {
                        waiting = format!(" — {} waiting, {} queued", f.waiters, f.queued);
                    }
                    o.push_str(&format!(
                        "  {:<10} {} [{}]{waiting}\n",
                        f.intent, f.path, f.step
                    ));
                }
            }

            if let Some((open, dirty)) = self.buffers {
                o.push_str(&format!(
                    "\nbuffers: {open} open, {dirty} with unsaved changes\n"
                ));
            }
        }
        o
    }
}

/// The inode → path and file id → path joins, done once per sample.
///
/// This is the whole trick of the command: the daemon hands out numbers because
/// its report has to be shareable, and the numbers become names here, in a
/// process that is already allowed to read the user's file names.
#[derive(Default)]
struct Names {
    by_inode: BTreeMap<u64, NodeRow>,
    by_file_id: BTreeMap<u64, NodeRow>,
}

impl Names {
    fn resolve(db: Option<&StateDb>, report: &DiagReport) -> Self {
        let mut names = Names::default();
        let Some(db) = db else { return names };
        for o in &report.machine.objects {
            if let Ok(Some(row)) = db.node_by_inode(o.object) {
                names.by_inode.insert(o.object, row);
            }
        }
        for id in &report.hydrating {
            if let Ok(Some(row)) = db.node_by_file_id(*id) {
                names.by_file_id.insert(*id, row);
            }
        }
        names
    }

    /// The node's path, or the bare number when the row is not there — a file
    /// removed under us, or a database that cannot be read. Saying "inode 26298"
    /// is honest; inventing a name would not be.
    fn name_of(&self, node: Option<&NodeRow>, fallback: &str) -> String {
        match node {
            Some(n) if n.path.is_empty() => "/".to_string(),
            Some(n) => n.path.clone(),
            None => fallback.to_string(),
        }
    }
}

/// Ask the running mount for its snapshot. The error is a sentence for the user,
/// not a type to match on — every reason ends the same way: no live view.
fn read_report(mountpoint: &Path) -> std::result::Result<DiagReport, String> {
    use std::io::Read;
    let path = wusel_core::config::diag_socket_for_mount(mountpoint);
    let mut stream = std::os::unix::net::UnixStream::connect(&path)
        .map_err(|e| format!("no mount is running for this account ({e})"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut buf = String::new();
    stream
        .read_to_string(&mut buf)
        .map_err(|e| format!("the daemon did not answer ({e})"))?;
    DiagReport::from_json(&buf).map_err(|e| format!("the daemon's answer did not parse ({e})"))
}

/// Bytes as something a person reads at a glance. Binary units, because that is
/// what a file manager shows next to the same file.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_status() -> Status {
        Status {
            account: "default".into(),
            daemon: None,
            uploads: Vec::new(),
            downloads: Vec::new(),
            other: Vec::new(),
            buffers: Some((0, 0)),
        }
    }

    #[test]
    fn a_parked_upload_is_named_with_its_reason() {
        // The case the command exists for: the file reads as saved and is not on
        // the server, and nothing retries it.
        let mut s = a_status();
        s.uploads.push(Upload {
            state: UploadState::Error,
            path: "Documents/report.odt".into(),
            attempts: 3,
            last_error: Some("Permanent".into()),
        });
        let text = s.to_text();
        assert!(text.contains("PARKED"), "{text}");
        assert!(text.contains("Documents/report.odt"), "{text}");
        assert!(text.contains("after 3 attempt(s)"), "{text}");
    }

    #[test]
    fn owed_uploads_are_reported_without_a_running_daemon() {
        // The durable half does not need the socket — which is the point of
        // reading it from the database rather than the daemon.
        let mut s = a_status();
        s.daemon = Some("no mount is running for this account".into());
        s.buffers = None;
        s.uploads.push(Upload {
            state: UploadState::Pending,
            path: "Notes/todo.md".into(),
            attempts: 0,
            last_error: None,
        });
        let text = s.to_text();
        assert!(text.contains("no live view"), "{text}");
        assert!(text.contains("waiting    Notes/todo.md"), "{text}");
        // Nothing is claimed about live work when there is no live view.
        assert!(!text.contains("DOWNLOADS"), "{text}");
    }

    #[test]
    fn a_background_hydration_and_a_live_read_are_told_apart() {
        let mut s = a_status();
        s.downloads.push(Download {
            path: "Videos/talk.mp4".into(),
            size: 5 * 1024 * 1024,
            background: true,
        });
        s.downloads.push(Download {
            path: "Docs/handbook.pdf".into(),
            size: 2048,
            background: false,
        });
        let text = s.to_text();
        assert!(
            text.contains("caching    Videos/talk.mp4 (5.0 MiB)"),
            "{text}"
        );
        assert!(
            text.contains("reading    Docs/handbook.pdf (2.0 KiB)"),
            "{text}"
        );
    }

    #[test]
    fn an_idle_mount_says_so_rather_than_printing_nothing() {
        let text = a_status().to_text();
        assert!(text.contains("nothing owed to the server"), "{text}");
        assert!(text.contains("nothing coming down"), "{text}");
    }

    #[test]
    fn an_unresolvable_id_prints_the_number_rather_than_a_guess() {
        let names = Names::default();
        assert_eq!(names.name_of(None, "inode 26298"), "inode 26298");
    }

    #[test]
    fn sizes_read_the_way_a_file_manager_shows_them() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1024 * 1024 * 3 / 2), "1.5 MiB");
    }
}
