// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The synchronous submit-and-wait bridge over the engine's asynchronous
//! substrate.
//!
//! The substrate answers on a single [`Receiver<Answered>`]: every finished
//! request, from every caller, arrives on the one channel keyed by
//! [`RequestId`]. A socket frontend is the opposite shape — many connections,
//! each wanting *its* answer and nothing else, synchronously. The [`Driver`]
//! reconciles the two: one demultiplexer thread drains the receiver and routes
//! each answer to the caller that registered its id, so [`Driver::call`] can be
//! an ordinary blocking method that several connection threads share.
//!
//! This is the same submit → wait bridge the mock test harness builds
//! (`wusel-mock/tests/common/mod.rs`), lifted into library code and made safe
//! for concurrent callers.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use wusel_core::provider::Provider;
use wusel_core::runtime::{Payload, Pools, SubmitHandle, Substrate};
use wusel_core::state::ROOT_INODE;
use wusel_fsm::{Failure, Intent, ObjectId, Outcome, Request, RequestId};

/// The waiting side of one in-flight request: the sender the demultiplexer hands
/// the answer to.
type Waiter = Sender<(Outcome, Payload)>;

/// Owns the running engine and turns its asynchronous answer stream into
/// synchronous calls.
///
/// The [`Provider`] is kept alive alongside the [`Substrate`] because it holds
/// the shared state the substrate's workers read through (the same reason the
/// mock harness keeps it): dropping it would pull the ground out from under the
/// engine.
pub struct Driver {
    /// Whether this driver started the engine or merely rides one. Held, not
    /// read: it is what keeps an owned substrate alive, and its drop order is
    /// what shuts one down cleanly.
    _engine: Engine,
    /// How requests reach the substrate, and where their ids come from. The one
    /// allocator per substrate, so a co-hosted FUSE mount and this driver can
    /// never mint the same id.
    ids: SubmitHandle,
    /// The Provider, for the two operations that are not intents: `pin` and
    /// `unpin` are direct Provider methods, and `is_pinned`/`is_stale` are read
    /// on the status path. Shared rather than owned, because when the mount
    /// hosts this driver the mount is what keeps the Provider alive.
    ///
    /// Behind a `Mutex` because a `Provider` owns a `rusqlite::Connection`,
    /// which is `Send` but not `Sync`, and connection threads share one
    /// `Arc<Driver>`.
    provider: Arc<Mutex<Provider>>,
    /// Callers waiting for an answer, by id. Shared with whatever drains the
    /// answer stream — our own demultiplexer, or the mount's reply pump.
    pending: Arc<Mutex<HashMap<RequestId, Waiter>>>,
}

/// Where a driver's engine comes from.
///
/// `wusel serve` starts its own and owns it. A driver hosted by a mount
/// (`wusel_fuse::mount_with`) rides the mount's: the mount owns the substrate
/// and drains the answer stream, handing this driver the ids it does not hold.
/// Two substrates over one state database is what this exists to avoid.
// Both variants are pure keep-alive: nothing reads them, and that is the point
// — `Owned` holds the substrate and demultiplexer so they live exactly as long
// as the driver, in that drop order.
#[allow(dead_code)]
enum Engine {
    Owned {
        /// Declared before `_demux` on purpose: it drops first, its `Drop`
        /// closes the answer channel, and the demultiplexer's `recv` then
        /// returns `Err` so the thread finishes on its own.
        substrate: Substrate,
        _demux: JoinHandle<()>,
    },
    Attached,
}

