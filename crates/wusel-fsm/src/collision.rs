// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! What happens when a request arrives for an object that is already busy.
//!
//! This is the whole of the concurrency policy. It lives in one function so
//! there is one place to read, one place to change, and — because every row
//! below has a test named after it in `tests/collision_policy.rs` — one place
//! that is demonstrated rather than asserted.

use crate::registry::Facts;
use crate::script::{Flow, Step};
use crate::Intent;

/// What to do with a request that met a busy object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Collision {
    /// Park it; it starts when the object goes idle. This is where `write` →
    /// `flush` ordering comes from, without a lock.
    ///
    /// The policy table calls some of these rows "wait" — waiting for the
    /// running flow and queueing behind it are the same act seen from the two
    /// ends, and one mechanism is better than two names for it.
    Queue,
    /// Ride along on the running flow: one transfer, several answers.
    Join,
    /// Give up the running flow, then start this one. Only when the arrival
    /// makes the running work **pointless** — never merely to overtake it.
    Abort,
    /// Drop the arrival. It has nothing to contribute and no one is owed an
    /// answer beyond "nothing to do".
    Skip,
}

/// Decide what an arriving intent does to a running flow.
///
/// The policy, row by row — the table from the concurrency page, which this
/// function *is*:
///
/// | Running | Arriving | Decision | Why |
/// |---|---|---|---|
/// | Hydration | `write` | Queue | the base is what a later three-way merge is made of; aborting destroys it |
/// | Hydration (background refresh) | `remove` | Abort | the object is gone, so the rest of the download is waste |
/// | Hydration | second read, same range | Join | one transfer, two answers |
/// | Upload | `write` | Queue | aborting mid-upload can leave half an object on the server |
/// | Upload | `remove` | Queue | the upload may already have landed, so delete what is really there |
/// | Upload | `rename` | Queue | the office-suite atomic save; ordering is everything |
/// | Conflict resolution | anything | Queue | it owns the buffer |
/// | Listing | second listing | Join | one PROPFIND answers both |
/// | anything | refresh, local edit pending | Skip | a server-side change must never overwrite an unsaved local one |
///
/// Everything not named above queues. That default is deliberate and it is the
/// safe one: running in turn is always correct, merely sometimes slower than it
/// had to be. Join and Abort are the optimisations, and they are opt-in per row
/// precisely because getting them wrong loses work.
#[must_use]
pub fn collision(running: &Flow, arriving: &Intent, facts: &Facts) -> Collision {
    // Row 9 first, because it holds whatever is running: an unsaved local edit
    // outranks anything the server has to say. Until our upload lands, our copy
    // *is* the newer one, and the next invalidation will tell us the truth
    // anyway.
    if matches!(arriving, Intent::Refresh) && facts.buffer_dirty {
        return Collision::Skip;
    }

    // Conflict resolution owns the buffer outright — it is reading it, merging
    // it and re-uploading it — so nothing may touch the object until it is done.
    if running.step == Step::PubConflict {
        return Collision::Queue;
    }

    match &running.intent {
        // A read in flight. A second reader of the same range wants exactly the
        // bytes already on their way.
        Intent::Fetch { offset, len } => match arriving {
            Intent::Fetch {
                offset: o2,
                len: l2,
            } if offset == o2 && len == l2 => Collision::Join,
            Intent::Fetch { .. }
            | Intent::Write { .. }
            | Intent::Stat
            | Intent::Enumerate
            | Intent::Materialise { .. }
            | Intent::Publish
            | Intent::Remove { .. }
            | Intent::Move { .. }
            | Intent::Refresh
            | Intent::Lookup { .. }
            | Intent::State
            | Intent::Relist
            | Intent::SetAttr { .. } => Collision::Queue,
        },

        // A write that is still fetching its base. Aborting here would throw
        // away the merge base — and an editor saving every two seconds would
        // restart the download forever, so it would never finish.
        Intent::Write { .. } => match arriving {
            Intent::Fetch { .. }
            | Intent::Write { .. }
            | Intent::Stat
            | Intent::Enumerate
            | Intent::Materialise { .. }
            | Intent::Publish
            | Intent::Remove { .. }
            | Intent::Move { .. }
            | Intent::Refresh
            | Intent::Lookup { .. }
            | Intent::State
            | Intent::Relist
            | Intent::SetAttr { .. } => Collision::Queue,
        },

        // A listing. A second one wants the same answer.
        // A listing somebody is waiting for. A second one wants the same
        // answer, and a background refresh is redundant while it runs.
        Intent::Enumerate => match arriving {
            Intent::Enumerate => Collision::Join,
            Intent::Relist => Collision::Skip,
            Intent::Fetch { .. }
            | Intent::Write { .. }
            | Intent::Stat
            | Intent::Materialise { .. }
            | Intent::Publish
            | Intent::Remove { .. }
            | Intent::Move { .. }
            | Intent::Refresh
            | Intent::Lookup { .. }
            | Intent::State
            | Intent::SetAttr { .. } => Collision::Queue,
        },

        // A background refresh, which nobody is waiting for and which therefore
        // produces no listing to hand out. A caller that *does* want entries
        // must not ride along on it — joining here would answer them with
        // nothing. It waits instead; the refresh is short.
        Intent::Relist => match arriving {
            Intent::Relist => Collision::Skip,
            Intent::Fetch { .. }
            | Intent::Write { .. }
            | Intent::Stat
            | Intent::Enumerate
            | Intent::Materialise { .. }
            | Intent::Publish
            | Intent::Remove { .. }
            | Intent::Move { .. }
            | Intent::Refresh
            | Intent::Lookup { .. }
            | Intent::State
            | Intent::SetAttr { .. } => Collision::Queue,
        },

        // An upload. Never aborted: a chunked upload cut off mid-flight leaves
        // a half object on the server, and a delete arriving now has to apply
        // to whatever really ends up there.
        Intent::Publish => match arriving {
            Intent::Fetch { .. }
            | Intent::Write { .. }
            | Intent::Stat
            | Intent::Enumerate
            | Intent::Materialise { .. }
            | Intent::Publish
            | Intent::Remove { .. }
            | Intent::Move { .. }
            | Intent::Refresh
            | Intent::Lookup { .. }
            | Intent::State
            | Intent::Relist
            | Intent::SetAttr { .. } => Collision::Queue,
        },

        // A background refresh. It is speculative work by definition, so an
        // arrival that makes it pointless wins.
        Intent::Refresh => match arriving {
            Intent::Remove { .. } => Collision::Abort,
            Intent::Refresh => Collision::Skip, // at most one pending refresh
            Intent::Fetch { .. }
            | Intent::Write { .. }
            | Intent::Stat
            | Intent::Enumerate
            | Intent::Materialise { .. }
            | Intent::Publish
            | Intent::Move { .. }
            | Intent::Lookup { .. }
            | Intent::State
            | Intent::Relist
            | Intent::SetAttr { .. } => Collision::Queue,
        },

        // Short, local flows. Nothing gains from riding along with them, and
        // they finish quickly enough that queueing costs nothing worth naming.
        Intent::Stat
        | Intent::Materialise { .. }
        | Intent::Remove { .. }
        | Intent::Move { .. }
        | Intent::Lookup { .. }
        | Intent::State
        | Intent::SetAttr { .. } => Collision::Queue,
    }
}
