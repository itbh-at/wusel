// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Pins — "always keep this offline" — kept outside the cache.
//!
//! A pin is the one thing in this program that the user *said*. Everything else
//! in the state database is a copy of something the server already knows and can
//! be thrown away and fetched again; a pin cannot. That is why it does not live
//! there:
//!
//! * `cache clear` empties the database. Losing pins to it would break the
//!   offline promise at exactly the moment somebody was clearing space before a
//!   trip.
//! * A database on a network home is [relocated to local
//!   storage](crate::storage) and becomes per-machine. Pins must not follow it —
//!   they belong to the person, not to the laptop.
//! * A rebuild after corruption is a cold start, not a loss of intent.
//!
//! So they live in a small file next to the configuration, which is small,
//! rarely written, and part of a roaming profile.
//!
//! # Two processes, one file
//!
//! `wusel pin` runs as its own process while the daemon is mounted, so this is
//! genuinely shared state — the thing SQLite used to handle. Two mechanisms
//! replace it, and neither needs a dependency:
//!
//! * **Writing** takes a lock directory, re-reads the file, changes it, and
//!   renames a temporary file over it. Read-modify-write under a lock, so a
//!   concurrent `pin` and `unpin` cannot lose one another; the rename means a
//!   reader never sees half a file, whatever happens mid-write.
//! * **Reading** compares the file's mtime and length against what was last
//!   loaded and re-reads when they differ. So the mounted daemon picks up a pin
//!   made from the command line without being told, which is what the database
//!   did for free.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

use crate::{Error, Result};

/// The on-disk form. A version tag because this file outlives the database and
/// will be read by a future that has forgotten this one.
#[derive(serde::Serialize, serde::Deserialize)]
struct Document {
    version: u32,
    pins: Vec<Entry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    path: String,
    /// Whether the pin covers a subtree. Named `dir` on disk because that is
    /// what it means to a person reading the file.
    dir: bool,
}

const VERSION: u32 = 1;

/// Path → is_dir. A `BTreeMap` so the file comes out in a stable order and a
/// diff of it means something.
type Map = BTreeMap<String, bool>;

// --- The decisions, as plain functions ---------------------------------------
//
// Everything below this line that decides anything takes a `Map` and returns a
// value. No file, no lock, no clock — so the semantics that matter (a directory
// pin covers its subtree, a rename carries the promise, a delete drops it) are
// tested directly, and the file handling has nothing clever left in it.

/// Whether `path` is kept offline: pinned itself, under a pinned *directory*, or
/// covered by the root pin.
///
/// Bounded by the path's depth rather than the number of pins — this is asked
/// once per file when a file manager draws a folder.
#[must_use]
pub fn covers(map: &Map, path: &str) -> bool {
    // A pin on the entry itself counts whatever kind it is …
    if map.contains_key(path) {
        return true;
    }
    // … an ancestor only if it is a *directory* pin, because that is what
    // promises a subtree.
    for (i, _) in path.match_indices('/') {
        if map.get(&path[..i]) == Some(&true) {
            return true;
        }
    }
    // The root pin ("") is "keep everything offline".
    map.get("") == Some(&true)
}

/// Every pin at or below `path`, root included.
#[must_use]
fn under(map: &Map, path: &str) -> Vec<String> {
    let prefix = format!("{path}/");
    map.keys()
        .filter(|p| p.as_str() == path || p.starts_with(&prefix))
        .cloned()
        .collect()
}

/// Rewrite every pin at or below `old` onto `new`, so a rename carries the
/// promise with it.
///
/// Without this, renaming a pinned directory silently unpins it: pins are keyed
/// by path, so the new path answers `false`, newly added files are never
/// hydrated, the emblem is wrong, and the eviction markers keep protecting blobs
/// nothing points at any more.
fn rename_in(map: &mut Map, old: &str, new: &str) {
    if old.is_empty() || old == new {
        return; // the root never moves, and a no-op move changes nothing
    }
    // Collect first, then apply: the destination may itself carry a pin (a
    // rename that overwrites), and two passes keep the result from depending on
    // the order the keys come back in.
    let moved: Vec<(String, bool)> = under(map, old)
        .into_iter()
        .map(|p| {
            let is_dir = map[&p];
            let rest = &p[old.len()..];
            (format!("{new}{rest}"), is_dir)
        })
        .collect();
    for p in under(map, old) {
        map.remove(&p);
    }
    map.extend(moved);
}

