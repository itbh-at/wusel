// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Shared helpers for the wusel-mock integration tests.
//!
//! This lives in `tests/common/mod.rs` — the standard way to share code between
//! integration tests. (A `tests/common.rs` would not work: cargo compiles every
//! top-level file under `tests/` as its own test binary, so a phantom, empty
//! "common" suite would run on every `cargo test`.) Each test file pulls it in
//! with `mod common;`, compiling its own copy — hence the `dead_code` allow:
//! no single test binary uses every helper.
#![allow(dead_code)]

use std::path::Path;

/// An in-process wusel-mock WebDAV server, shut down when this guard drops.
///
/// The listener is bound *here*, synchronously, and handed to
/// [`wusel_mock::serve`] pre-bound (the same pattern as
/// `wusel-fuse/tests/mount_e2e.rs`). That kills two problems of the old
/// "free port + spawned binary" helper at once:
///
/// * **No port TOCTOU.** The old helper asked the OS for a free port, released
///   it, and had the spawned `wusel-mock` binary re-bind it later — under
///   parallel CI another test could grab the port in between. A pre-bound
///   listener cannot race.
/// * **No "wait until listening" poll.** Connections queue in the listener's
///   backlog from the moment `bind` returns, even before the accept loop runs,
///   so tests can connect immediately.
pub struct Mock {
    /// `host:port` the server listens on, e.g. `127.0.0.1:49213`.
    pub addr: String,
    /// The server's private runtime. `Option` so `Drop` can move it out.
    rt: Option<tokio::runtime::Runtime>,
}

impl Mock {
    /// Serve `root` as user `alice` (what every test uses) on an OS-chosen port.
    pub fn serve(root: &Path) -> Self {
        // Bind synchronously so the port is ours before this returns.
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
        let addr = std_listener
            .local_addr()
            .expect("mock listener addr")
            .to_string();
        // Tokio's reactor drives readiness via non-blocking sockets; a std
        // listener is blocking by default, so flip it before the handover.
        std_listener.set_nonblocking(true).expect("set_nonblocking");

        // A private single-worker runtime, so this one helper serves plain
        // `#[test]`s and `#[tokio::test]`s alike. (Nesting *runtimes* is fine —
        // only a nested `block_on` is not, and we never block here.)
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build mock runtime");
        let root = root.to_path_buf();
        rt.spawn(async move {
            // `from_std` registers with the runtime's reactor, so it must run
            // inside the runtime — hence here, not before the spawn.
            let listener = tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
            let _ = wusel_mock::serve(listener, root, "alice").await;
        });
        Mock { addr, rt: Some(rt) }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        // Shutting down drops the pending `serve` future, whose internal guard
        // then removes the uploads scratch directory. In a plain `#[test]` we
        // can afford the *blocking* shutdown, which guarantees that cleanup ran
        // before the test binary exits. Inside a `#[tokio::test]` body blocking
        // (like dropping a runtime outright) would panic, so there we fall back
        // to the non-blocking variant and the cleanup becomes best-effort — it
        // may lose a race against process exit.
        if let Some(rt) = self.rt.take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                rt.shutdown_background();
            } else {
                rt.shutdown_timeout(std::time::Duration::from_secs(1));
            }
        }
    }
}

