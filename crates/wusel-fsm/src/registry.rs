// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! What the machine knows about an object without asking anybody.
//!
//! Every script starts by asking questions like "is a write buffer open for
//! this object?" and "is it dirty?". Those are decisions, and the design's one
//! rule says a decision costs no I/O — so the answers cannot live behind a
//! thread hop into the engine. They live here.
//!
//! That placement is also the portability rule in miniature: the engine may
//! *offer* a write buffer, but it must never be the **authority** on whether an
//! object is dirty. Under FUSE we hold that authority; on Windows and macOS the
//! operating system does and tells us afterwards. Two holders of one truth
//! surface late and look like data loss.

use std::collections::HashMap;

use crate::ObjectId;

/// One object's write buffer, as far as decisions are concerned.
///
/// The bytes are a file somewhere — that is the I/O worker's business. What the
/// machine needs is only what a decision turns on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Buffer {
    /// Written to since it was opened. A clean buffer is discarded rather than
    /// uploaded.
    pub dirty: bool,
    /// The version the buffer was based on — the precondition a later upload
    /// asserts. Empty means "never seen on the server".
    pub base_etag: String,
    /// An mtime set through `setattr`, to travel with the next upload so
    /// `cp -p` preserves timestamps.
    pub pending_mtime: Option<i64>,
    /// An ephemeral editor or office file: it lives entirely in the buffer and
    /// is never sent to the server.
    pub ignored: bool,
}

impl Buffer {
    /// A fresh buffer for an object whose server version is `base_etag`.
    #[must_use]
    pub fn new(base_etag: String) -> Self {
        Self {
            dirty: false,
            base_etag,
            pending_mtime: None,
            ignored: false,
        }
    }
}

/// The facts a script may consult between steps, snapshotted for one decision.
///
/// Passed to [`crate::advance`] rather than read from a shared map inside it:
/// that is what keeps the transition function a pure function of its inputs,
/// and therefore testable without a machine, a mount or a database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    pub buffer_open: bool,
    pub buffer_dirty: bool,
    pub ignored: bool,
    pub base_etag: String,
    /// An mtime set through the attribute path, to travel with the upload so
    /// `cp -p` and `rsync -t` preserve timestamps server-side.
    pub pending_mtime: Option<i64>,
}

/// Every open write buffer, keyed by object. Not in the map = no buffer.
#[derive(Debug, Default)]
pub struct Registry {
    buffers: HashMap<ObjectId, Buffer>,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot what a decision may look at.
    #[must_use]
    pub fn facts(&self, object: ObjectId) -> Facts {
        match self.buffers.get(&object) {
            Some(b) => Facts {
                buffer_open: true,
                buffer_dirty: b.dirty,
                ignored: b.ignored,
                base_etag: b.base_etag.clone(),
                pending_mtime: b.pending_mtime,
            },
            None => Facts::default(),
        }
    }

    /// Register a buffer, replacing any previous one.
    pub fn open(&mut self, object: ObjectId, buffer: Buffer) {
        self.buffers.insert(object, buffer);
    }

    /// Forget a buffer. The file itself is removed by an I/O job — dropping only
    /// the bookkeeping would leak it until the next start.
    pub fn close(&mut self, object: ObjectId) -> Option<Buffer> {
        self.buffers.remove(&object)
    }

    /// Remember a timestamp for the next upload to carry. No-op when no buffer
    /// is open: there is nothing for it to travel with.
    pub fn set_pending_mtime(&mut self, object: ObjectId, mtime: i64) {
        if let Some(b) = self.buffers.get_mut(&object) {
            b.pending_mtime = Some(mtime);
        }
    }

    /// Mark the buffer written to. No-op when none is open, which is the
    /// harmless reading: nothing to dirty.
    pub fn mark_dirty(&mut self, object: ObjectId) {
        if let Some(b) = self.buffers.get_mut(&object) {
            b.dirty = true;
        }
    }

    #[must_use]
    pub fn get(&self, object: ObjectId) -> Option<&Buffer> {
        self.buffers.get(&object)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.buffers.len()
    }

    /// How many open buffers hold unsaved edits — the count a diagnostics
    /// snapshot reports so "N dirty buffers" is visible without naming a file.
    #[must_use]
    pub fn dirty_count(&self) -> usize {
        self.buffers.values().filter(|b| b.dirty).count()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_object_with_no_buffer_reports_nothing_open() {
        let reg = Registry::new();
        assert_eq!(reg.facts(ObjectId(1)), Facts::default());
    }

    #[test]
    fn a_dirty_buffer_is_visible_to_a_decision() {
        let mut reg = Registry::new();
        reg.open(ObjectId(1), Buffer::new("v1".into()));
        assert!(reg.facts(ObjectId(1)).buffer_open);
        assert!(!reg.facts(ObjectId(1)).buffer_dirty);

        reg.mark_dirty(ObjectId(1));
        let f = reg.facts(ObjectId(1));
        assert!(f.buffer_dirty);
        assert_eq!(f.base_etag, "v1");
    }

    #[test]
    fn closing_forgets_the_buffer() {
        let mut reg = Registry::new();
        reg.open(ObjectId(2), Buffer::new(String::new()));
        assert!(reg.close(ObjectId(2)).is_some());
        assert!(reg.is_empty());
    }
}
