// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The wire format of a diagnostics snapshot.
//!
//! One JSON shape, agreed by the two ends that never share a process: the mount
//! serves it on a unix socket (the producer), and `wusel doctor` — a separate
//! invocation of the binary — reads it back (the consumer). It lives here
//! because both sides depend on `wusel-core`, and a wire format with two
//! definitions drifts.
//!
//! Name-free by construction, inherited from [`crate::runtime::SubstrateSnapshot`]
//! and the machine snapshot under it: a support bundle must never carry the
//! user's file names, so the report speaks in inode numbers and work kinds.

use serde::{Deserialize, Serialize};

use crate::runtime::SubstrateSnapshot;

/// The wire version. Bumped only on a breaking change to the shape, so an older
/// `doctor` reading a newer mount (or the reverse) can say so instead of
/// misparsing.
pub const SCHEMA: u32 = 1;

/// A diagnostics snapshot as it crosses the socket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiagReport {
    /// The wire version; see [`SCHEMA`].
    pub schema: u32,
    /// What the state machine is doing.
    pub machine: MachineReport,
    /// Background refreshes in flight.
    pub refreshing: usize,
    /// File ids of whole-file hydrations running right now — the background
    /// downloads that never become flows and so appear nowhere in `machine`.
    ///
    /// File ids rather than paths, for the same reason the rest of this report
    /// speaks in inodes: a support bundle must not carry the user's names.
    /// Whoever holds the state database resolves them (`wusel status` does).
    ///
    /// Defaulted rather than required, so a newer `doctor` reading an older
    /// mount gets an empty list instead of a parse error — which is why adding
    /// it did not need a [`SCHEMA`] bump.
    #[serde(default)]
    pub hydrating: Vec<u64>,
    /// How many threads each pool runs.
    pub pools: PoolsReport,
    /// FUSE replies parked while their work runs — the count that, held against
    /// the kernel's `waiting`, tells a lost reply from a busy one. Filled by the
    /// FUSE frontend; `None` when the report is produced without one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub replies_pending: Option<usize>,
    /// What the notify_push listener is doing. Filled by the daemon that runs
    /// one; `None` from a report produced without it, and from a mount older
    /// than this field — defaulted for that reason, like `hydrating`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub push: Option<PushReport>,
}

/// Where the notify_push listener stands — the mount's live-update channel.
///
/// The point of reporting it: whether live updates work is invisible from
/// outside. The mount works either way (TTL polling is the fallback), so a
/// WebSocket path that a reverse proxy does not pass through shows up only as
/// listings going stale — and, until then, as a daemon that keeps failing to
/// connect while every plain HTTP request succeeds. Only the listener itself
/// can say which of the two it is.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PushPhase {
    /// Asking the server whether it offers notify_push at all.
    Discovering,
    /// It does not (or the lookup was refused for good): the mount polls.
    Unavailable,
    /// The endpoint is known and the first connection is being made.
    Connecting,
    /// Authenticated; file-change events arrive.
    Connected,
    /// The connection ended and the listener is waiting out its backoff.
    Reconnecting,
    /// The listener was asked to stop, or gave up for good.
    Stopped,
    /// A phase this build does not know — the daemon is newer than `doctor`.
    /// Catches the variant instead of failing the whole report over it.
    #[serde(other)]
    Unknown,
}

impl PushPhase {
    /// The phase as it is spelled on the wire — for a report that quotes it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discovering => "discovering",
            Self::Unavailable => "unavailable",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
            Self::Stopped => "stopped",
            Self::Unknown => "unknown",
        }
    }
}

/// The notify_push listener's state, as it crosses the socket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushReport {
    pub phase: PushPhase,
    /// Seconds the current phase has lasted.
    pub since_secs: u64,
    /// The advertised WebSocket endpoint, once discovered. A URL the server
    /// publishes, not a user path — nothing private in it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub endpoint: Option<String>,
    /// Failed attempts since the last successful authentication.
    pub failures: u32,
    /// Successful authentications since the mount started.
    pub connects: u32,
    /// The most recent failure, verbatim — the line `doctor` exists to surface.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_error: Option<String>,
}

/// The machine's occupancy, mirroring [`wusel_fsm::MachineSnapshot`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MachineReport {
    pub objects: Vec<ObjectReport>,
    pub buffers_open: usize,
    pub buffers_dirty: usize,
}

/// One busy object, mirroring [`wusel_fsm::BusyObject`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObjectReport {
    pub object: u64,
    pub intent: String,
    pub step: String,
    pub outstanding: bool,
    pub waiters: usize,
    pub queued: usize,
    pub abort: bool,
}

/// Pool sizes, mirroring [`crate::runtime::PoolSizes`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PoolsReport {
    pub db_readers: usize,
    pub net: usize,
    pub file: usize,
}