// --- The file ----------------------------------------------------------------

/// The pins of one account.
///
/// Cheap to clone via `Arc` at the call site; the cache inside is shared and
/// refreshed from disk whenever the file has changed underneath.
pub struct Pins {
    file: PathBuf,
    cache: RwLock<Cache>,
}

/// What was read, and what it was read from — so a re-read happens exactly when
/// the file has moved on.
#[derive(Default)]
struct Cache {
    stamp: Option<(SystemTime, u64)>,
    map: Map,
    /// Set once the file has been read at least once. Distinguishes "no pins"
    /// from "never looked", which decides whether an absent file is an answer.
    loaded: bool,
}

impl Pins {
    /// The pins file for a config directory. Reading is lazy — nothing touches
    /// the disk until somebody asks a question.
    #[must_use]
    pub fn new(config_dir: &Path) -> Self {
        Self {
            file: config_dir.join("pins.json"),
            cache: RwLock::new(Cache::default()),
        }
    }

    /// Where the pins are kept, for messages that need to name it.
    #[must_use]
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// Whether `path` is kept offline.
    ///
    /// # Errors
    /// If the file exists but cannot be read or parsed. An *absent* file is not
    /// an error: it is an account with no pins.
    pub fn is_pinned(&self, path: &str) -> Result<bool> {
        self.with(|map| covers(map, path.trim_matches('/')))
    }

    /// All pins as `(path, is_dir)`, in path order.
    ///
    /// # Errors
    /// If the file cannot be read or parsed.
    pub fn all(&self) -> Result<Vec<(String, bool)>> {
        self.with(|map| map.iter().map(|(p, d)| (p.clone(), *d)).collect())
    }

    /// Record a pin on `path` (empty = the account root).
    ///
    /// # Errors
    /// If the file cannot be read, written, or the lock not taken.
    pub fn set(&self, path: &str, is_dir: bool) -> Result<()> {
        self.change(|map| {
            map.insert(path.trim_matches('/').to_string(), is_dir);
        })
    }

    /// Remove the pin on exactly `path`, leaving any below it alone.
    ///
    /// # Errors
    /// If the file cannot be read, written, or the lock not taken.
    pub fn remove(&self, path: &str) -> Result<()> {
        self.change(|map| {
            map.remove(path.trim_matches('/'));
        })
    }

    /// Drop the pin on `path` and every pin below it, returning how many went.
    ///
    /// Used when a directory is deleted: its subtree's "keep offline" promises
    /// are void, and a pin left behind would keep protecting blobs for a path
    /// that no longer exists.
    ///
    /// # Errors
    /// If the file cannot be read, written, or the lock not taken.
    pub fn remove_under(&self, path: &str) -> Result<usize> {
        let path = path.trim_matches('/').to_string();
        let mut removed = 0;
        self.change(|map| {
            if path.is_empty() {
                removed = map.len();
                map.clear();
                return;
            }
            for p in under(map, &path) {
                map.remove(&p);
                removed += 1;
            }
        })?;
        Ok(removed)
    }

    /// Carry the pins of a renamed subtree to its new path.
    ///
    /// # Errors
    /// If the file cannot be read, written, or the lock not taken.
    pub fn rename(&self, old: &str, new: &str) -> Result<()> {
        let old = old.trim_matches('/').to_string();
        let new = new.trim_matches('/').to_string();
        self.change(|map| rename_in(map, &old, &new))
    }

    /// Take the pins out of a state database, once.
    ///
    /// Only when this file does not exist yet: after the first start on the new
    /// code the file is the truth, and re-importing would resurrect pins the
    /// user has since removed. Returns how many were taken over.
    ///
    /// # Errors
    /// If the file cannot be written.
    pub fn migrate_from(&self, existing: &[(String, bool)]) -> Result<usize> {
        if existing.is_empty() || self.file.exists() {
            return Ok(0);
        }
        let _lock = Lock::take(&self.file)?;
        // Checked again under the lock: two processes may start at once, and
        // the second must not overwrite what the first has already written.
        if self.file.exists() {
            return Ok(0);
        }
        let map: Map = existing.iter().cloned().collect();
        let n = map.len();
        self.write(&map)?;
        Ok(n)
    }

