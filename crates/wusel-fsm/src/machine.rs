// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Occupancy: one clipboard per object.
//!
//! A switchboard, not a worker. It records what is running for an object, which
//! step it is on and who is waiting behind it — and it never makes the calls
//! itself. Both entry points return a list of [`Action`]s for the caller to
//! carry out, which is exactly how a decider stays free of I/O: it says what
//! should happen and somebody else does it.

use std::collections::{HashMap, VecDeque};

use crate::collision::{collision, Collision};
use crate::registry::{Buffer, Registry};
use crate::script::{advance, start, Flow, Next};
use crate::{Completion, Failure, Intent, Job, ObjectId, Request, RequestId};

/// How a flow ended, for everyone who was waiting on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Failed(Failure),
}

/// What the caller should do. The machine never does these itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Hand this to the executor named by [`Job::executor`], along with the
    /// reply handles of everyone waiting, and report back with
    /// [`Machine::on_completion`].
    Dispatch { object: ObjectId, job: Job },
    /// Start this as well, with nobody waiting for it. The machine asks for
    /// follow-up work rather than doing it: serving a stale listing is only
    /// correct if something refreshes it afterwards.
    Schedule { object: ObjectId, intent: Intent },
    /// Re-read this directory from the server, **beside** the machine rather
    /// than through it.
    ///
    /// A background refresh is the one piece of work with nobody waiting for
    /// it, and routing it through occupancy made everybody else wait for it
    /// instead: an `ls` served locally in 20 ms scheduled a refresh, and the
    /// next `ls` — or every `lookup` a file manager makes in that directory —
    /// queued behind a three-second PROPFIND. Aborting would not have helped;
    /// a flow gives up at its next step boundary, and the refresh *is* one
    /// step, so the request in flight still had to finish.
    ///
    /// So it does not become a flow. Nothing is decided about it, which is the
    /// justification: the machine exists to order work that contends, and a
    /// refresh contends with nothing — it reads the server and reconciles rows
    /// in one transaction, which a reader is free to run beside.
    ///
    /// The executor is expected to drop the answer, to refuse a second refresh
    /// of the same object while one runs, and to skip it entirely while that
    /// object is busy with real work.
    Refresh { object: ObjectId },
    /// Run a read-only metadata job **beside** a busy object, then answer this
    /// one request directly with what it produced.
    ///
    /// A `getattr`/`xattr` reads only committed rows — the row an in-flight
    /// upload will not touch until it lands — so it must never queue behind that
    /// upload (minutes, over a slow link). It carries no occupancy and no
    /// ordering, so it does not become a flow the object is busy with; its
    /// answer is delivered on its own, not through the object-keyed payload the
    /// running flow may also be holding.
    ReadBeside {
        object: ObjectId,
        job: Job,
        request: RequestId,
    },
    /// Answer these requests. Note the plural: Join is why.
    Answer {
        /// Whose flow ended. The machine does not need it, but whoever pairs the
        /// answer with the bytes a step produced does — and that pairing must
        /// not be guessed.
        object: ObjectId,
        requests: Vec<RequestId>,
        outcome: Outcome,
    },
}

/// The answer an abandoned request is owed: an error the dead caller will never
/// read, but which frees whatever the kernel is holding for it.
fn answer_interrupted(object: ObjectId, request: RequestId) -> Action {
    Action::Answer {
        object,
        requests: vec![request],
        outcome: Outcome::Failed(Failure::Interrupted),
    }
}

/// A read-only, name-free picture of the machine's occupancy, for diagnostics.
///
/// This is the view a support tool reads to tell a wedged mount from a busy
/// one. It carries object ids (inode numbers) and the *kind* of work, never a
/// file name — a stuck `lookup` or `remove` names a file, and a support bundle
/// must not carry the user's private names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSnapshot {
    /// One entry per object with work in flight or queued, ordered by id.
    pub objects: Vec<BusyObject>,
    /// Open write buffers, and how many of them are dirty (unsaved edits).
    pub buffers_open: usize,
    pub buffers_dirty: usize,
}

