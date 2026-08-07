// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The execution substrate: the deciding thread, and the pools it hands work to.
//!
//! [`wusel_fsm`] says *what* should happen; this module is where it happens.
//! The shape is the one the concurrency design argues for:
//!
//! * one **deciding thread** owning the machine. It decides and nothing else —
//!   it holds no connection and opens no file, which the crate boundary of
//!   `wusel-fsm` already guarantees rather than merely asks for.
//! * **N database readers**, each with its own connection. WAL allows many
//!   readers alongside one writer, and the common operations are reads, so a
//!   metadata lookup must not queue behind a write that a virus scanner is
//!   holding up. `tests/db_concurrency.rs` is that claim, made falsifiable.
//! * **one database writer**, also with its own connection, so writes serialise
//!   in one place rather than by luck.
//! * **network and file pools**, where blocking is the job rather than a
//!   problem.
//!
//! What this replaces is a single owner thread that held the whole engine and
//! ran each operation to completion. That removed a lock; it did not remove the
//! serialisation, and one slow filesystem still stalled every callback.
//!
//! Every worker has the same capabilities. Which pool a job goes to is decided
//! by [`wusel_fsm::Job::executor`] and expresses how it is expected to block —
//! not what it is allowed to touch.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use wusel_fsm::{
    Action, Buffer, Completion, Executor, Failure, Job, Machine, NodeFacts, ObjectId, Outcome,
    Request, RequestId,
};

use crate::content::ContentSource;
use crate::provider::FileState;
use crate::provider::{
    child_path, read_range_from_scratch, run_conflict_resolution, run_reload_dir, WriteContext,
};

use crate::state::{NodeRow, StateDb};

/// What reaches the deciding thread.
enum Event {
    Request(Request),
    /// Nobody is waiting for this request any more.
    Abandon(RequestId),
    Completed {
        object: ObjectId,
        completion: Completion,
        /// What the step produced. Carried beside the completion rather than
        /// inside it: the machine decides, and a result is not a decision.
        payload: Payload,
    },
    /// A background refresh finished. Carries no result — it is only how the
    /// decider learns that this object may be refreshed again.
    Refreshed {
        object: ObjectId,
    },
    /// A read-only metadata job that ran beside a busy object finished. It
    /// carries its own request and payload rather than routing through the
    /// object-keyed path, because the object may be busy with an upload whose
    /// answer and payload are unrelated to this read's.
    CompletedBeside {
        request: RequestId,
        completion: Completion,
        payload: Payload,
    },
    /// Recover an upload a crash or a transient failure left owed: seed the
    /// buffer if the registry lost it (after a restart it is empty, but the
    /// buffer file and the durable record survive), then publish. Carries the
    /// precondition from the record so a resumed upload stays conditional.
    ResumeUpload {
        object: ObjectId,
        base_etag: String,
        mtime: Option<i64>,
    },
    /// Someone asked what the decider is doing. It answers on the channel with
    /// its own part of a diagnostics snapshot and carries on. Because it is an
    /// ordinary event, the decider stays the sole owner of the machine — no lock,
    /// no shared state — and answering it is proof the decider is *not* wedged.
    Diag(Sender<DeciderDiag>),
    Stop,
}

/// The decider's own contribution to a diagnostics snapshot: what the state
/// machine is doing, and how many background refreshes are in flight.
pub struct DeciderDiag {
    pub machine: wusel_fsm::MachineSnapshot,
    pub refreshing: usize,
}

/// How many threads each pool runs. Reported so a support bundle shows whether
/// the mount is running one dispatch thread or several.
#[derive(Debug, Clone, Copy)]
pub struct PoolSizes {
    pub db_readers: usize,
    pub net: usize,
    pub file: usize,
}

/// A read-only picture of the running substrate, for diagnostics.
///
/// The engine half of what `wusel doctor` reports: the machine's occupancy (see
/// [`wusel_fsm::MachineSnapshot`]), the number of background refreshes running,
/// and the pool sizes. Name-free, like the machine snapshot it wraps.
#[derive(Debug, Clone)]
pub struct SubstrateSnapshot {
    pub machine: wusel_fsm::MachineSnapshot,
    pub refreshing: usize,
    pub pools: PoolSizes,
}

/// A cloneable, `Send` handle for taking diagnostics snapshots of a running
/// substrate. Just a channel sender and the pool sizes, so it can be handed to
/// the diagnostics socket thread after the substrate itself has moved into the
/// FUSE session.
#[derive(Clone)]
pub struct DiagHandle {
    to_fsm: Sender<Event>,
    pools: PoolSizes,
}

impl DiagHandle {
    /// Ask the decider for a snapshot and merge in the pool sizes.
    ///
    /// The request is an ordinary event the decider answers between other work,
    /// which keeps it the sole owner of the machine — no lock — and makes the
    /// answer itself proof the decider is not wedged. The wait is two seconds,
    /// not forever: a decider that ever did wedge becomes the diagnosis rather
    /// than a second hang. (No known failure wedges it — a stalled mount is
    /// unanswered kernel reads while the decider sits idle.)
    ///
    /// # Errors
    /// If the substrate has stopped, or the decider does not answer in time.
    pub fn snapshot(&self) -> crate::Result<SubstrateSnapshot> {
        let (tx, rx) = channel::<DeciderDiag>();
        self.to_fsm
            .send(Event::Diag(tx))
            .map_err(|_| crate::Error::Other("the decider is gone".into()))?;
        let diag = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| crate::Error::Other("the decider did not answer in time".into()))?;
        Ok(SubstrateSnapshot {
            machine: diag.machine,
            refreshing: diag.refreshing,
            pools: self.pools,
        })
    }
}

/// What a step produced, on its way to whoever is waiting.
///
/// Kept beside the machine rather than inside it: results are not decisions,
/// and a mechanism that must stay portable has no business carrying one
/// frontend's reply shapes. The variants are exactly what an operation can
/// hand back — nothing speculative.
#[derive(Debug, Clone, Default)]
pub enum Payload {
    /// The operation has nothing to return but its success.
    #[default]
    None,
    /// Content, for a read.
    Bytes(Vec<u8>),
    /// One object's row, for an attribute lookup.
    Node(Box<NodeRow>),
    /// A directory's children.
    Entries(Vec<NodeRow>),
    /// How many bytes a write accepted.
    Written(u32),
    /// What an OS integration should draw for this object.
    State(FileState),
}

/// A finished request, on its way back to whoever asked.
#[derive(Debug)]
pub struct Answered {
    pub requests: Vec<RequestId>,
    pub outcome: Outcome,
    pub payload: Payload,
}

/// What a worker needs to carry out a job.
///
/// Everything here is shareable by construction — an `Arc` or a per-thread
/// connection — which is what makes several workers possible at all.
pub struct Context {
    /// Where the state database lives, so each worker can open its own
    /// connection to it rather than share one.
    pub db_path: PathBuf,
    /// Content delivery. Already `Send + Sync` by contract, because the caching
    /// decorator's single-flight coordination relies on it.
    pub content: Arc<dyn ContentSource>,
    /// Where write buffers live, so the file pool can serve a range out of one.
    pub scratch_dir: PathBuf,
    /// The user's "keep this offline" list, shared with the Provider rather than
    /// re-read per worker: it lives in a file now, and one cache for the process
    /// is both faster and impossible to disagree with itself.
    pub pins: Arc<crate::pins::Pins>,
    /// What to serve when an outdated pinned file is opened.
    pub open_pinned: crate::config::OpenPinned,
    /// Whether the connection costs money, cached — see [`Metered`].
    pub metered: Arc<Metered>,
    /// Names that never reach the server — editor swap files, office locks.
    pub ignore_patterns: Vec<String>,
    /// Re-list a directory whose last listing is older than this.
    pub revalidate_secs: u64,
    /// Rate limit for push-triggered re-lists.
    pub push_floor_secs: u64,
    /// Unix seconds of the last push signal; a listing at or before it is stale
    /// however young it is.
    pub invalidate_after: Arc<std::sync::atomic::AtomicI64>,
    /// Whether `flush` returns before the upload (asynchronous write-back, the
    /// default) or waits for it (`[sync] upload = sync`).
    pub async_upload: bool,
    /// The shareable half of the engine's write path — the WebDAV client, the
    /// runtime, the desktop channel. `None` leaves the network steps unwired,
    /// which is what the substrate-level tests use.
    pub write: Option<WriteContext>,
}