    /// Answer a question from the current contents, re-reading if the file has
    /// changed since last time.
    fn with<T>(&self, f: impl FnOnce(&Map) -> T) -> Result<T> {
        let stamp = stamp_of(&self.file);
        {
            let cache = self.cache.read().unwrap_or_else(|e| e.into_inner());
            if cache.loaded && cache.stamp == stamp {
                return Ok(f(&cache.map));
            }
        }
        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        // Re-checked under the write lock: another thread may have refreshed it
        // while this one was waiting, and re-reading would be pure waste.
        if !cache.loaded || cache.stamp != stamp {
            cache.map = read_file(&self.file)?;
            cache.stamp = stamp;
            cache.loaded = true;
        }
        Ok(f(&cache.map))
    }

    /// Read, change, write — under the lock, so a concurrent process cannot
    /// lose one of the two changes.
    fn change(&self, f: impl FnOnce(&mut Map)) -> Result<()> {
        let _lock = Lock::take(&self.file)?;
        // From disk, not from the cache: another process may have written since
        // this one last looked, and its change must survive ours.
        let mut map = read_file(&self.file)?;
        f(&mut map);
        self.write(&map)?;
        Ok(())
    }

    /// Replace the file atomically and adopt what was written as the cache.
    fn write(&self, map: &Map) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(io("create the config directory"))?;
        }
        let doc = Document {
            version: VERSION,
            pins: map
                .iter()
                .map(|(path, dir)| Entry {
                    path: path.clone(),
                    dir: *dir,
                })
                .collect(),
        };
        let text = serde_json::to_string_pretty(&doc)
            .map_err(|e| Error::Other(format!("could not encode the pins: {e}")))?;
        // Same directory, so the rename below is a rename and not a copy; the
        // pid keeps two processes from sharing a temporary file.
        let tmp = self
            .file
            .with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, text.as_bytes()).map_err(io("write the pins file"))?;
        // The rename is the commit: a reader sees either the old file or the
        // new one, never a half-written one, even if the machine loses power.
        std::fs::rename(&tmp, &self.file).map_err(io("replace the pins file"))?;

        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        cache.map = map.clone();
        cache.stamp = stamp_of(&self.file);
        cache.loaded = true;
        Ok(())
    }
}

impl std::fmt::Debug for Pins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pins").field("file", &self.file).finish()
    }
}

/// A file's identity for cache purposes: when it changed and how big it is.
///
/// `None` means "not there", which is a perfectly good answer and compares equal
/// to itself — an account with no pins does not re-read anything.
fn stamp_of(file: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(file).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn read_file(file: &Path) -> Result<Map> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        // No file is not an error: it is an account nobody has pinned anything
        // in. Any other error is real and must not be read as "no pins", which
        // would quietly unprotect everything.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(io("read the pins file")(e)),
    };
    let doc: Document = serde_json::from_str(&text)
        .map_err(|e| Error::Other(format!("{}: {e}", file.display())))?;
    if doc.version > VERSION {
        return Err(Error::Other(format!(
            "{} was written by a newer version of wusel ({} > {VERSION}); \
             refusing to read it rather than dropping pins it may contain",
            file.display(),
            doc.version
        )));
    }
    Ok(doc.pins.into_iter().map(|e| (e.path, e.dir)).collect())
}

fn io(what: &'static str) -> impl Fn(std::io::Error) -> Error {
    move |e| Error::Other(format!("could not {what}: {e}"))
}

// --- The lock ----------------------------------------------------------------

/// A cross-process lock on the pins file, released when dropped.
///
/// A *directory* rather than a file: `mkdir` is atomic and refuses to succeed
/// twice on every filesystem worth the name, including the network ones this
/// file exists to survive — which is exactly where `O_EXCL` on a file has
/// historically been unreliable.
struct Lock {
    dir: PathBuf,
}

/// How long a lock may be held before it is assumed to belong to a process that
/// died. Generous: the operations under it are a read, an edit and a rename.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

impl Lock {
    fn take(file: &Path) -> Result<Self> {
        Self::take_after(file, STALE_AFTER)
    }