impl Driver {
    /// Start the engine's substrate for `provider` and wrap it in a driver.
    ///
    /// # Errors
    /// If the substrate cannot start (a worker cannot open the state database).
    pub fn start(provider: Provider, pools: Pools) -> wusel_core::Result<Self> {
        // Synchronous write-back is this driver's policy: a socket `publish`
        // blocks until the upload actually lands, so "publish returned Done"
        // means "the server has it" — the completion semantics a File
        // Provider's item upload wants.
        //
        // It is set here, on the substrate we start, and so applies only to a
        // driver that owns its engine. One attached to a mount
        // (`Driver::attach`) inherits the mount's asynchronous write-back
        // instead: the policy belongs to the substrate, and the mount's latency
        // is not ours to trade away. Nothing is lost by that — a file manager
        // queries status, it does not publish.
        let mut ctx = provider.substrate_context();
        ctx.async_upload = false;
        let (substrate, answers) = Substrate::start(&ctx, pools)?;

        let pending: Arc<Mutex<HashMap<RequestId, Waiter>>> = Arc::new(Mutex::new(HashMap::new()));
        let demux_pending = Arc::clone(&pending);
        // One thread drains the substrate's single answer stream and routes each
        // answer to whoever is waiting for its id. It exits on its own when the
        // substrate drops its sending end (see `Drop`).
        let demux = std::thread::Builder::new()
            .name("wusel-ipc-demux".into())
            .spawn(move || {
                while let Ok(answered) = answers.recv() {
                    let mut map = demux_pending.lock().unwrap_or_else(|e| e.into_inner());
                    for id in &answered.requests {
                        if let Some(waiter) = map.remove(id) {
                            // A dropped receiver (the caller gave up) is
                            // harmless — the send simply fails and we move on.
                            let _ = waiter.send((answered.outcome, answered.payload.clone()));
                        }
                    }
                }
            })
            .expect("spawn the ipc demultiplexer thread");

        // Field order is load-bearing for a clean shutdown: `substrate` is
        // declared first, so it drops first, and its `Drop` closes the answer
        // channel — which makes the demultiplexer's `recv` return `Err` so the
        // thread finishes. The `_demux` handle then drops last and simply
        // detaches an already-finished thread. No manual `Drop` is needed.
        Ok(Self {
            ids: substrate.submit_handle(),
            _engine: Engine::Owned {
                substrate,
                _demux: demux,
            },
            provider: Arc::new(Mutex::new(provider)),
            pending,
        })
    }

    /// A driver riding an engine somebody else started — the mount's.
    ///
    /// Nothing is started here: no substrate, no demultiplexer thread. The
    /// requests go out through `ids`, and the answers come back through the
    /// route returned by [`Driver::route`], which the mount's reply pump calls
    /// for every id it does not hold itself.
    ///
    /// `provider` is the mount's, shared: `pin`/`unpin` and the `is_pinned` /
    /// `is_stale` reads on the status path are direct Provider calls rather
    /// than intents, so a driver without one could not answer them — and a
    /// pinned *directory* would report as unpinned, since a directory has no
    /// content state to infer it from.
    #[must_use]
    pub fn attach(ids: SubmitHandle, provider: Arc<Mutex<Provider>>) -> Self {
        Self {
            _engine: Engine::Attached,
            ids,
            provider,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The route that feeds this driver's waiters, for the frontend that owns
    /// the answer stream. See [`Driver::attach`].
    #[must_use]
    pub fn route(&self) -> wusel_core::runtime::AnswerRoute {
        let pending = Arc::clone(&self.pending);
        Arc::new(move |id, answered: &wusel_core::runtime::Answered| {
            if let Some(waiter) = pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id)
            {
                // A dropped receiver (the caller gave up) is harmless.
                let _ = waiter.send((answered.outcome, answered.payload.clone()));
            }
        })
    }

    /// Submit one intent for `object` and block until its answer arrives.
    ///
    /// Fail-soft: a substrate that has stopped, or an answer channel that closes
    /// under us, yields `Failed(Io)` rather than a panic — a frontend must be
    /// able to keep serving other connections.
    pub fn call(&self, object: ObjectId, intent: Intent) -> (Outcome, Payload) {
        self.dispatch(object, intent, None)
    }

    /// Like [`call`](Self::call), but for an intent that carries bytes — a
    /// [`Intent::Write`]. The `data` is parked with the substrate before the
    /// request is sent, so the step that consumes it cannot run first.
    pub fn call_write(
        &self,
        object: ObjectId,
        intent: Intent,
        data: Vec<u8>,
    ) -> (Outcome, Payload) {
        self.dispatch(object, intent, Some(data))
    }

    /// The shared body of [`call`](Self::call) and [`call_write`](Self::call_write):
    /// register a waiter, submit (with or without parked bytes), block for the
    /// answer.
    fn dispatch(
        &self,
        object: ObjectId,
        intent: Intent,
        data: Option<Vec<u8>>,
    ) -> (Outcome, Payload) {
        let id = self.ids.next_request_id();
        let (tx, rx) = channel();
        // Register before submitting, so the answer can never arrive before the
        // waiter is in the map.
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);

        let request = Request { id, object, intent };
        let submitted = match data {
            Some(bytes) => self.ids.submit_write(request, bytes),
            None => self.ids.submit(request),
        };
        if submitted.is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return (Outcome::Failed(Failure::Io), Payload::None);
        }