impl DiagReport {
    /// Build the wire report from a substrate snapshot. The FUSE frontend adds
    /// `replies_pending` afterwards; core has no notion of a parked reply.
    #[must_use]
    pub fn from_substrate(s: &SubstrateSnapshot) -> Self {
        Self {
            schema: SCHEMA,
            machine: MachineReport {
                objects: s
                    .machine
                    .objects
                    .iter()
                    .map(|o| ObjectReport {
                        object: o.object,
                        intent: o.intent.to_string(),
                        step: o.step.clone(),
                        outstanding: o.outstanding,
                        waiters: o.waiters,
                        queued: o.queued,
                        abort: o.abort,
                    })
                    .collect(),
                buffers_open: s.machine.buffers_open,
                buffers_dirty: s.machine.buffers_dirty,
            },
            refreshing: s.refreshing,
            hydrating: s.hydrating.clone(),
            pools: PoolsReport {
                db_readers: s.pools.db_readers,
                net: s.pools.net,
                file: s.pools.file,
            },
            replies_pending: None,
            push: None,
        }
    }

    /// Serialize to the JSON that crosses the socket.
    ///
    /// # Errors
    /// If serialization fails, which for these plain-data types it does not in
    /// practice — the signature keeps the caller honest all the same.
    pub fn to_json(&self) -> crate::Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| crate::Error::Other(format!("serialize diagnostics: {e}")))
    }

    /// Parse the JSON a mount served.
    ///
    /// # Errors
    /// If the bytes are not the JSON this version understands.
    pub fn from_json(s: &str) -> crate::Result<Self> {
        serde_json::from_str(s).map_err(|e| crate::Error::Other(format!("parse diagnostics: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{PoolSizes, SubstrateSnapshot};
    use wusel_fsm::{BusyObject, MachineSnapshot};

    fn a_snapshot() -> SubstrateSnapshot {
        SubstrateSnapshot {
            machine: MachineSnapshot {
                objects: vec![BusyObject {
                    object: 1234,
                    intent: "fetch",
                    step: "FetchBytes".to_string(),
                    outstanding: true,
                    waiters: 3,
                    queued: 1,
                    abort: false,
                }],
                buffers_open: 2,
                buffers_dirty: 1,
            },
            refreshing: 0,
            hydrating: vec![9001],
            pools: PoolSizes {
                db_readers: 2,
                net: 4,
                file: 2,
            },
        }
    }

    #[test]
    fn a_report_round_trips_through_json() {
        let mut report = DiagReport::from_substrate(&a_snapshot());
        report.replies_pending = Some(3);
        report.push = Some(PushReport {
            phase: PushPhase::Reconnecting,
            since_secs: 17,
            endpoint: Some("wss://cloud.example.org/push/ws".into()),
            failures: 4,
            connects: 1,
            last_error: Some("websocket upgrade refused with 502 Bad Gateway".into()),
        });
        let back = DiagReport::from_json(&report.to_json().unwrap()).unwrap();
        assert_eq!(report, back);
    }

    /// A `doctor` older than the daemon must still read the report: an unknown
    /// phase becomes `Unknown`, and a report without the field parses at all.
    #[test]
    fn a_newer_or_older_daemon_still_parses() {
        let mut json = DiagReport::from_substrate(&a_snapshot()).to_json().unwrap();
        assert!(!json.contains("\"push\""), "absent, not null, when unset");
        let back = DiagReport::from_json(&json).unwrap();
        assert_eq!(back.push, None);

        json.insert_str(
            json.len() - 1,
            r#","push":{"phase":"teleporting","since_secs":1,"failures":0,"connects":0}"#,
        );
        let back = DiagReport::from_json(&json).unwrap();
        assert_eq!(back.push.unwrap().phase, PushPhase::Unknown);
    }

    #[test]
    fn the_report_preserves_the_stuck_flow_details() {
        let report = DiagReport::from_substrate(&a_snapshot());
        assert_eq!(report.schema, SCHEMA);
        let o = &report.machine.objects[0];
        assert_eq!(o.object, 1234);
        assert_eq!(o.intent, "fetch");
        assert_eq!(o.step, "FetchBytes");
        assert_eq!(o.waiters, 3);
        assert_eq!(report.pools.net, 4);
        // The background download nothing else can see: absent from `machine`,
        // because a hydration never becomes a flow.
        assert_eq!(report.hydrating, vec![9001]);
        assert!(report.machine.objects.iter().all(|o| o.object != 9001));
        // Produced without a frontend: no parked-reply count, and omitted from
        // the JSON rather than written as null.
        assert_eq!(report.replies_pending, None);
        assert!(!report.to_json().unwrap().contains("replies_pending"));
    }
}