/// How many threads each pool gets.
#[derive(Debug, Clone, Copy)]
pub struct Pools {
    pub db_readers: usize,
    pub net: usize,
    pub file: usize,
}

impl Default for Pools {
    fn default() -> Self {
        Self {
            db_readers: 2,
            net: 4,
            file: 2,
        }
    }
}

/// Bytes a write brought with it, waiting for the step that puts them in the
/// buffer.
///
/// Inbound counterpart of the outbound payload stash, and kept outside the
/// machine for the same reason: payload is not a decision. One queue per object
/// rather than one slot, because several writes may be waiting their turn — and
/// the machine's FIFO guarantees they are consumed in the order they arrived.
type Inbox = Arc<Mutex<std::collections::HashMap<ObjectId, std::collections::VecDeque<Vec<u8>>>>>;

/// Drop the write permission from a Nextcloud permission string.
///
/// Empty means "the server said nothing", which [`crate::model::is_writable`]
/// reads as writable — so it cannot stay empty here, and gets the read letter
/// instead. Removing `W` is enough for a file; a directory would also need `C`
/// and `K`, but a directory has no outdated copy to be read-only about.
fn withdraw_write(permissions: &str) -> String {
    if permissions.is_empty() {
        return "R".to_string();
    }
    permissions.replace(['W', 'C', 'K'], "")
}

/// Whether the connection is metered, asked rarely.
///
/// The answer comes from the desktop backend, which asks NetworkManager over
/// D-Bus. That is far too expensive to do per read — and pointless, because it
/// changes when somebody walks out of the building, not between two 128 KiB
/// chunks. So it is cached, briefly: long enough that a file being read costs
/// one lookup, short enough that leaving the office is noticed while the user is
/// still opening things.
pub struct Metered {
    desktop: Arc<Mutex<Arc<dyn crate::desktop::Desktop>>>,
    last: Mutex<Option<(std::time::Instant, Option<bool>)>>,
    /// How long an answer is trusted. A field rather than a constant so a test
    /// can watch it expire without waiting out the real half-minute.
    ttl: std::time::Duration,
}

/// How long a metering answer is trusted: long enough that a file being read
/// costs one lookup, short enough that leaving the office is noticed while the
/// user is still opening things.
const METERED_TTL: std::time::Duration = std::time::Duration::from_secs(30);

impl Metered {
    /// Reads through the same shared slot the syncer uses, so installing the
    /// real desktop backend after start-up reaches this too.
    #[must_use]
    pub fn new(desktop: Arc<Mutex<Arc<dyn crate::desktop::Desktop>>>) -> Self {
        Self::with_ttl(desktop, METERED_TTL)
    }

    /// The same, with an explicit lifetime for the cached answer — for tests
    /// that need the cache to expire on demand.
    #[must_use]
    pub fn with_ttl(
        desktop: Arc<Mutex<Arc<dyn crate::desktop::Desktop>>>,
        ttl: std::time::Duration,
    ) -> Self {
        Self {
            desktop,
            last: Mutex::new(None),
            ttl,
        }
    }

    /// `None` means "cannot tell", which callers must not read as "free".
    #[must_use]
    pub fn get(&self) -> Option<bool> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, value)) = *last {
            if at.elapsed() < self.ttl {
                return value;
            }
        }
        let desktop = self
            .desktop
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let value = desktop.is_metered();
        *last = Some((std::time::Instant::now(), value));
        value
    }
}

/// A running substrate. Dropping it stops every thread.
pub struct Substrate {
    to_fsm: Sender<Event>,
    inbox: Inbox,
    threads: Vec<JoinHandle<()>>,
    /// Kept only so a diagnostics snapshot can report them.
    pools: PoolSizes,
    /// Dropped first on teardown, to wake the uploader out of its wait so the
    /// join below does not block for a whole retry interval.
    uploader_shutdown: Option<Sender<()>>,
}

impl Substrate {
    /// Start the deciding thread and the pools.
    ///
    /// # Errors
    /// If a worker cannot open its own connection to the state database.
    pub fn start(ctx: &Context, pools: Pools) -> crate::Result<(Self, Receiver<Answered>)> {
        let (to_fsm, from_all) = channel::<Event>();
        let (answers_tx, answers_rx) = channel::<Answered>();

        let (db_read_tx, db_read_rx) = channel::<Dispatched>();
        let (db_write_tx, db_write_rx) = channel::<Dispatched>();
        let (net_tx, net_rx) = channel::<Dispatched>();
        let (file_tx, file_rx) = channel::<Dispatched>();

        let inbox: Inbox = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let mut threads = Vec::new();
        // Several threads on one queue: a receiver behind a mutex *is* a work
        // queue, and the lock is held only long enough to take the next item.
        let db_read_rx = Arc::new(Mutex::new(db_read_rx));
        let net_rx = Arc::new(Mutex::new(net_rx));
        let file_rx = Arc::new(Mutex::new(file_rx));

        for (name, count, rx) in [
            ("wusel-db-read", pools.db_readers, &db_read_rx),
            ("wusel-net", pools.net, &net_rx),
            ("wusel-file", pools.file, &file_rx),
        ] {
            for n in 0..count.max(1) {
                let worker = Worker::open(ctx, Arc::clone(&inbox))?;
                let rx = Arc::clone(rx);
                let back = to_fsm.clone();
                threads.push(spawn_worker(
                    format!("{name}-{n}"),
                    back,
                    worker,
                    move || rx.lock().unwrap_or_else(|e| e.into_inner()).recv().ok(),
                ));
            }
        }

        // The writer is alone by design, so it takes its queue directly.
        let worker = Worker::open(ctx, Arc::clone(&inbox))?;
        let back = to_fsm.clone();
        threads.push(spawn_worker(
            "wusel-db-write".into(),
            back,
            worker,
            move || db_write_rx.recv().ok(),
        ));

        threads.push(spawn_decider(
            to_fsm.clone(),
            from_all,
            Queues {
                db_read: db_read_tx,
                db_write: db_write_tx,
                net: net_tx,
                file: file_tx,
            },
            answers_tx,
            ctx.async_upload,
        ));

        // The asynchronous uploader: resume anything owed at start-up, and retry
        // transient failures until they land. It only nudges the decider (the
        // durable record is the source of truth), so it needs the sender and the
        // database path, nothing more. Synchronous write-back has no owed uploads
        // to chase — `flush` waited for each — so it runs no uploader.
        let uploader_shutdown = if ctx.async_upload {
            let (uploader_shutdown, up_rx) = channel::<()>();
            let up_to_fsm = to_fsm.clone();
            let up_db = ctx.db_path.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("wusel-uploader".into())
                    .spawn(move || uploader_loop(&up_to_fsm, &up_db, &up_rx))
                    .expect("spawn the uploader thread"),
            );
            Some(uploader_shutdown)
        } else {
            None
        };