        rx.recv()
            .unwrap_or((Outcome::Failed(Failure::Io), Payload::None))
    }

    /// Pin `path` (a directory recursively) and hydrate it now, keeping it
    /// offline. Returns the number of files downloaded. Unlike the read/write
    /// path, pinning is not an [`Intent`] — it is a direct `Provider` method — so
    /// it briefly locks the held provider (otherwise idle) for the operation.
    ///
    /// # Errors
    /// If the path does not resolve or a download fails.
    pub fn pin(&self, path: &str) -> wusel_core::Result<usize> {
        self.provider().pin(path)
    }

    /// Unpin `path`, dropping its keep-offline protection (the bytes stay cached
    /// until evicted). See [`pin`](Self::pin). The engine emits a notice if the
    /// path stays offline anyway (covered by a pinned ancestor), so the "still
    /// pinned" result is not needed here.
    ///
    /// # Errors
    /// If the pin store cannot be written.
    pub fn unpin(&self, path: &str) -> wusel_core::Result<()> {
        self.provider().unpin(path).map(|_| ())
    }

    /// Whether `path` is currently kept offline. A cheap read over the pin store,
    /// so a frontend can label its "make available offline" action.
    ///
    /// # Errors
    /// If the pins file cannot be read.
    pub fn is_pinned(&self, path: &str) -> wusel_core::Result<bool> {
        self.provider().is_pinned(path)
    }

    /// Whether `node`'s offline copy is out of date (the server moved past it).
    /// A cheap read over the cache, so a frontend can flag a pinned-but-stale file.
    pub fn is_stale(&self, node: &wusel_core::state::NodeRow) -> bool {
        self.provider().is_stale(node)
    }

    /// Lock the held provider. It is otherwise never locked (it is kept only to
    /// stay alive), so this contends only pin/unpin against each other — never the
    /// read/write path, which goes through the substrate, not the provider.
    fn provider(&self) -> std::sync::MutexGuard<'_, Provider> {
        self.provider.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Resolve a path to an object id **purely through intents** — the boundary
    /// discipline a real frontend must keep: never touch the database or WebDAV
    /// directly, only ask the engine.
    ///
    /// Starts at the root and walks one [`Intent::Lookup`] per path segment,
    /// following each returned node's inode. `Lookup` fills an unlisted
    /// directory from the server itself, so this resolves paths that have never
    /// been enumerated. Any miss — a failed lookup, or an answer that is not a
    /// node — yields `None`.
    pub fn resolve(&self, path: &str) -> Option<ObjectId> {
        let mut object = ObjectId(ROOT_INODE);
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            match self.call(
                object,
                Intent::Lookup {
                    name: segment.to_string(),
                },
            ) {
                (Outcome::Ok, Payload::Node(node)) => object = ObjectId(node.inode),
                _ => return None,
            }
        }
        Some(object)
    }
}
