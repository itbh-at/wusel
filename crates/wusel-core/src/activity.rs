// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Which objects the machine is changing, for the listings that could catch
//! it halfway.
//!
//! A server listing is a snapshot, and applying it overwrites rows by
//! (parent, name). That is only safe when nothing local changed that directory
//! while the snapshot was being taken. The machine orders the flows on one
//! object, but a listing reaches across objects: the sync walk and the
//! background refresh run beside the machine altogether, and a flow's own
//! relist — the parent listing after a file's first upload — reconciles rows
//! other flows may be changing.
//!
//! The case that made this necessary is an atomic save. The rename runs as a
//! server-side `MOVE`, then as `MoveRows` locally — two steps, with the
//! directory held busy by the machine for both. A walk that lists the directory
//! in between sees the server *after* the move and the rows *before* it: the
//! temporary is gone from the listing, so reconcile deletes its row, and
//! `MoveRows` then fails on a row that no longer exists — EIO on the rename.
//!
//! So the decider reports here which objects have a state-changing flow
//! running (reads are left out, see `Intent::changes_state`). A listing is
//! dropped if its directory was changing at any time from before the PROPFIND
//! to the reconcile, and a child that was changing keeps its row. "Busy" spans
//! a flow from before its first server request to after its last local commit,
//! which is exactly the window a listing must not straddle.
//!
//! Shared across threads, hence the `Mutex`: the decider writes, the walk and
//! the refresh workers read. Every access is a few hash-map operations.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// How many busy/idle transitions are remembered. Past that, the history is
/// dropped and every listing taken before the drop is refused — conservative,
/// and cheap: the next walk or refresh lists again.
const HISTORY: usize = 4096;

#[derive(Default)]
pub struct Activity {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Counts every change of the busy set.
    seq: u64,
    busy: HashSet<u64>,
    /// The `seq` at which each object last became busy or idle.
    changed: HashMap<u64, u64>,
    /// Listings taken before this `seq` predate the remembered history.
    floor: u64,
}

impl Activity {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The decider's report: exactly these objects are busy now.
    pub fn set_busy(&self, now: impl IntoIterator<Item = u64>) {
        let now: HashSet<u64> = now.into_iter().collect();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if now == inner.busy {
            return;
        }
        inner.seq += 1;
        let seq = inner.seq;
        let flipped: Vec<u64> = now.symmetric_difference(&inner.busy).copied().collect();
        for object in flipped {
            inner.changed.insert(object, seq);
        }
        inner.busy = now;
        if inner.changed.len() > HISTORY {
            inner.changed.clear();
            inner.floor = seq;
        }
    }

    /// Where the history stands. Taken before a PROPFIND and handed back to
    /// [`Self::quiet_since`] after it.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).seq
    }

    /// Whether `object` is idle now and has been since `since` — the condition
    /// for applying a listing of it that was taken at `since`.
    #[must_use]
    pub fn quiet_since(&self, object: u64, since: u64) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        since >= inner.floor
            && !inner.busy.contains(&object)
            && inner.changed.get(&object).is_none_or(|&at| at <= since)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_throughout_is_quiet() {
        let a = Activity::new();
        a.set_busy([7]);
        a.set_busy([]);
        let since = a.seq();
        assert!(a.quiet_since(7, since));
        assert!(a.quiet_since(8, since));
    }

    #[test]
    fn busy_now_is_not_quiet() {
        let a = Activity::new();
        let since = a.seq();
        a.set_busy([7]);
        assert!(!a.quiet_since(7, since));
        assert!(a.quiet_since(8, since), "only the busy object is affected");
    }

    #[test]
    fn busy_and_released_in_between_is_not_quiet() {
        let a = Activity::new();
        let since = a.seq();
        a.set_busy([7]);
        a.set_busy([]);
        assert!(!a.quiet_since(7, since));
    }

    #[test]
    fn a_listing_older_than_the_history_is_refused() {
        let a = Activity::new();
        let since = a.seq();
        for object in 0..=HISTORY as u64 {
            a.set_busy([object + 100_000]);
        }
        a.set_busy([]);
        assert!(
            !a.quiet_since(7, since),
            "7 never changed, but that is no longer knowable"
        );
        assert!(a.quiet_since(7, a.seq()));
    }
}