        Ok((
            Self {
                to_fsm,
                inbox,
                threads,
                pools: PoolSizes {
                    db_readers: pools.db_readers.max(1),
                    net: pools.net.max(1),
                    file: pools.file.max(1),
                },
                uploader_shutdown,
            },
            answers_rx,
        ))
    }

    /// A read-only snapshot of what the substrate is doing. See
    /// [`DiagHandle::snapshot`].
    ///
    /// # Errors
    /// If the substrate has stopped, or the decider does not answer in time.
    pub fn snapshot(&self) -> crate::Result<SubstrateSnapshot> {
        self.diag_handle().snapshot()
    }

    /// A cloneable handle for taking snapshots without holding the whole
    /// substrate. The mount moves the substrate into the FUSE session, so the
    /// diagnostics socket — on another thread — holds one of these instead.
    #[must_use]
    pub fn diag_handle(&self) -> DiagHandle {
        DiagHandle {
            to_fsm: self.to_fsm.clone(),
            pools: self.pools,
        }
    }

    /// Hand a request that carries bytes — a write — to the deciding thread.
    ///
    /// The bytes are parked before the request is sent, so the step that wants
    /// them cannot possibly run first.
    ///
    /// # Errors
    /// If the substrate has stopped.
    pub fn submit_write(&self, request: Request, data: Vec<u8>) -> crate::Result<()> {
        self.inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(request.object)
            .or_default()
            .push_back(data);
        self.submit(request)
    }

    /// Tell the machine that nobody is waiting for this request any more.
    ///
    /// The transfer it was riding on gives up at its next step boundary, and
    /// over a metered or throttled link that is the user's bandwidth — and
    /// possibly their money — not merely a tidy-up.
    ///
    /// # Errors
    /// If the substrate has stopped.
    pub fn abandon(&self, request: RequestId) -> crate::Result<()> {
        self.to_fsm
            .send(Event::Abandon(request))
            .map_err(|_| crate::Error::Other("the decider is gone".into()))
    }

    /// Hand a request to the deciding thread.
    ///
    /// # Errors
    /// If the substrate has stopped.
    pub fn submit(&self, request: Request) -> crate::Result<()> {
        self.to_fsm
            .send(Event::Request(request))
            .map_err(|_| crate::Error::Other("the decider is gone".into()))
    }
}