/// What one object is doing, as diagnostics sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusyObject {
    /// The inode. A number, never a name.
    pub object: u64,
    /// The kind of the running flow's intent (`fetch`, `publish`, …).
    pub intent: &'static str,
    /// Which step of its script it is on — the value that pinpoints *where* a
    /// stuck flow is stuck.
    pub step: String,
    /// Whether a job is currently handed out to a worker for this object.
    pub outstanding: bool,
    /// How many callers are waiting on this flow (Join), and how many more are
    /// queued behind it. A flow with waiters that never clears is the shape of
    /// the reads that once never got a reply.
    pub waiters: usize,
    pub queued: usize,
    /// Whether the flow has been asked to give up at its next step boundary.
    pub abort: bool,
}

/// What one object is doing. Not in the map = idle.
struct Busy {
    flow: Flow,
    /// The step currently handed out, kept so its completion can be turned into
    /// the right bookkeeping (a created buffer, a discarded one).
    outstanding: Option<Job>,
    /// FIFO. This is where `write` → `flush` ordering lives.
    queue: VecDeque<Request>,
}

/// The switchboard.
pub struct Machine {
    busy: HashMap<ObjectId, Busy>,
    /// Single-step read-only reads running beside a busy object, keyed by the
    /// one request each answers. Outside `busy` on purpose: they hold no
    /// occupancy, so they neither block nor are blocked by the flow the object
    /// is busy with.
    beside: HashMap<RequestId, Flow>,
    registry: Registry,
    /// Whether `flush` is answered as soon as the change is durable (the default,
    /// asynchronous write-back) or held until the upload actually lands (the
    /// synchronous fallback). Only [`Next::AnswerThen`] reads it.
    async_upload: bool,
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            busy: HashMap::new(),
            beside: HashMap::new(),
            registry: Registry::new(),
            async_upload: true,
        }
    }

    /// Choose asynchronous (`true`, the default) or synchronous (`false`)
    /// write-back. In synchronous mode `flush` waits for the upload and reports
    /// its real result, as it did before the change.
    pub fn set_async_upload(&mut self, async_upload: bool) {
        self.async_upload = async_upload;
    }

    /// The buffer bookkeeping, for the frontend to seed and inspect.
    #[must_use]
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
    }

    /// Is anything running for this object?
    #[must_use]
    pub fn is_busy(&self, object: ObjectId) -> bool {
        self.busy.contains_key(&object)
    }

    /// A read-only snapshot of what every object is doing.
    ///
    /// The one view that turns "the mount is wedged and the daemon looks idle"
    /// into "object 1234 has a `fetch` stuck at `FetchBytes` with three waiters"
    /// — without a debugger, and without naming a single file. See
    /// [`MachineSnapshot`].
    #[must_use]
    pub fn snapshot(&self) -> MachineSnapshot {
        let mut objects: Vec<BusyObject> = self
            .busy
            .iter()
            .map(|(object, busy)| BusyObject {
                object: object.0,
                intent: busy.flow.intent.kind(),
                // `Step` is all unit variants, so `Debug` is the bare variant
                // name — a stable, name-free label.
                step: format!("{:?}", busy.flow.step),
                outstanding: busy.outstanding.is_some(),
                waiters: busy.flow.waiters.len(),
                queued: busy.queue.len(),
                abort: busy.flow.abort,
            })
            .collect();
        // A stable order, so two snapshots of one state read the same.
        objects.sort_by_key(|o| o.object);
        MachineSnapshot {
            objects,
            buffers_open: self.registry.len(),
            buffers_dirty: self.registry.dirty_count(),
        }
    }

    /// A request arrived from a dispatch thread.
    pub fn on_request(&mut self, request: Request) -> Vec<Action> {
        let object = request.object;

        if !self.busy.contains_key(&object) {
            return self.begin(request);
        }

        // The object is busy. A read-only metadata read still must not wait: it
        // reads only committed rows, which the in-flight flow has not changed
        // yet, so run it beside that flow rather than behind it. Without this, a
        // `getattr` on a file being uploaded queues for the whole upload — an
        // `ls` that hangs for minutes over a slow link.
        if matches!(request.intent, Intent::Stat | Intent::State) {
            return self.begin_beside(request);
        }

        let facts = self.registry.facts(object);
        let busy = self.busy.get_mut(&object).expect("checked busy above");
        match collision(&busy.flow, &request.intent, &facts) {
            Collision::Queue => {
                busy.queue.push_back(request);
                Vec::new()
            }
            // A flow that has been given up takes no new riders. Joining one is
            // how a live reader ends up owed an answer that is never sent: the
            // flow reaches its next step boundary, sees `abort`, and ends as
            // [`Next::Abandoned`] — which answers nobody, because "nobody is
            // waiting" is exactly what setting `abort` meant. The joiner *is*
            // waiting. Queue it instead, so it runs as its own flow once this
            // one is gone.
            Collision::Join if busy.flow.abort => {
                busy.queue.push_back(request);
                Vec::new()
            }
            Collision::Join => {
                // One transfer, several answers — the whole point of the row.
                busy.flow.waiters.push(request.id);
                Vec::new()
            }
            Collision::Abort => {
                // A request, not an act: the running flow is marked and gives up
                // at its next step boundary, so a side effect already in flight
                // still finishes and is undone rather than cut off.
                busy.flow.abort = true;
                busy.queue.push_back(request);
                Vec::new()
            }
            Collision::Skip => vec![Action::Answer {
                object,
                requests: vec![request.id],
                outcome: Outcome::Ok,
            }],
        }
    }

    /// Nobody is waiting for this request any more — so it must still be
    /// **answered**, with an error, before the work it was riding on is given up.
    ///
    /// The answer is not optional even though the caller is gone. The kernel
    /// holds a locked page for an outstanding `read` until that read is
    /// answered; drop the answer and the page — and every later reader of it —
    /// wedges for good, in an uninterruptible sleep no signal can clear. This is
    /// the one that hung Nautilus: readahead reads left in flight when a handle
    /// closed were abandoned without a reply, and the locked pages never came
    /// back. So the reply is sent as [`Failure::Interrupted`] rather than
    /// dropped.
    ///
    /// Giving up the transfer is a *second*, independent effect — a dead
    /// reader's download is waste over a metered link — and it fires only when
    /// this was the last one waiting. It is a request and not an act: the step
    /// already running may have a side effect that has to finish and be undone
    /// rather than be cut off, so the flow gives up at its next boundary.
    ///
    /// Returns the answer to deliver, or nothing when the request was already
    /// gone — which is how the caller tells a live request from one that had
    /// finished while the abandon message was on its way.
    pub fn abandon(&mut self, request: RequestId) -> Vec<Action> {
        for (object, busy) in &mut self.busy {
            // Riding along on the running flow.
            if let Some(i) = busy.flow.waiters.iter().position(|w| *w == request) {
                busy.flow.waiters.remove(i);
                if busy.flow.waiters.is_empty() {
                    busy.flow.abort = true;
                }
                return vec![answer_interrupted(*object, request)];
            }
            // Queued behind the running flow, never started — still parked, so
            // still owed an answer.
            if let Some(i) = busy.queue.iter().position(|r| r.id == request) {
                busy.queue.remove(i);
                return vec![answer_interrupted(*object, request)];
            }
        }
        Vec::new()
    }

    /// A step finished.
    pub fn on_completion(&mut self, object: ObjectId, completion: Completion) -> Vec<Action> {
        let Some(busy) = self.busy.remove(&object) else {
            // Nothing is running for this object. Reachable only if an executor
            // answers twice; dropping it is the harmless reading.
            return Vec::new();
        };
        let Busy {
            flow,
            outstanding,
            queue,
        } = busy;

        // Record what the finished step changed about the buffer *before*
        // deciding what comes next, so the decision sees the world as it is.
        if let Some(job) = outstanding {
            self.apply_to_registry(
                &job,
                &flow.intent,
                &completion,
                &flow.carry.node.etag,
                flow.carry.node.ignored,
            );
        }

        let facts = self.registry.facts(object);
        let (flow, next) = advance(flow, completion, &facts);
        self.settle(object, flow, next, queue)
    }

    /// Start a request on an idle object.
    fn begin(&mut self, request: Request) -> Vec<Action> {
        let object = request.object;
        let facts = self.registry.facts(object);
        let (flow, next) = start(object, request.intent, request.id, &facts);
        self.settle(object, flow, next, VecDeque::new())
    }

    /// Start a read-only metadata read beside a busy object (see
    /// [`Action::ReadBeside`]). `Stat` and `State` are single-step — one read
    /// job, then the answer — so the flow is parked by request id and finished
    /// by [`on_beside`](Self::on_beside), never entering `busy`.
    fn begin_beside(&mut self, request: Request) -> Vec<Action> {
        let object = request.object;
        let facts = self.registry.facts(object);
        let id = request.id;
        let (flow, next) = start(object, request.intent, id, &facts);
        if let Next::Do(job) = next {
            self.beside.insert(id, flow);
            return vec![Action::ReadBeside {
                object,
                job,
                request: id,
            }];
        }
        // Stat/State always start with a Do; nothing else routes here. Stay
        // total by settling it as an ordinary flow rather than dropping it.
        self.settle(object, flow, next, VecDeque::new())
    }

    /// A beside read finished: advance its one step to the answer and drop it.
    ///
    /// Returns the outcome for the single request it answers; the caller pairs
    /// that with the payload the read produced (a beside read carries its own
    /// payload, never the object-keyed one the busy flow may hold).
    pub fn on_beside(&mut self, request: RequestId, completion: Completion) -> Outcome {
        let Some(flow) = self.beside.remove(&request) else {
            // Already answered or gone — a stray completion with no one waiting.
            return Outcome::Failed(Failure::Interrupted);
        };
        let facts = self.registry.facts(flow.object);
        let (_flow, next) = advance(flow, completion, &facts);
        match next {
            Next::Fail(f) => Outcome::Failed(f),
            // Done. A single-step read yields neither another Do nor AnswerThen.
            _ => Outcome::Ok,
        }
    }

    /// Turn a [`Next`] into actions, releasing the object and starting whatever
    /// queued behind it when the flow ends.
    fn settle(
        &mut self,
        object: ObjectId,
        flow: Flow,
        next: Next,
        mut queue: VecDeque<Request>,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut flow = flow;
        let mut next = next;

        loop {
            match next {
                Next::Do(job) => {
                    actions.push(Action::Dispatch {
                        object,
                        job: job.clone(),
                    });
                    self.busy.insert(
                        object,
                        Busy {
                            flow,
                            outstanding: Some(job),
                            queue,
                        },
                    );
                    return actions;
                }
                Next::AnswerThen(job) => {
                    // Asynchronous (the default): the change is durable, so
                    // answer everyone waiting now, with success, and keep the
                    // object busy with this job and nobody waiting — the upload
                    // runs on in the background.
                    //
                    // Synchronous (the fallback): keep the waiters. They are
                    // answered when the whole flow ends, so `flush` waits for the
                    // upload and reports its real result, success or failure.
                    if self.async_upload && !flow.waiters.is_empty() {
                        actions.push(Action::Answer {
                            object,
                            requests: std::mem::take(&mut flow.waiters),
                            outcome: Outcome::Ok,
                        });
                    }
                    actions.push(Action::Dispatch {
                        object,
                        job: job.clone(),
                    });
                    self.busy.insert(
                        object,
                        Busy {
                            flow,
                            outstanding: Some(job),
                            queue,
                        },
                    );
                    return actions;
                }
                Next::Done | Next::Fail(_) | Next::Abandoned => {
                    // Abandoned normally answers nobody — the flow was given up
                    // *because* whoever wanted it is gone. Anyone still waiting
                    // is therefore not covered by that reasoning and is still
                    // owed a reply: dropping it leaves the kernel holding a
                    // locked page for good (see [`Self::abandon`]). Interrupted
                    // is the honest outcome — the work was given up, not done.
                    if let Some(outcome) = match next {
                        Next::Done => Some(Outcome::Ok),
                        Next::Fail(f) => Some(Outcome::Failed(f)),
                        Next::Abandoned => Some(Outcome::Failed(Failure::Interrupted)),
                        Next::Do(_) | Next::AnswerThen(_) => None,
                    } {
                        if !flow.waiters.is_empty() {
                            actions.push(Action::Answer {
                                object,
                                requests: flow.waiters.clone(),
                                outcome,
                            });
                        }
                    }
                    // A file that was renamed before it ever reached the
                    // server has to be published under its new name — the
                    // office-suite atomic save. Scheduled rather than chained,
                    // because a failed upload must not fail the rename: the
                    // local rename is already committed, and reporting failure
                    // would tell the kernel it never happened.
                    if matches!(flow.intent, Intent::Move { .. })
                        && !flow.carry.node.materialised
                        && flow.carry.node.id != ObjectId::default()
                    {
                        actions.push(Action::Schedule {
                            object: flow.carry.node.id,
                            intent: Intent::Publish,
                        });
                    }
                    // A listing that was served past its revalidation interval
                    // owes the next caller a fresher one — beside the machine,
                    // never in front of the next caller. See [`Action::Refresh`].
                    if matches!(flow.intent, Intent::Enumerate) && flow.carry.node.listing_stale {
                        actions.push(Action::Refresh { object });
                    }
                    // The object is idle. Start the next queued request, which
                    // may itself finish without a job — hence the loop rather
                    // than a recursive call.
                    let Some(request) = queue.pop_front() else {
                        return actions;
                    };
                    let facts = self.registry.facts(object);
                    let (f, n) = start(object, request.intent, request.id, &facts);
                    flow = f;
                    next = n;
                }
            }
        }
    }

    /// What a finished step means for the buffer bookkeeping.
    ///
    /// The machine is the authority on whether an object is dirty (see
    /// [`crate::registry`]), so this is where that authority is exercised — one
    /// place, driven by the job that actually completed rather than by what we
    /// hoped it would do.
    fn apply_to_registry(
        &mut self,
        job: &Job,
        intent: &Intent,
        completion: &Completion,
        base_etag: &str,
        ignored: bool,
    ) {
        if matches!(completion, Completion::Failed(_)) {
            return; // a step that failed changed nothing
        }
        match job {
            Job::CreateBuffer { object } | Job::HydrateBuffer { object } => {
                // The version the buffer is based on — what a later upload
                // asserts, and what tells a conflict from an ordinary save.
                let mut buffer = Buffer::new(base_etag.to_string());
                buffer.ignored = ignored;
                // A newly created object is a pending change even with no bytes
                // yet, so `touch` alone still publishes it.
                buffer.dirty = matches!(intent, Intent::Materialise { .. });
                self.registry.open(*object, buffer);
            }
            Job::WriteBuffer { object, .. } | Job::TruncateBuffer { object, .. } => {
                self.registry.mark_dirty(*object);
            }
            // The timestamp is recorded in the state *and* handed to the buffer,
            // so it travels with the upload as `X-OC-Mtime` rather than costing
            // the server a second round-trip afterwards.
            Job::RecordMtime { object, mtime } => {
                self.registry.set_pending_mtime(*object, *mtime);
            }
            Job::DiscardBuffer { object } => {
                self.registry.close(*object);
            }
            Job::ReadNode { .. }
            | Job::ReadChildren { .. }
            | Job::ReadBuffer { .. }
            | Job::ReadBlob { .. }
            | Job::FetchRange { .. }
            | Job::ListRemote { .. }
            | Job::BufferSize { .. }
            | Job::Upload { .. }
            | Job::ResolveConflict { .. }
            | Job::RecordVersion { .. }
            | Job::StoreBlob { .. }
            | Job::HydrateCache { .. }
            | Job::InsertNode { .. }
            | Job::CreateRemoteDir { .. }
            | Job::DeleteRemote { .. }
            | Job::MoveRemote { .. }
            | Job::RemoveRows { .. }
            | Job::MoveRows { .. }
            | Job::MarkPending { .. }
            | Job::ClearPending { .. }
            | Job::SetUploadError { .. }
            | Job::ReadChild { .. }
            | Job::ReadState { .. } => {}
        }
    }
}