/// Point the XDG base directories at a throwaway location under `base`, so the
/// account's config/state/cache never touch the real home directory.
///
/// # Why `set_var` is safe *here*, and only here
///
/// `std::env::set_var` is documented-unsound on a multi-threaded process: the
/// POSIX environment is a plain global with no locking, so a concurrent
/// `getenv` in another thread may read a dangling pointer. Rust made it
/// `unsafe` in edition 2024 for exactly that reason (this crate is on 2021, so
/// it still compiles without the keyword — the hazard is identical). libtest is
/// multi-threaded, and both reqwest (proxy variables) and the keyring read the
/// environment, so this is not a theoretical concern in general.
///
/// The only thing that makes it sound in this harness is a *structural*
/// property, not a lucky ordering: **each test binary that calls this has
/// exactly one `#[test]`, and calls it as its first statement**, before any
/// runtime, HTTP client or keyring exists. With one test, libtest has spawned
/// one thread and is itself parked waiting for it — nobody else can be inside
/// `getenv`. Every reader of `XDG_*` in this process is created afterwards.
///
/// Doing better would mean injecting the base directories into `wusel-core`
/// instead of going through the environment, which its `config::xdg_dir` does
/// not offer; that is an engine-side change, not a test-side one.
///
/// Because the argument rests entirely on "exactly one test, called first", the
/// first half of it is *enforced* rather than trusted: a second call — which is
/// what adding a second `#[test]` to such a file would produce — panics instead
/// of racing. Silence would be the dangerous outcome, so this is deliberately
/// loud.
pub fn xdg_sandbox(base: &Path) {
    // Per test binary: cargo compiles a private copy of this module into each.
    static SANDBOXED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    assert!(
        !SANDBOXED.swap(true, std::sync::atomic::Ordering::SeqCst),
        "xdg_sandbox must be called exactly once per test binary: the env \
         mutation below is only sound while this process is effectively \
         single-threaded. Split the tests into separate files instead."
    );

    let xdg = base.join("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.join("config"));
    std::env::set_var("XDG_STATE_HOME", xdg.join("state"));
    std::env::set_var("XDG_CACHE_HOME", xdg.join("cache"));
}

// --- The engine under test, driven the way the mount drives it --------------

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use wusel_core::config::Account;
use wusel_core::provider::{FileState, Provider};
use wusel_core::runtime::{Answered, Payload, Pools, Substrate};
use wusel_core::state::{NodeRow, StateDb};
use wusel_core::webdav::WebDavClient;
use wusel_fsm::{Intent, ObjectId, Outcome, Request, RequestId};

/// How long a step may take before the test calls it a hang. Generous: a
/// chunked upload of several megabytes against the mock is still well inside it.
const PATIENCE: Duration = Duration::from_secs(30);

/// An account's engine plus the substrate that runs its work.
///
/// The tests below drive operations exactly as the mount does — an intent in,
/// an answer out — rather than through a second set of entry points that only
/// tests would use. A path that only tests exercise is a path that can drift
/// from the one that ships while still looking green.
///
/// The `Provider` remains for what is genuinely not per-request: resolving a
/// path at startup, pins, and the background syncer.
pub struct Engine {
    provider: Provider,
    substrate: Substrate,
    answers: Receiver<Answered>,
    next: AtomicU64,
}

impl Engine {
    /// Build an account against `addr` with its state under the sandboxed XDG
    /// directories, and start its substrate.
    pub fn start(addr: &str) -> Self {
        Self::start_with(addr, None)
    }

    /// The same, with a desktop backend in place *before* the substrate is
    /// built — its workers take a copy, so setting one afterwards would leave
    /// them talking to the old one.
    pub fn start_with(
        addr: &str,
        desktop: Option<std::sync::Arc<dyn wusel_core::desktop::Desktop>>,
    ) -> Self {
        let account = Account::new("default");
        let dav = WebDavClient::new(
            reqwest::Client::new(),
            &format!("http://{addr}"),
            "alice",
            "pw",
        );
        std::fs::create_dir_all(account.state_db_path().parent().unwrap()).unwrap();
        let state = StateDb::open(&account.state_db_path()).unwrap();
        let mut provider = Provider::new(dav, state, &account).unwrap();
        if let Some(d) = desktop {
            provider.set_desktop(d);
        }
        let (substrate, answers) =
            Substrate::start(&provider.substrate_context(), Pools::default()).unwrap();
        Self {
            provider,
            substrate,
            answers,
            next: AtomicU64::new(1),
        }
    }

    /// For what is not a per-request operation: pins, the syncer, startup.
    pub fn provider(&mut self) -> &mut Provider {
        &mut self.provider
    }

    // Straight through to the engine. These are not per-request operations —
    // they are startup and user-intent, and they never touch a write buffer, so
    // there is no authority to split.

    pub fn resolve(&mut self, path: &str) -> wusel_core::Result<Option<NodeRow>> {
        self.provider.resolve(path)
    }

    pub fn pin(&mut self, path: &str) -> wusel_core::Result<usize> {
        self.provider.pin(path)
    }

    pub fn unpin(&mut self, path: &str) -> wusel_core::Result<()> {
        self.provider.unpin(path)
    }

    pub fn pins(&self) -> wusel_core::Result<Vec<(String, bool)>> {
        self.provider.pins()
    }

    /// Wait until the syncer has picked up a server-side change to `path`, the
    /// way it really happens: a push, then the walk that reconciles the new
    /// version. Panics if it does not arrive — a wedged syncer is a failure, not
    /// a slow test.
    ///
    /// Used by the `open_pinned` tests, where the whole point is what happens
    /// *after* the local copy has gone out of date.
    pub fn wait_until_stale(&mut self, path: &str, was: &str) {
        let push = self.sync_trigger();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while self.resolve(path).unwrap().unwrap().etag == was {
            assert!(
                std::time::Instant::now() < deadline,
                "the syncer never picked up the server-side change to {path}"
            );
            push();
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    pub fn sync_trigger(&self) -> impl Fn() {
        let tx = self.provider.sync_trigger();
        move || {
            let _ = tx.send(());
        }
    }

    #[allow(dead_code)]
    fn sync_trigger_unused(&self) -> std::sync::mpsc::Sender<()> {
        self.provider.sync_trigger()
    }

    pub fn take_invalidations(&mut self) -> Option<Receiver<wusel_core::provider::Invalidation>> {
        self.provider.take_invalidations()
    }

    pub fn set_desktop(&mut self, desktop: std::sync::Arc<dyn wusel_core::desktop::Desktop>) {
        self.provider.set_desktop(desktop);
    }

    pub fn invalidation_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicI64> {
        self.provider.invalidation_handle()
    }

    /// A directory's children, under the name the old engine API used.
    pub fn list_dir(&self, ino: u64) -> Vec<NodeRow> {
        self.list(ino)
    }

    fn ticket(&self) -> RequestId {
        RequestId(self.next.fetch_add(1, Ordering::Relaxed))
    }

    /// Wait for this ticket's answer, ignoring any that belong to another.
    fn wait(&self, id: RequestId) -> (Outcome, Payload) {
        loop {
            let a = self
                .answers
                .recv_timeout(PATIENCE)
                .unwrap_or_else(|e| panic!("no answer for {id:?}: {e}"));
            if a.requests.contains(&id) {
                return (a.outcome, a.payload);
            }
        }
    }

    fn run(&self, object: u64, intent: Intent) -> (Outcome, Payload) {
        let id = self.ticket();
        self.substrate
            .submit(Request {
                id,
                object: ObjectId(object),
                intent,
            })
            .expect("submit");
        self.wait(id)
    }

    fn run_write(&self, object: u64, intent: Intent, data: Vec<u8>) -> (Outcome, Payload) {
        let id = self.ticket();
        self.substrate
            .submit_write(
                Request {
                    id,
                    object: ObjectId(object),
                    intent,
                },
                data,
            )
            .expect("submit");
        self.wait(id)
    }

    // --- the operations, as the mount issues them --------------------------

    pub fn read(&self, ino: u64, offset: u64, len: u32) -> Result<Vec<u8>, Outcome> {
        match self.run(ino, Intent::Fetch { offset, len }) {
            (Outcome::Ok, Payload::Bytes(b)) => Ok(b),
            (Outcome::Ok, _) => Ok(Vec::new()),
            (other, _) => Err(other),
        }
    }

    pub fn write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u32, Outcome> {
        let len = data.len() as u32;
        match self.run_write(ino, Intent::Write { offset, len }, data.to_vec()) {
            (Outcome::Ok, Payload::Written(n)) => Ok(n),
            (Outcome::Ok, _) => Ok(len),
            (other, _) => Err(other),
        }
    }

    pub fn flush(&self, ino: u64) -> Result<(), Outcome> {
        match self.run(ino, Intent::Publish) {
            (Outcome::Ok, _) => Ok(()),
            (other, _) => Err(other),
        }
    }

    /// Wait until every pending upload has settled — landed (the record is
    /// cleared) or parked as an error (a terminal state a test can then assert
    /// on). Uploads are asynchronous now: `flush` returns once the change is
    /// durable locally, so a test that inspects the *server* must wait for this
    /// first. Panics on a 10-second timeout.
    pub fn wait_for_uploads(&self) {
        // Let work just scheduled (a promotion publish after a rename is
        // scheduled the moment the rename is answered) reach the decider before
        // we start judging idleness.
        std::thread::sleep(std::time::Duration::from_millis(30));
        let db_path = Account::new("default").state_db_path();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            // Settled means two things at once: nothing is running for any
            // object, and no pending record is still queued or in flight (an
            // error record is a terminal state a test can then assert on).
            let idle = self
                .substrate
                .snapshot()
                .map(|s| s.machine.objects.is_empty())
                .unwrap_or(true);
            let pending = StateDb::open_existing(&db_path)
                .unwrap()
                .pending_uploads()
                .unwrap();
            let unsettled = pending
                .iter()
                .filter(|p| !matches!(p.state, wusel_core::state::UploadState::Error))
                .count();
            if idle && unsettled == 0 {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "uploads did not settle within 10 s: {unsettled} still in flight"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    /// Wait until the initial upload of `ino` has been attempted and left the
    /// record `pending` — the machine idle again with the change still owed. With
    /// retries suppressed (`WUSEL_UPLOAD_RETRY_SECS` set high) this is the state a
    /// resume test drops the engine in: committed, attempted, not landed.
    pub fn wait_until_upload_attempted(&self, ino: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let idle = self
                .substrate
                .snapshot()
                .map(|s| s.machine.objects.is_empty())
                .unwrap_or(true);
            if idle
                && matches!(
                    self.upload_state(ino),
                    Some(wusel_core::state::UploadState::Pending)
                )
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the upload of inode {ino} was never attempted-and-left-pending"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    /// How a pending upload stands right now (`None` = none pending — landed or
    /// never created). Lets a failure test assert the error was recorded.
    pub fn upload_state(&self, ino: u64) -> Option<wusel_core::state::UploadState> {
        let db_path = Account::new("default").state_db_path();
        StateDb::open_existing(&db_path)
            .unwrap()
            .pending_upload(wusel_fsm::ObjectId(ino))
            .unwrap()
            .map(|p| p.state)
    }

    pub fn create(&self, parent: u64, name: &str) -> Result<NodeRow, Outcome> {
        self.made(parent, name, false)
    }

    pub fn mkdir(&self, parent: u64, name: &str) -> Result<NodeRow, Outcome> {
        self.made(parent, name, true)
    }

    fn made(&self, parent: u64, name: &str, dir: bool) -> Result<NodeRow, Outcome> {
        let intent = Intent::Materialise {
            name: name.to_string(),
            dir,
        };
        match self.run(parent, intent) {
            (Outcome::Ok, Payload::Node(n)) => Ok(*n),
            (Outcome::Ok, _) => Err(Outcome::Failed(wusel_fsm::Failure::Io)),
            (other, _) => Err(other),
        }
    }

    pub fn remove(&self, parent: u64, name: &str) -> Result<(), Outcome> {
        let intent = Intent::Remove {
            name: name.to_string(),
        };
        match self.run(parent, intent) {
            (Outcome::Ok, _) => Ok(()),
            (other, _) => Err(other),
        }
    }

    pub fn rename(
        &self,
        parent: u64,
        name: &str,
        to_parent: u64,
        to_name: &str,
    ) -> Result<(), Outcome> {
        let intent = Intent::Move {
            from_name: name.to_string(),
            to_parent: ObjectId(to_parent),
            to_name: to_name.to_string(),
        };
        match self.run(parent, intent) {
            (Outcome::Ok, _) => Ok(()),
            (other, _) => Err(other),
        }
    }

    pub fn truncate(&self, ino: u64, size: u64) -> Result<(), Outcome> {
        match self.run(
            ino,
            Intent::SetAttr {
                size: Some(size),
                mtime: None,
            },
        ) {
            (Outcome::Ok, _) => Ok(()),
            (other, _) => Err(other),
        }
    }

    pub fn set_mtime(&self, ino: u64, mtime: i64) -> Result<(), Outcome> {
        match self.run(
            ino,
            Intent::SetAttr {
                size: None,
                mtime: Some(mtime),
            },
        ) {
            (Outcome::Ok, _) => Ok(()),
            (other, _) => Err(other),
        }
    }

    pub fn stat(&self, ino: u64) -> Option<NodeRow> {
        match self.run(ino, Intent::Stat) {
            (Outcome::Ok, Payload::Node(n)) => Some(*n),
            _ => None,
        }
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Option<NodeRow> {
        let intent = Intent::Lookup {
            name: name.to_string(),
        };
        match self.run(parent, intent) {
            (Outcome::Ok, Payload::Node(n)) => Some(*n),
            _ => None,
        }
    }

    pub fn list(&self, ino: u64) -> Vec<NodeRow> {
        match self.run(ino, Intent::Enumerate) {
            (Outcome::Ok, Payload::Entries(rows)) => rows,
            _ => Vec::new(),
        }
    }

    pub fn state(&self, ino: u64) -> Option<FileState> {
        match self.run(ino, Intent::State) {
            (Outcome::Ok, Payload::State(s)) => Some(s),
            _ => None,
        }
    }
}