    /// The lock, with the staleness limit as a parameter so a test can watch a
    /// dead holder actually being displaced rather than trust that it would be.
    fn take_after(file: &Path, stale_after: std::time::Duration) -> Result<Self> {
        let dir = file.with_extension("lock");
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).map_err(io("create the config directory"))?;
        }
        // A second is far longer than a read-edit-rename takes; anything longer
        // is not contention but a corpse, handled below.
        for _ in 0..50 {
            match std::fs::create_dir(&dir) {
                Ok(()) => return Ok(Self { dir }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if stale(&dir, stale_after) {
                        // Whoever held this is gone. Removing it may race with
                        // another process doing the same; the loop settles it,
                        // because only one `create_dir` can win.
                        let _ = std::fs::remove_dir(&dir);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => return Err(io("take the pins lock")(e)),
            }
        }
        Err(Error::Other(format!(
            "the pins file is locked by another wusel process ({}); \
             remove it by hand if no wusel is running",
            dir.display()
        )))
    }
}

/// Whether a lock has been held so long that its holder must be gone.
///
/// A clock going backwards (`elapsed` failing) reads as *not* stale: breaking
/// somebody else's lock on a bad timestamp is the worse mistake of the two.
fn stale(dir: &Path, stale_after: std::time::Duration) -> bool {
    let Ok(created) = std::fs::metadata(dir).and_then(|m| m.modified()) else {
        return false;
    };
    created.elapsed().is_ok_and(|age| age > stale_after)
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, bool)]) -> Map {
        entries
            .iter()
            .map(|(p, d)| ((*p).to_string(), *d))
            .collect()
    }

    // --- the semantics, without a disk ---

    #[test]
    fn a_pinned_file_is_covered_and_its_neighbour_is_not() {
        let m = map(&[("Docs/Report.pdf", false)]);
        assert!(covers(&m, "Docs/Report.pdf"));
        assert!(!covers(&m, "Docs/Other.pdf"));
        assert!(!covers(&m, "Docs"));
    }

    #[test]
    fn a_directory_pin_covers_its_whole_subtree() {
        let m = map(&[("Photos", true)]);
        assert!(covers(&m, "Photos"));
        assert!(covers(&m, "Photos/2024/pic.jpg"));
        // …and only its subtree: a name that merely starts the same does not
        // count, which is the bug a plain string prefix would introduce.
        assert!(!covers(&m, "PhotosOld/pic.jpg"));
    }

    #[test]
    fn a_file_pin_does_not_cover_anything_below_it() {
        // Nothing lives below a file, but the map cannot know that — and a pin
        // recorded as a file must not behave like a directory pin.
        let m = map(&[("Docs/Report.pdf", false)]);
        assert!(!covers(&m, "Docs/Report.pdf/inside"));
    }

    #[test]
    fn the_root_pin_covers_everything() {
        let m = map(&[("", true)]);
        assert!(covers(&m, "anything/at/all.txt"));
        assert!(covers(&m, ""));
    }

    #[test]
    fn a_root_entry_that_is_not_a_directory_pin_covers_only_itself() {
        let m = map(&[("", false)]);
        assert!(covers(&m, ""));
        assert!(!covers(&m, "anything.txt"));
    }

    #[test]
    fn renaming_a_directory_carries_its_subtrees_pins() {
        let mut m = map(&[("Photos", true), ("Photos/2024/pic.jpg", false)]);
        rename_in(&mut m, "Photos", "Archive/Bilder");
        assert!(covers(&m, "Archive/Bilder/2024/new.jpg"));
        assert!(covers(&m, "Archive/Bilder/2024/pic.jpg"));
        assert!(
            !covers(&m, "Photos/2024/pic.jpg"),
            "the old path is vacated"
        );
    }

    #[test]
    fn renaming_onto_a_path_that_was_itself_pinned_keeps_one_answer() {
        let mut m = map(&[("A", true), ("B", false)]);
        rename_in(&mut m, "A", "B");
        assert!(covers(&m, "B/inside.txt"), "the moved pin wins");
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn the_root_pin_does_not_travel() {
        let mut m = map(&[("", true)]);
        rename_in(&mut m, "", "Somewhere");
        assert!(covers(&m, "anything"));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn removing_a_subtree_takes_the_pins_at_and_below_it() {
        let mut m = map(&[
            ("Photos", true),
            ("Photos/2024/pic.jpg", false),
            ("PhotosOld", true),
        ]);
        for p in under(&m, "Photos") {
            m.remove(&p);
        }
        assert_eq!(m.len(), 1, "only the lookalike survives: {m:?}");
        assert!(covers(&m, "PhotosOld/x"));
    }

    // --- the file, with one ---

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wusel-pins-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_account_with_no_file_has_no_pins() {
        let pins = Pins::new(&scratch("absent"));
        assert!(!pins.is_pinned("anything").unwrap());
        assert!(pins.all().unwrap().is_empty());
    }

    #[test]
    fn a_pin_survives_being_written_and_read_back() {
        let dir = scratch("roundtrip");
        let pins = Pins::new(&dir);
        pins.set("Photos", true).unwrap();
        pins.set("Docs/Report.pdf", false).unwrap();

        // A second handle on the same directory: what a `wusel pins` process
        // sees while the daemon is running.
        let other = Pins::new(&dir);
        assert!(other.is_pinned("Photos/2024/pic.jpg").unwrap());
        assert!(!other.is_pinned("Docs/Other.pdf").unwrap());
        assert_eq!(
            other.all().unwrap(),
            vec![
                ("Docs/Report.pdf".to_string(), false),
                ("Photos".to_string(), true)
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_change_from_elsewhere_is_picked_up_without_being_told() {
        let dir = scratch("reload");
        let daemon = Pins::new(&dir);
        let cli = Pins::new(&dir);
        assert!(!daemon.is_pinned("Photos").unwrap()); // loads and caches "empty"

        cli.set("Photos", true).unwrap();
        // The stamp differs, so the cached answer is discarded. Without that,
        // `wusel pin` would not take effect until the next mount.
        assert!(
            daemon.is_pinned("Photos/2024/pic.jpg").unwrap(),
            "a pin made by another process must reach the running daemon"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_removed_file_means_no_pins_rather_than_an_error() {
        let dir = scratch("deleted");
        let pins = Pins::new(&dir);
        pins.set("Photos", true).unwrap();
        std::fs::remove_file(pins.file()).unwrap();
        assert!(!pins.is_pinned("Photos").unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_corrupt_file_is_an_error_and_not_silently_no_pins() {
        let dir = scratch("corrupt");
        let pins = Pins::new(&dir);
        pins.set("Photos", true).unwrap();
        std::fs::write(pins.file(), b"{ this is not json").unwrap();
        assert!(
            pins.is_pinned("Photos").is_err(),
            "reading garbage as 'nothing is pinned' would unprotect everything"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_from_a_newer_version_is_refused_rather_than_truncated() {
        let dir = scratch("newer");
        let pins = Pins::new(&dir);
        std::fs::write(
            pins.file(),
            br#"{"version":99,"pins":[{"path":"Photos","dir":true}]}"#,
        )
        .unwrap();
        assert!(pins.is_pinned("Photos").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migration_happens_once_and_never_resurrects_a_removed_pin() {
        let dir = scratch("migrate");
        let pins = Pins::new(&dir);
        let from_db = vec![("Photos".to_string(), true), ("a.txt".to_string(), false)];

        assert_eq!(pins.migrate_from(&from_db).unwrap(), 2);
        assert!(pins.is_pinned("Photos/x").unwrap());

        // The user removes a pin; the database still has the old row, because
        // nothing writes to it any more.
        pins.remove("Photos").unwrap();
        assert_eq!(pins.migrate_from(&from_db).unwrap(), 0, "already migrated");
        assert!(
            !pins.is_pinned("Photos/x").unwrap(),
            "a second migration would undo the user's removal"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_lock_left_by_a_dead_process_is_taken_over() {
        let dir = scratch("stalelock");
        let pins = Pins::new(&dir);
        // What a killed `wusel pin` leaves behind.
        let lock = pins.file().with_extension("lock");
        std::fs::create_dir_all(&lock).unwrap();

        // A live holder is not displaced …
        assert!(
            !stale(&lock, std::time::Duration::from_secs(30)),
            "a lock taken a moment ago belongs to somebody"
        );
        // … but one past the limit is, and the wait is a second, not forever.
        let taken = Lock::take_after(pins.file(), std::time::Duration::ZERO);
        assert!(
            taken.is_ok(),
            "a dead holder must not block writes for ever"
        );
        drop(taken);

        pins.set("Photos", true).unwrap();
        assert!(pins.is_pinned("Photos").unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_live_lock_is_respected_until_it_is_released() {
        let dir = scratch("livelock");
        let pins = Pins::new(&dir);
        let held = Lock::take(pins.file()).expect("take the lock");
        // A second taker gives up after its retries rather than trampling it.
        assert!(
            Lock::take(pins.file()).is_err(),
            "two writers at once is exactly what the lock prevents"
        );
        drop(held);
        pins.set("Photos", true).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