impl Drop for Substrate {
    fn drop(&mut self) {
        // Wake the uploader first — dropping the sender disconnects its wait — so
        // the join below is prompt rather than blocking for a whole retry
        // interval.
        self.uploader_shutdown.take();
        let _ = self.to_fsm.send(Event::Stop);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// The asynchronous uploader.
///
/// It never uploads anything itself — the durable `pending_uploads` records are
/// the source of truth, and the decider owns the buffers — so all it does is
/// nudge the decider to (re)publish anything still owed: once at start-up, to
/// resume what a crash or a shutdown left behind, and then on a growing interval
/// to retry transient failures until they land.
///
/// The interval doubles (to a cap) while work remains and resets once nothing is
/// owed, so a server that keeps refusing is not hammered while a passing blip is
/// retried soon.
fn uploader_loop(to_fsm: &Sender<Event>, db_path: &std::path::Path, shutdown: &Receiver<()>) {
    use std::sync::mpsc::RecvTimeoutError;

    let base = std::env::var("WUSEL_UPLOAD_RETRY_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(3);
    let cap = base.saturating_mul(20).max(30);
    let mut interval = base;

    loop {
        // A fresh connection per pass, like the syncer's: cheap, and it never
        // holds the database while it waits.
        let owed = match StateDb::open_existing(db_path).and_then(|db| db.pending_uploads()) {
            Ok(pending) => {
                let mut owed = 0usize;
                for p in pending {
                    // `error` records are parked for the user; only `pending`
                    // (and in-flight, which the decider dedups) are retried.
                    if matches!(p.state, crate::state::UploadState::Pending) {
                        owed += 1;
                        if to_fsm
                            .send(Event::ResumeUpload {
                                object: p.object,
                                base_etag: p.base_etag,
                                mtime: p.mtime,
                            })
                            .is_err()
                        {
                            return; // the decider is gone
                        }
                    }
                }
                owed
            }
            Err(e) => {
                tracing::debug!(%e, "uploader: could not read pending uploads");
                0
            }
        };

        interval = if owed == 0 {
            base
        } else {
            interval.saturating_mul(2).min(cap)
        };
        match shutdown.recv_timeout(std::time::Duration::from_secs(interval)) {
            Err(RecvTimeoutError::Timeout) => {}
            // Signalled, or the substrate dropped its end: stop.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Where the decider sends work.
struct Queues {
    db_read: Sender<Dispatched>,
    db_write: Sender<Dispatched>,
    net: Sender<Dispatched>,
    file: Sender<Dispatched>,
}

impl Queues {
    /// Hand a job to the pool its executor names.
    fn send(
        &self,
        object: ObjectId,
        job: Job,
        deliver: Deliver,
    ) -> std::result::Result<(), std::sync::mpsc::SendError<Dispatched>> {
        let to = match job.executor() {
            Executor::DbRead => &self.db_read,
            Executor::DbWrite => &self.db_write,
            Executor::Net => &self.net,
            Executor::FileIo => &self.file,
        };
        to.send(Dispatched {
            object,
            job,
            deliver,
        })
    }
}

/// Where a finished job's result goes.
enum Deliver {
    /// Back to the machine as the object's running flow (the common case).
    Flow,
    /// Nowhere — a background refresh nobody waits for (see [`Action::Refresh`]).
    Detached,
    /// Straight to this one request, beside the object's flow (see
    /// [`Action::ReadBeside`]).
    Beside(RequestId),
}

/// The deciding thread: requests and completions in, actions out.
///
/// It never blocks on anything but its own channel — the one property the whole
/// design is built to preserve.
fn spawn_decider(
    self_tx: Sender<Event>,
    events: Receiver<Event>,
    queues: Queues,
    answers: Sender<Answered>,
    async_upload: bool,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("wusel-fsm".into())
        .spawn(move || {
            let mut machine = Machine::new();
            machine.set_async_upload(async_upload);
            // Bytes a step produced, waiting for the flow to end so they can go
            // out with the answer. One flow per object is the machine's own
            // invariant, so the object is a sufficient key.
            let mut payloads: std::collections::HashMap<ObjectId, Payload> =
                std::collections::HashMap::new();
            // Objects with a background refresh in flight, so a second one is
            // not started while the first is still running.
            let mut refreshing: std::collections::HashSet<ObjectId> =
                std::collections::HashSet::new();

            while let Ok(event) = events.recv() {
                // The object a completion belongs to, so its leftover payload can
                // be dropped if the flow ended without answering anyone — a
                // detached flow, such as an asynchronous upload, does exactly
                // that, and its payload would otherwise linger.
                let mut completed: Option<ObjectId> = None;
                let actions = match event {
                    Event::Request(r) => machine.on_request(r),
                    // The caller is gone, but its request is still owed an
                    // answer — the machine returns it, so the parked reply is
                    // completed and the kernel lets go of the page it locked.
                    Event::Abandon(id) => machine.abandon(id),
                    Event::Completed {
                        object,
                        completion,
                        payload,
                    } => {
                        // The last step to produce something is the one whose
                        // result the caller wants: a read's bytes supersede the
                        // row that was fetched to find them.
                        if !matches!(payload, Payload::None) {
                            payloads.insert(object, payload);
                        }
                        completed = Some(object);
                        machine.on_completion(object, completion)
                    }
                    Event::Refreshed { object } => {
                        refreshing.remove(&object);
                        Vec::new()
                    }
                    Event::CompletedBeside {
                        request,
                        completion,
                        payload,
                    } => {
                        // A beside read: answer its one caller directly with the
                        // payload it produced. It never touched occupancy or the
                        // object-keyed payload map, so nothing here does either.
                        let outcome = machine.on_beside(request, completion);
                        let _ = answers.send(Answered {
                            requests: vec![request],
                            outcome,
                            payload,
                        });
                        Vec::new()
                    }
                    Event::ResumeUpload {
                        object,
                        base_etag,
                        mtime,
                    } => {
                        // Skip if something is already running for the object —
                        // the initial upload, or an earlier resume — so a retry
                        // never doubles up.
                        if machine.is_busy(object) {
                            Vec::new()
                        } else {
                            // After a restart the registry is empty; re-open the
                            // buffer from the durable record so the publish finds
                            // it. Within a session the buffer is still open, so
                            // leave it as it is.
                            if !machine.registry().facts(object).buffer_open {
                                machine.registry_mut().open(
                                    object,
                                    Buffer {
                                        dirty: true,
                                        base_etag,
                                        pending_mtime: mtime,
                                        ignored: false,
                                    },
                                );
                            }
                            machine.on_request(Request {
                                id: RequestId(0),
                                object,
                                intent: wusel_fsm::Intent::Publish,
                            })
                        }
                    }
                    Event::Diag(reply) => {
                        // The decider owns the machine, so this is the one place
                        // its state can be read without a lock. A dropped
                        // receiver (the asker gave up) is harmless.
                        let _ = reply.send(DeciderDiag {
                            machine: machine.snapshot(),
                            refreshing: refreshing.len(),
                        });
                        Vec::new()
                    }
                    Event::Stop => return,
                };

                for action in actions {
                    match action {
                        Action::Dispatch { object, job } => {
                            if queues.send(object, job, Deliver::Flow).is_err() {
                                return; // the pool is gone; so are we
                            }
                        }
                        Action::ReadBeside {
                            object,
                            job,
                            request,
                        } => {
                            // Beside the busy flow, answered on its own: a
                            // metadata read that must not wait on an upload.
                            if queues.send(object, job, Deliver::Beside(request)).is_err() {
                                return;
                            }
                        }
                        Action::Refresh { object } => {
                            // Beside the machine, never in front of a caller.
                            // Two guards, both cheap and both necessary: one
                            // refresh per object at a time, and none at all
                            // while that object has real work running — a
                            // reconcile beside a rename or a delete could
                            // resurrect the rows it just removed.
                            if refreshing.contains(&object) || machine.is_busy(object) {
                                continue;
                            }
                            refreshing.insert(object);
                            if queues
                                .send(object, Job::ListRemote { object }, Deliver::Detached)
                                .is_err()
                            {
                                return;
                            }
                        }
                        Action::Schedule { object, intent } => {
                            // Nobody waits for it, so it gets a ticket nobody
                            // holds: the answer is dropped, the work is not.
                            let _ = self_tx.send(Event::Request(Request {
                                id: RequestId(0),
                                object,
                                intent,
                            }));
                        }
                        Action::Answer {
                            object,
                            requests,
                            outcome,
                        } => {
                            let _ = answers.send(Answered {
                                requests,
                                outcome,
                                payload: payloads.remove(&object).unwrap_or_default(),
                            });
                        }
                    }
                }

                // A flow that has ended and is no longer busy owes no more
                // answers. If it left a payload behind — a detached background
                // upload never answers anyone — drop it, so the map does not
                // grow one stale entry per asynchronous upload.
                if let Some(object) = completed {
                    if !machine.is_busy(object) {
                        payloads.remove(&object);
                    }
                }
            }
        })
        .expect("spawn the decider thread")
}

/// A job on its way to a pool, and whether its answer is owed to anyone.
///
/// A background refresh runs beside the machine, not through it (see
/// [`wusel_fsm::Action::Refresh`]), so its completion must not be fed back —
/// there is no flow it belongs to, and the machine would reject it.
struct Dispatched {
    object: ObjectId,
    job: Job,
    deliver: Deliver,
}

/// One worker: take a job, run it, report the completion.
fn spawn_worker<F>(
    name: String,
    back: Sender<Event>,
    mut worker: Worker,
    mut next: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Option<Dispatched> + Send + 'static,
{
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            while let Some(d) = next() {
                let (completion, payload) = worker.run(&d.job);
                let event = match d.deliver {
                    Deliver::Detached => {
                        // The work was the point; the answer is nobody's. Report
                        // a failure, though — a refresh that keeps failing
                        // silently is a directory that quietly stops updating.
                        if let Completion::Failed(f) = completion {
                            tracing::debug!(object = d.object.0, ?f, "background refresh failed");
                        }
                        Event::Refreshed { object: d.object }
                    }
                    Deliver::Beside(request) => Event::CompletedBeside {
                        request,
                        completion,
                        payload,
                    },
                    Deliver::Flow => Event::Completed {
                        object: d.object,
                        completion,
                        payload,
                    },
                };
                if back.send(event).is_err() {
                    return;
                }
            }
        })
        .expect("spawn a worker thread")
}

/// A step that could not be carried out. Storage or transport failed; the
/// script ends the flow and everyone waiting is told.
///
/// And the log is told too, which it was not for a long time. Every arm here
/// used to read `Err(_) => failed()`, so a failure became an errno for the
/// kernel and nothing else — no line, no cause, no job name. Two defects found
/// on a real desktop, an EIO on every save and a listing that stalled for
/// seconds, both left the journal completely silent and had to be reproduced in
/// a test before they could even be seen.
///
/// `debug`, not `warn`: some of these are ordinary. A `lookup` of a name that is
/// not there, a read of a file somebody just deleted — the script has a branch
/// for each, and warning about them would train people to ignore the warnings.
/// What matters is that the cause is written down *somewhere* at all.
fn failed(job: &Job, error: &dyn std::fmt::Display) -> (Completion, Payload) {
    failed_at(job_name(job), error)
}

/// The same, where the job is known by name rather than by value — the helper
/// methods a job delegates to.
fn failed_at(what: &'static str, error: &dyn std::fmt::Display) -> (Completion, Payload) {
    // `to_string` allocates, on a path that is already an error. Cheap where it
    // is paid and free everywhere else.
    tracing::debug!(job = what, error = %error.to_string(), "step failed");
    (Completion::Failed(Failure::Io), Payload::None)
}

/// The job's name for a log line — the variant, without its fields.
///
/// A hand-written match rather than `Debug`: the fields can hold a whole write
/// buffer's worth of parameters, and a log line is for reading.
fn job_name(job: &Job) -> &'static str {
    match job {
        Job::ReadNode { .. } => "read-node",
        Job::ReadChild { .. } => "read-child",
        Job::ReadChildren { .. } => "read-children",
        Job::ReadState { .. } => "read-state",
        Job::RecordVersion { .. } => "record-version",
        Job::RecordMtime { .. } => "record-mtime",
        Job::InsertNode { .. } => "insert-node",
        Job::RemoveRows { .. } => "remove-rows",
        Job::MoveRows { .. } => "move-rows",
        Job::FetchRange { .. } => "fetch-range",
        Job::ListRemote { .. } => "list-remote",
        Job::Upload { .. } => "upload",
        Job::ResolveConflict { .. } => "resolve-conflict",
        Job::CreateRemoteDir { .. } => "create-remote-dir",
        Job::DeleteRemote { .. } => "delete-remote",
        Job::MoveRemote { .. } => "move-remote",
        Job::HydrateBuffer { .. } => "hydrate-buffer",
        Job::HydrateCache { .. } => "hydrate-cache",
        Job::ReadBuffer { .. } => "read-buffer",
        Job::ReadBlob { .. } => "read-blob",
        Job::BufferSize { .. } => "buffer-size",
        Job::StoreBlob { .. } => "store-blob",
        Job::DiscardBuffer { .. } => "discard-buffer",
        Job::CreateBuffer { .. } => "create-buffer",
        Job::WriteBuffer { .. } => "write-buffer",
        Job::TruncateBuffer { .. } => "truncate-buffer",
        Job::MarkPending { .. } => "mark-pending",
        Job::ClearPending { .. } => "clear-pending",
        Job::SetUploadError { .. } => "set-upload-error",
    }
}

/// What a worker can do.
///
/// Every pool gets the same capabilities; which queue a worker takes from is a
/// policy about how its jobs block, not about what it may touch.
struct Worker {
    db: StateDb,
    content: Arc<dyn ContentSource>,
    scratch_dir: PathBuf,
    pins: Arc<crate::pins::Pins>,
    /// What to serve when an outdated pinned file is opened.
    open_pinned: crate::config::OpenPinned,
    /// The connection's cost, asked rarely and shared between workers.
    metered: Arc<Metered>,
    inbox: Inbox,
    write: Option<WriteContext>,
    ignore_patterns: Vec<String>,
    revalidate_secs: u64,
    push_floor_secs: u64,
    invalidate_after: Arc<std::sync::atomic::AtomicI64>,
}

impl Worker {
    fn open(ctx: &Context, inbox: Inbox) -> crate::Result<Self> {
        Ok(Self {
            // Attach, do not initialise: starting a worker must not wait for
            // whoever happens to hold the write lock.
            db: StateDb::open_existing(&ctx.db_path)?,
            content: Arc::clone(&ctx.content),
            scratch_dir: ctx.scratch_dir.clone(),
            pins: Arc::clone(&ctx.pins),
            open_pinned: ctx.open_pinned,
            metered: Arc::clone(&ctx.metered),
            inbox,
            write: ctx.write.clone(),
            ignore_patterns: ctx.ignore_patterns.clone(),
            revalidate_secs: ctx.revalidate_secs,
            push_floor_secs: ctx.push_floor_secs,
            invalidate_after: Arc::clone(&ctx.invalidate_after),
        })
    }

    fn buffer_path(&self, object: ObjectId) -> PathBuf {
        self.scratch_dir.join(object.0.to_string())
    }

    /// Would placing `name` under `parent` land inside a freedesktop trash
    /// directory? Catches a pre-existing `.Trash-<uid>` (synced from the server
    /// or another client) that the mount-root guard in the frontend cannot see.
    /// Uses the parent's path — the same lookup the insert would do anyway.
    fn child_is_trash(&self, parent: ObjectId, name: &str) -> bool {
        match self.db.node_by_inode(parent.0) {
            Ok(Some(node)) => {
                crate::provider::is_trash_path(&crate::provider::child_path(&node.path, name))
            }
            _ => false,
        }
    }

    /// Commit a change locally so `flush` can be answered before the upload
    /// runs: capture the upload target now, make the buffer durable, and record
    /// the object as owed to the server. The record is what a background or
    /// resumed upload works from, and what stops a crash between close and
    /// upload from losing the change.
    fn mark_pending(
        &self,
        object: ObjectId,
        base_etag: &str,
        mtime: Option<i64>,
    ) -> crate::Result<()> {
        // The upload target, resolved now and stored — not re-walked later, when
        // a rename may have moved the object.
        let remote_path = self
            .db
            .node_by_inode(object.0)?
            .map(|n| n.path)
            .unwrap_or_default();
        // The bytes must be on disk before "saved" is true: the pending record
        // points at this buffer, and a crash must not leave it pointing at a
        // half-written file.
        if let Ok(f) = std::fs::File::open(self.buffer_path(object)) {
            let _ = f.sync_all();
        }
        // Reflect the committed length on the row now. The upload is
        // asynchronous, so `getattr` runs against this row long before the
        // bytes reach the server; without this it would keep reporting the
        // pre-write size — zero for a freshly created file — so a file just
        // copied in shows as 0 bytes until (and unless) the upload lands. The
        // size the server will confirm is exactly the buffer's current length.
        if let Ok(meta) = std::fs::metadata(self.buffer_path(object)) {
            let _ = self.db.set_size(object.0, meta.len());
        }
        self.db
            .mark_pending_upload(object, &remote_path, base_etag, mtime)
    }

    /// Carry out one job.
    ///
    /// No catch-all arm, deliberately: a job added later must be placed here by
    /// hand, and the compiler names this spot. A silently mishandled step would
    /// surface as a request that never returns.
    fn run(&mut self, job: &Job) -> (Completion, Payload) {
        match job {
            Job::ReadNode { object } => self.read_node(*object),
            Job::ReadChild { parent, name } => match self.db.child_by_name(parent.0, name) {
                Ok(Some(node)) => self.present(node),
                Ok(None) => (Completion::Node(NodeFacts::default()), Payload::None),
                Err(e) => failed(job, &e),
            },
            Job::ReadState { object, buffered } => match self.read_state(*object, *buffered) {
                Ok(state) => (Completion::StateKnown, Payload::State(state)),
                Err(e) => failed(job, &e),
            },
            Job::TruncateBuffer { object, size } => match self.truncate_buffer(*object, *size) {
                Ok(()) => (Completion::Done, Payload::None),
                Err(e) => failed(job, &e),
            },
            Job::RecordMtime { object, mtime } => match self.db.set_mtime(object.0, *mtime) {
                Ok(()) => (Completion::Done, Payload::None),
                Err(e) => failed(job, &e),
            },
            Job::ReadChildren { object } => match self.db.children_of(object.0) {
                Ok(rows) => (Completion::Listed, Payload::Entries(rows)),
                Err(e) => failed(job, &e),
            },
            Job::RecordVersion { object, etag, size } => {
                match self.db.set_etag_size(object.0, etag, *size) {
                    Ok(()) => (Completion::Done, Payload::None),
                    Err(e) => failed(job, &e),
                }
            }
            Job::MarkPending {
                object,
                base_etag,
                mtime,
            } => match self.mark_pending(*object, base_etag, *mtime) {
                Ok(()) => (Completion::Done, Payload::None),
                Err(e) => failed(job, &e),
            },
            Job::ClearPending { object } => match self.db.clear_pending_upload(*object) {
                Ok(()) => (Completion::Done, Payload::None),
                Err(e) => failed(job, &e),
            },
            Job::SetUploadError { object, message } => {
                match self
                    .db
                    .set_upload_state(*object, crate::state::UploadState::Error, Some(message))
                {
                    Ok(()) => (Completion::Done, Payload::None),
                    Err(e) => failed(job, &e),
                }
            }
            Job::InsertNode { parent, name, dir } => {
                if *dir {
                    // A directory has to exist on the server before anything can
                    // be put in it, so it is never merely local.
                    failed_at(
                        "insert-node",
                        &"a directory cannot be inserted locally; it is created on the server first",
                    )
                } else if self.child_is_trash(*parent, name) {
                    (Completion::Failed(Failure::NotWritable), Payload::None)
                } else {
                    match self.db.insert_local_file(parent.0, name) {
                        Ok(_) => (Completion::Done, Payload::None),
                        Err(e) => failed(job, &e),
                    }
                }
            }
            Job::RemoveRows { object } => match self.remove_rows(*object) {
                Ok(()) => (Completion::Done, Payload::None),
                Err(e) => failed(job, &e),
            },
            Job::MoveRows {
                object,
                to_parent,
                to_name,
            } if self.child_is_trash(*to_parent, to_name) => {
                let _ = object;
                (Completion::Failed(Failure::NotWritable), Payload::None)
            }
            Job::MoveRows {
                object,
                to_parent,
                to_name,
            } => match self.db.move_subtree(object.0, to_parent.0, to_name) {
                Ok((from, to)) => {
                    // The rows moved; the promise has to follow them. Not
                    // inside the transaction, because the pins are not in this
                    // database any more — so a failure here leaves the rename
                    // standing and says so, loudly. A pin that ends silently is
                    // the outcome worth this much noise.
                    if let Err(e) = self.pins.rename(&from, &to) {
                        tracing::error!(%e, %from, %to,
                            "renamed, but the pin did not follow — re-pin the new path");
                    }
                    (Completion::Done, Payload::None)
                }
                Err(e) => failed(job, &e),
            },
            Job::ReadBuffer {
                object,
                offset,
                len,
            } => match read_range_from_scratch(&self.buffer_path(*object), *offset, *len) {
                Ok(bytes) => (Completion::Bytes, Payload::Bytes(bytes)),
                Err(e) => failed(job, &e),
            },
            Job::CreateBuffer { object } => match self.create_buffer(*object) {
                Ok(()) => (Completion::Done, Payload::None),
                Err(e) => failed(job, &e),
            },
            Job::WriteBuffer {
                object,
                offset,
                len,
            } => {
                match self.write_buffer(*object, *offset) {
                    // The caller is owed a count, and a short write would be a
                    // lie: the buffer took all of it or the step failed.
                    Ok(()) => (Completion::Done, Payload::Written(*len)),
                    Err(e) => failed(job, &e),
                }
            }
            Job::DiscardBuffer { object } => {
                // A buffer that is already gone is the outcome we wanted, not an
                // error — every path here is "make sure it is not there".
                let _ = std::fs::remove_file(self.buffer_path(*object));
                (Completion::Done, Payload::None)
            }
            Job::BufferSize { object } => match std::fs::metadata(self.buffer_path(*object)) {
                Ok(m) => (Completion::Size(m.len()), Payload::None),
                Err(e) => failed(job, &e),
            },
            Job::Upload {
                object,
                size,
                precondition,
                mtime,
            } => self.upload(*object, *size, precondition, *mtime),
            Job::ResolveConflict { object } => match self.resolve_conflict(*object) {
                Ok(()) => (Completion::Done, Payload::None),
                Err(e) => failed(job, &e),
            },
            Job::HydrateBuffer { object } => match self.hydrate_buffer(*object) {
                Ok(()) => (Completion::Done, Payload::None),
                Err(e) => failed(job, &e),
            },
            Job::StoreBlob { object } => match self.store_blob(*object) {
                Ok(()) => (Completion::Done, Payload::None),
                Err(e) => failed(job, &e),
            },
            // The machine has already decided *why* these bytes are wanted —
            // cached or live — and the source carries it out. The two jobs
            // differ in which pool they run on, which is the point: a range GET
            // must not occupy a slot meant for local reads.
            Job::ReadBlob {
                object,
                offset,
                len,
            }
            | Job::FetchRange {
                object,
                offset,
                len,
            } => self.fetch(*object, *offset, *len),
            // Steps that later phases wire. Reported as a failure rather than
            // silently succeeding.
            Job::ListRemote { object } => match self.list_remote(*object) {
                Ok(()) => (Completion::Listed, Payload::None),
                Err(e) => failed(job, &e),
            },
            Job::HydrateCache { object } => match self.hydrate_cache(*object) {
                Ok(()) => (Completion::Done, Payload::None),
                Err(e) => failed(job, &e),
            },
            Job::CreateRemoteDir { parent, name } if self.child_is_trash(*parent, name) => {
                (Completion::Failed(Failure::NotWritable), Payload::None)
            }
            Job::CreateRemoteDir { parent, name } => match self.create_remote_dir(*parent, name) {
                Ok(()) => (Completion::Done, Payload::None),
                Err(e) => failed(job, &e),
            },
            Job::DeleteRemote { object } => match self.delete_remote(*object) {
                Ok(()) => (Completion::Done, Payload::None),
                Err(e) => failed(job, &e),
            },
            Job::MoveRemote {
                object,
                to_parent,
                to_name,
            } if self.child_is_trash(*to_parent, to_name) => {
                let _ = object;
                (Completion::Failed(Failure::NotWritable), Payload::None)
            }
            Job::MoveRemote {
                object,
                to_parent,
                to_name,
            } => match self.move_remote(*object, *to_parent, to_name) {
                Ok(()) => (Completion::Done, Payload::None),
                Err(e) => failed(job, &e),
            },
        }
    }

    /// One read answers everything the next decision turns on — including
    /// whether the cached copy is still current, which means stating a file.
    /// Doing that here, on a worker, is precisely what keeps the three-way
    /// choice in the machine a decision rather than an I/O step.
    fn read_node(&mut self, object: ObjectId) -> (Completion, Payload) {
        match self.db.node_by_inode(object.0) {
            Ok(Some(node)) => self.present(node),
            Ok(None) => (Completion::Node(NodeFacts::default()), Payload::None),
            Err(e) => failed_at("read-node", &e),
        }
    }

    /// Everything the next decision turns on, worked out here so the decision
    /// itself costs nothing.
    /// Whether an outdated local copy may answer this read.
    ///
    /// Ordered by cost, cheapest first, because this runs for every `getattr`
    /// and every open: the configured policy (a comparison), the pin (a cached
    /// file), the blob's freshness (two small reads), and only then the
    /// connection's cost, which is cached with a short life of its own.
    ///
    /// Pinned files only. An unpinned cached file promises nothing — "the next
    /// read goes live" is ordinary VFS behaviour, and serving something outdated
    /// for it would be a bug rather than a policy.
    fn stale_copy_ok(&self, node: &NodeRow) -> bool {
        if self.open_pinned == crate::config::OpenPinned::Newest {
            return false;
        }
        if !self.pins.is_pinned(&node.path).unwrap_or(false) {
            return false;
        }
        if !self.content.is_stale(node) {
            return false; // current, or nothing on disk to serve
        }
        self.open_pinned.decide(self.metered.get()) == crate::config::OpenAction::ServeLocal
    }

    /// Hand a row to the machine and to the frontend, as one answer.
    ///
    /// The single place where "what we will actually serve" is turned into what
    /// everyone else sees — which is why the read-only rule lives here rather
    /// than in two.
    ///
    /// **An outdated offline copy is read-only, whatever made it outdated.** The
    /// permission is withdrawn on the row itself, so the machine refuses a write
    /// with EACCES *and* the frontend reports a mode without the write bits —
    /// which is the part that matters, because it lets an editor open the file
    /// read-only instead of letting somebody type for ten minutes and fail at
    /// save time.
    ///
    /// Editing it would be worse than it looks. The write buffer is seeded from
    /// the *server's* current version and records its ETag as the base, so the
    /// upload would assert it started from the version it never saw and replace
    /// it — silently, with no conflict raised. Refusing the write is not
    /// caution, it is the only honest answer until an edit can be based on the
    /// copy the user actually read.
    fn present(&self, mut node: NodeRow) -> (Completion, Payload) {
        if self.stale_copy_ok(&node) {
            node.permissions = withdraw_write(&node.permissions);
        }
        let facts = self.facts_for(&node);
        (Completion::Node(facts), Payload::Node(Box::new(node)))
    }

    fn facts_for(&self, node: &NodeRow) -> NodeFacts {
        NodeFacts {
            id: ObjectId(node.inode),
            found: true,
            parent: ObjectId(node.parent),
            dir: node.is_dir,
            writable: node.is_writable(),
            etag: node.etag.clone(),
            size: node.size,
            blob_current: self.content.is_cached(node),
            stale_copy_ok: self.stale_copy_ok(node),
            materialised: node.file_id.is_some(),
            ignored: crate::ignore::is_ignored(&node.name, &self.ignore_patterns),
            children_loaded: self.db.children_loaded(node.inode).unwrap_or(false),
            listing_stale: self
                .db
                .dir_needs_reload(
                    node.inode,
                    self.revalidate_secs,
                    self.invalidate_after
                        .load(std::sync::atomic::Ordering::SeqCst),
                    self.push_floor_secs,
                )
                .unwrap_or(false),
        }
    }

    /// The object's emblem state.
    ///
    /// `buffered` comes from the machine and is not re-derived here: an unsaved
    /// edit is the most actionable thing there is, and it is knowledge only the
    /// machine holds.
    fn read_state(&mut self, object: ObjectId, buffered: bool) -> crate::Result<FileState> {
        // A committed change on its way to the server — or parked after a
        // permanent failure — is the state the user most needs to see, so it
        // trumps everything else, including the still-open buffer it rides on.
        if let Some(p) = self.db.pending_upload(object)? {
            return Ok(match p.state {
                crate::state::UploadState::Error => FileState::SyncError,
                crate::state::UploadState::Pending | crate::state::UploadState::Uploading => {
                    FileState::Uploading
                }
            });
        }
        if buffered {
            // An open buffer with no pending record yet: being edited, not yet
            // flushed.
            return Ok(FileState::Modified);
        }
        let Some(node) = self.db.node_by_inode(object.0)? else {
            return Err(crate::Error::NotFound);
        };
        if self.pins.is_pinned(&node.path)? {
            // Kept on purpose, but the server has moved on: say so now rather
            // than let the user find out when they are already offline.
            return Ok(if self.content.is_stale(&node) {
                FileState::PinnedStale
            } else {
                FileState::Pinned
            });
        }
        if node.is_dir {
            // A plain directory has no content of its own, so it carries no
            // content state and the file manager draws no emblem.
            return Err(crate::Error::NotFound);
        }
        if self.content.is_cached(&node) {
            Ok(FileState::Cached)
        } else {
            Ok(FileState::OnlineOnly)
        }
    }

    /// Drop an object's rows — and, first, the promises they carried.
    ///
    /// The pins and eviction markers go before the rows do, because both are
    /// keyed by path and file id: once the rows are gone there is nothing left
    /// to find them by, and the markers would keep those bytes exempt from the
    /// cache budget for ever.
    fn remove_rows(&mut self, object: ObjectId) -> crate::Result<()> {
        if let Some(node) = self.db.node_by_inode(object.0)? {
            crate::provider::run_unpin_subtree(
                &mut self.db,
                &self.pins,
                self.content.as_ref(),
                &node,
            )?;
        }
        self.db.remove_subtree(object.0)
    }

    /// Resize the write buffer.
    fn truncate_buffer(&self, object: ObjectId, size: u64) -> crate::Result<()> {
        std::fs::OpenOptions::new()
            .write(true)
            .open(self.buffer_path(object))?
            .set_len(size)?;
        Ok(())
    }

    /// A fresh, empty buffer.
    fn create_buffer(&self, object: ObjectId) -> crate::Result<()> {
        std::fs::create_dir_all(&self.scratch_dir)?;
        std::fs::write(self.buffer_path(object), [])?;
        Ok(())
    }

    /// Put the bytes this write brought into the buffer.
    ///
    /// Taking them from the inbox here — rather than routing them through the
    /// machine — is what lets the decider stay a decider. The queue is popped in
    /// arrival order, which is the same order the machine started the writes in.
    fn write_buffer(&self, object: ObjectId, offset: u64) -> crate::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let data = self
            .inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&object)
            .and_then(std::collections::VecDeque::pop_front)
            .ok_or_else(|| crate::Error::Other("a write arrived without its bytes".into()))?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(self.buffer_path(object))?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(&data)?;
        Ok(())
    }

    /// Seed a buffer with the object's current content.
    ///
    /// Streamed, never held whole in memory: editing a file of any size has to
    /// preserve all of it, and a partial copy would silently become the base a
    /// later three-way merge is made of.
    fn hydrate_buffer(&mut self, object: ObjectId) -> crate::Result<()> {
        let Some(node) = self.db.node_by_inode(object.0)? else {
            return Err(crate::Error::NotFound);
        };
        std::fs::create_dir_all(&self.scratch_dir)?;
        let mut out = std::fs::File::create(self.buffer_path(object))?;
        self.content.stream_to(&node, &mut out)
    }

    /// Copy the buffer into the read cache, so the next read stays local.
    ///
    /// The row is read again rather than carried: by now the version step has
    /// recorded the new ETag, so this picks up the version the bytes actually
    /// belong to instead of the one they started from.
    fn store_blob(&mut self, object: ObjectId) -> crate::Result<()> {
        let Some(node) = self.db.node_by_inode(object.0)? else {
            return Err(crate::Error::NotFound);
        };
        let etag = node.etag.clone();
        self.content
            .store_file(&node, &self.buffer_path(object), &etag)
    }

    /// Send the buffer to the server under the precondition the machine chose.
    ///
    /// A rejection is not an error: `Rejected` is a step outcome the script has
    /// a branch for, and turning it into a failure here would lose the user's
    /// bytes rather than resolve the conflict.
    fn upload(
        &mut self,
        object: ObjectId,
        size: u64,
        precondition: &wusel_fsm::Precondition,
        mtime: Option<i64>,
    ) -> (Completion, Payload) {
        let (Some(write), Ok(Some(node))) = (self.write.as_ref(), self.db.node_by_inode(object.0))
        else {
            return failed_at(
                "upload",
                &"no write context, or the row is gone — nothing to upload against",
            );
        };
        let pre = match precondition {
            wusel_fsm::Precondition::MustNotExist => crate::webdav::Precondition::MustNotExist,
            wusel_fsm::Precondition::Match(etag) => {
                crate::webdav::Precondition::Match(etag.clone())
            }
            wusel_fsm::Precondition::Unconditional => crate::webdav::Precondition::Unconditional,
        };
        // The desktop indicator belongs around the upload, not around the
        // flow: this is the one step that can stand for minutes, and it is what
        // a user watching the tray is actually waiting on.
        write.desktop.set_status(crate::desktop::Status::Syncing);
        let path = self.buffer_path(object);
        let sent = if size > crate::webdav::CHUNK_SIZE {
            write
                .rt
                .block_on(write.dav.put_chunked(&node.path, &path, size, &pre, mtime))
        } else {
            match std::fs::read(&path) {
                Ok(bytes) => write
                    .rt
                    .block_on(write.dav.put_conditional(&node.path, bytes, &pre, mtime)),
                Err(e) => Err(e.into()),
            }
        };
        match sent {
            Ok(crate::webdav::PutResult::Uploaded(etag)) => {
                write.desktop.set_status(crate::desktop::Status::Idle);
                (
                    Completion::Uploaded {
                        etag: etag.unwrap_or_default(),
                        size,
                    },
                    Payload::None,
                )
            }
            // Not an error: the sub-script resolves it, and it sets the
            // indicator back itself once the bytes are safe somewhere.
            Ok(crate::webdav::PutResult::Conflict) => (Completion::Rejected, Payload::None),
            Err(e) => {
                // The edit is only local, which is data at risk and worth
                // telling the user about — the buffer is kept, so the next
                // flush retries.
                write.desktop.set_status(crate::desktop::Status::Error);
                if e.is_permanent() {
                    // No retry will fix this — tell the user and park it. The
                    // buffer is kept, so the change is not lost and can be
                    // resolved.
                    write.desktop.notify(&crate::desktop::Notice::UploadFailed {
                        path: node.path.clone(),
                        reason: e.to_string(),
                    });
                    tracing::warn!(path = %node.path, error = %e, "upload failed permanently; parked");
                    (Completion::Failed(Failure::Permanent), Payload::None)
                } else {
                    // Transient (a 5xx, a timeout, a dropped connection): the
                    // uploader will retry. Do not notify on every attempt — that
                    // is noise — and leave the change queued.
                    tracing::debug!(path = %node.path, error = %e, "upload failed transiently; will retry");
                    failed_at("upload", &e)
                }
            }
        }
    }

    /// The 412 sub-script: merge if we can, otherwise park the bytes beside the
    /// server's version. One implementation, shared with the engine's own path.
    fn resolve_conflict(&mut self, object: ObjectId) -> crate::Result<()> {
        let Some(write) = self.write.clone() else {
            return Err(crate::Error::Other("no write context".into()));
        };
        // Whichever way it resolves, the bytes end up safe — so the indicator
        // goes back to Idle when it returns.
        let Some(node) = self.db.node_by_inode(object.0)? else {
            return Err(crate::Error::NotFound);
        };
        let path = self.buffer_path(object);
        let size = std::fs::metadata(&path)?.len();
        let outcome = run_conflict_resolution(&write, &mut self.db, &node, &path, size);
        write.desktop.set_status(if outcome.is_ok() {
            crate::desktop::Status::Idle
        } else {
            crate::desktop::Status::Error
        });
        outcome
    }

    /// The shareable half of the engine, or a clear error if this substrate was
    /// started without one (which the substrate-level tests do on purpose).
    fn write_ctx(&self) -> crate::Result<WriteContext> {
        self.write
            .clone()
            .ok_or_else(|| crate::Error::Other("this substrate has no network context".into()))
    }

    /// List a directory on the server and reconcile it into the state.
    fn list_remote(&mut self, object: ObjectId) -> crate::Result<()> {
        let ctx = self.write_ctx()?;
        run_reload_dir(&ctx, &mut self.db, object.0)
    }

    /// Fill the read cache with the object's current content.
    ///
    /// Streamed into a temporary file and then handed to the cache, rather than
    /// held in memory: a refresh must work for a file of any size, and the one
    /// thing it may not do is decide how large is too large.
    fn hydrate_cache(&mut self, object: ObjectId) -> crate::Result<()> {
        let Some(node) = self.db.node_by_inode(object.0)? else {
            return Err(crate::Error::NotFound);
        };
        std::fs::create_dir_all(&self.scratch_dir)?;
        let tmp = self.scratch_dir.join(format!("{}.refresh", object.0));
        let outcome = (|| -> crate::Result<()> {
            let mut out = std::fs::File::create(&tmp)?;
            self.content.stream_to(&node, &mut out)?;
            let etag = node.etag.clone();
            self.content.store_file(&node, &tmp, &etag)
        })();
        // The temporary copy is ours alone; it must not survive a failure.
        let _ = std::fs::remove_file(&tmp);
        outcome
    }

    /// Create a directory on the server.
    fn create_remote_dir(&mut self, parent: ObjectId, name: &str) -> crate::Result<()> {
        let ctx = self.write_ctx()?;
        let Some(pnode) = self.db.node_by_inode(parent.0)? else {
            return Err(crate::Error::NotFound);
        };
        let path = child_path(&pnode.path, name);
        ctx.rt.block_on(ctx.dav.mkcol(&path))?;
        ctx.write_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Delete an object on the server.
    fn delete_remote(&mut self, object: ObjectId) -> crate::Result<()> {
        let ctx = self.write_ctx()?;
        let Some(node) = self.db.node_by_inode(object.0)? else {
            return Err(crate::Error::NotFound);
        };
        ctx.rt.block_on(ctx.dav.delete(&node.path, node.is_dir))?;
        Ok(())
    }

    /// Move an object on the server, overwrite allowed.
    fn move_remote(
        &mut self,
        object: ObjectId,
        to_parent: ObjectId,
        to_name: &str,
    ) -> crate::Result<()> {
        let ctx = self.write_ctx()?;
        let (Some(node), Some(parent)) = (
            self.db.node_by_inode(object.0)?,
            self.db.node_by_inode(to_parent.0)?,
        ) else {
            return Err(crate::Error::NotFound);
        };
        let dst = child_path(&parent.path, to_name);
        ctx.rt
            .block_on(ctx.dav.move_(&node.path, &dst, node.is_dir))?;
        ctx.write_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Serve a range through the content source.
    ///
    /// The row is looked up here rather than carried along from the step that
    /// read it: one extra local `SELECT` against a transfer measured in
    /// milliseconds at best is noise, and it keeps engine data out of the
    /// deciding thread entirely.
    fn fetch(&mut self, object: ObjectId, offset: u64, len: u32) -> (Completion, Payload) {
        let Ok(Some(node)) = self.db.node_by_inode(object.0) else {
            return (Completion::Failed(Failure::Stale), Payload::None);
        };
        // The machine may have decided that an outdated local copy is the right
        // answer — a pinned file on a metered connection, or "give me the
        // offline version and I will refresh when I mean to". Asking for it has
        // to be explicit: `read` re-decides on freshness by itself and would go
        // live, which is exactly what the setting exists to avoid. If there is
        // nothing on disk after all, fall through and read live.
        if self.stale_copy_ok(&node) {
            if let Some(bytes) = self.content.read_outdated(&node, offset, len) {
                return (Completion::Bytes, Payload::Bytes(bytes));
            }
        }
        match self.content.read(&node, offset, len) {
            Ok(bytes) => (Completion::Bytes, Payload::Bytes(bytes)),
            // Gone on the server since we listed it: a stale handle, so the
            // reader stops rather than retrying into the same 404.
            Err(crate::Error::NotFound) => (Completion::Failed(Failure::Stale), Payload::None),
            Err(e) => failed_at("fetch-range", &e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{withdraw_write, Metered};
    use crate::desktop::Desktop;
    use crate::model::is_writable;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Withdrawing the write permission is what makes an outdated offline copy
    /// read-only, so it must actually leave the row non-writable — and the empty
    /// string is the trap: `is_writable` reads it as "the server said nothing",
    /// which means writable, so it cannot stay empty.
    #[test]
    fn withdrawing_write_leaves_the_row_read_only() {
        // A file, whatever letters it started with, comes out non-writable.
        assert!(!is_writable(&withdraw_write("RGDNVW"), false));
        assert!(!is_writable(&withdraw_write("RW"), false));
        // The empty string — "server said nothing" — must not survive as
        // writable; it becomes an explicit read letter.
        assert_eq!(withdraw_write(""), "R");
        assert!(!is_writable(&withdraw_write(""), false));
        // A directory's create/rename letters go too, so read-only means it for
        // a directory as well (even though we only apply this to files today).
        assert!(!is_writable(&withdraw_write("RGDNVCK"), true));
        // What was already read-only is left as it was.
        assert!(!is_writable(&withdraw_write("RGD"), false));
    }

    /// The metering answer is cached: it comes from a D-Bus round-trip on every
    /// call otherwise, and it changes when someone leaves the building, not
    /// between two reads of a file.
    #[test]
    fn metered_is_asked_once_and_then_cached() {
        struct Counting(Arc<AtomicUsize>, Option<bool>);
        impl Desktop for Counting {
            fn notify(&self, _n: &crate::desktop::Notice) {}
            fn set_status(&self, _s: crate::desktop::Status) {}
            fn is_metered(&self) -> Option<bool> {
                self.0.fetch_add(1, Ordering::SeqCst);
                self.1
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let backend: Arc<dyn Desktop> = Arc::new(Counting(Arc::clone(&calls), Some(true)));
        let cell = Arc::new(Mutex::new(backend));

        // A normal lifetime: repeated reads share one answer.
        let cached = Metered::new(Arc::clone(&cell));
        assert_eq!(cached.get(), Some(true));
        assert_eq!(cached.get(), Some(true));
        assert_eq!(cached.get(), Some(true));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "three reads of a file must not be three D-Bus round-trips"
        );

        // A zero lifetime: every read is past the window, so every read asks
        // again. This is the other half of the contract — the answer must not
        // be trusted for ever, or leaving the office would never be noticed.
        calls.store(0, Ordering::SeqCst);
        let expiring = Metered::with_ttl(cell, std::time::Duration::ZERO);
        // The values are asserted elsewhere; here only the *number of lookups*
        // matters, so the answers are deliberately dropped.
        assert_eq!(expiring.get(), Some(true));
        assert_eq!(expiring.get(), Some(true));
        assert_eq!(expiring.get(), Some(true));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "an expired answer must be re-asked, not served stale for ever"
        );
    }

    /// A structural assertion, in the spirit of the others in this workspace:
    /// the compiler cannot express "do not throw the error away", so the source
    /// is checked for the shape that does.
    ///
    /// It is worth a test because the shape is *comfortable*. `Err(_) =>
    /// failed()` compiles, reads fine, and silently costs the next person the
    /// only evidence they would have had. Two defects were found on a real
    /// desktop this way, both with an empty journal.
    #[test]
    fn no_step_throws_its_error_away() {
        let source = include_str!("runtime.rs");
        // Assembled rather than written out: a test that searches for a literal
        // it contains finds itself, every time.
        let needle = format!("Err(_) {} failed", "=>");
        let offenders: Vec<&str> = source
            .lines()
            .map(str::trim)
            // Comments may quote the pattern — the one above `failed` does, to
            // explain what it cost.
            .filter(|line| !line.starts_with("//"))
            .filter(|line| line.contains(&needle))
            .collect();
        assert!(
            offenders.is_empty(),
            "a failing step must say what failed — use `failed(job, &e)` or \
             `failed_at(name, &e)`: {offenders:?}"
        );
    }
}
