// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The change-signal fan-out.
//!
//! The engine emits [`Invalidation`]s on a single channel — a server-side change
//! the syncer or notify_push noticed. A frontend under FUSE consumes that channel
//! directly; here, several `watch` clients may want the same stream at once, so
//! one background thread reads it and copies each change to every current
//! subscriber. A subscriber whose client has gone (its receiver dropped) is
//! pruned on the next send.
//!
//! The engine's `ObjectId`s do not cross the wire — they are a per-session inode
//! — so a change is reduced to its **kind** and its account-relative **path**,
//! which is what a frontend needs to re-list a directory or re-fetch a file.

use std::sync::{mpsc, Arc, Mutex};

use wusel_core::provider::Invalidation;

use crate::wire::ChangeKind;

/// A server-side change, as a `watch` subscriber receives it.
#[derive(Debug, Clone)]
pub struct Change {
    pub kind: ChangeKind,
    pub path: String,
}

impl From<Invalidation> for Change {
    fn from(inv: Invalidation) -> Self {
        match inv {
            Invalidation::Entry { path, .. } => Change {
                kind: ChangeKind::Entry,
                path,
            },
            Invalidation::Content { path, .. } => Change {
                kind: ChangeKind::Content,
                path,
            },
        }
    }
}

/// The most changes retained for [`Events::changes_since`]. Server-side changes
/// are human-frequency, so a few thousand covers a long session; older entries
/// are dropped and their sequence numbers retired.
const MAX_LOG: usize = 4096;

/// The retained change log with the sequence number of its first entry.
struct Log {
    /// Changes, oldest first.
    entries: Vec<Change>,
    /// The sequence number of `entries[0]`. It grows as old entries are trimmed,
    /// so `base + entries.len()` is always the current head sequence.
    base: u64,
}

/// The changes at or after `since`, and the head sequence to anchor from next.
///
/// A `since` past the head yields nothing (the caller is up to date). A `since`
/// before the retained window yields everything retained — a harmless superset,
/// since replaying an update or a delete on the frontend is idempotent.
fn slice_since(entries: &[Change], base: u64, since: u64) -> (u64, Vec<Change>) {
    let head = base + entries.len() as u64;
    let start = since.saturating_sub(base).min(entries.len() as u64) as usize;
    (head, entries[start..].to_vec())
}

/// Fans one engine invalidation stream out to every `watch` subscriber, and
/// keeps a bounded log a `changes` caller can replay from a sequence anchor.
pub struct Events {
    subscribers: Arc<Mutex<Vec<mpsc::Sender<Change>>>>,
    log: Arc<Mutex<Log>>,
}

impl Events {
    /// Start the fan-out: read the engine's invalidations on a background thread,
    /// append each to the replay log, and forward a mapped [`Change`] to every
    /// current subscriber. The thread ends when the engine drops its sender
    /// (daemon shutdown), which drops every subscriber receiver in turn and so
    /// ends each `watch` connection.
    #[must_use]
    pub fn start(invalidations: mpsc::Receiver<Invalidation>) -> Arc<Events> {
        let subscribers: Arc<Mutex<Vec<mpsc::Sender<Change>>>> = Arc::default();
        let log = Arc::new(Mutex::new(Log {
            entries: Vec::new(),
            base: 0,
        }));
        let subs = Arc::clone(&subscribers);
        let log_w = Arc::clone(&log);
        // Best-effort spawn: if it fails there is simply no change signalling,
        // which the read path survives (it degrades to the client's own polling).
        let _ = std::thread::Builder::new()
            .name("wusel-ipc-events".into())
            .spawn(move || {
                for inv in invalidations {
                    let change = Change::from(inv);
                    {
                        let mut l = log_w.lock().unwrap_or_else(|e| e.into_inner());
                        l.entries.push(change.clone());
                        // Trim the oldest past the cap, retiring their sequence
                        // numbers so anchors stay monotonic.
                        if l.entries.len() > MAX_LOG {
                            let drop = l.entries.len() - MAX_LOG;
                            l.entries.drain(0..drop);
                            l.base += drop as u64;
                        }
                    }
                    let mut list = subs.lock().unwrap_or_else(|e| e.into_inner());
                    // Send to all; drop the ones whose client has hung up.
                    list.retain(|tx| tx.send(change.clone()).is_ok());
                }
            });
        Arc::new(Events { subscribers, log })
    }

    /// Register a `watch` connection; it receives every change from now on.
    #[must_use]
    pub fn subscribe(&self) -> mpsc::Receiver<Change> {
        let (tx, rx) = mpsc::channel();
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        rx
    }

    /// The change log at or after sequence `since`, and the head sequence to use
    /// as the next anchor. See [`slice_since`].
    #[must_use]
    pub fn changes_since(&self, since: u64) -> (u64, Vec<Change>) {
        let l = self.log.lock().unwrap_or_else(|e| e.into_inner());
        slice_since(&l.entries, l.base, since)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(name: &str) -> Change {
        Change {
            kind: ChangeKind::Entry,
            path: format!("Angebote/{name}"),
        }
    }

    #[test]
    fn slice_since_returns_the_tail_and_head() {
        let entries = vec![change("a"), change("b"), change("c")];
        // From the start: everything, head = 3.
        let (head, got) = slice_since(&entries, 0, 0);
        assert_eq!(head, 3);
        assert_eq!(got.len(), 3);
        // From an interior anchor: only the tail.
        let (head, got) = slice_since(&entries, 0, 2);
        assert_eq!(head, 3);
        assert_eq!(
            got.iter().map(|c| c.path.clone()).collect::<Vec<_>>(),
            vec!["Angebote/c"]
        );
        // Caught up: nothing.
        let (head, got) = slice_since(&entries, 0, 3);
        assert_eq!(head, 3);
        assert!(got.is_empty());
        // Past the head (a stale/oversized anchor): still nothing, not a panic.
        let (_, got) = slice_since(&entries, 0, 99);
        assert!(got.is_empty());
    }

    #[test]
    fn slice_since_respects_a_trimmed_base() {
        // The first two entries were trimmed: base = 2, entries hold seq 2 and 3.
        let entries = vec![change("c"), change("d")];
        let (head, got) = slice_since(&entries, 2, 3);
        assert_eq!(head, 4);
        assert_eq!(
            got.iter().map(|c| c.path.clone()).collect::<Vec<_>>(),
            vec!["Angebote/d"]
        );
        // An anchor before the retained window yields the whole retained tail.
        let (_, got) = slice_since(&entries, 2, 0);
        assert_eq!(got.len(), 2);
    }
}
