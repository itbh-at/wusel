// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Local state in SQLite.
//!
//! Core responsibilities:
//! * stable **inode↔path** mapping (FUSE speaks inodes, Nextcloud speaks paths)
//! * ETag and size cache per entry (change detection)
//!
//! Cache state (which files are local) deliberately lives *outside* this DB, in
//! sidecar files next to the blobs (see [`crate::content`]); the user-visible
//! availability state is derived in [`crate::provider::Provider::file_state`].
//!
//! The FUSE layer holds no state itself — everything lives here, transactionally.

use rusqlite::{Connection, OptionalExtension};

use crate::model::RemoteEntry;
use crate::Result;

/// Root inode per FUSE convention.
pub const ROOT_INODE: u64 = 1;

pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    /// Opens (or creates) the database and sets up the schema.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // The background syncer opens a second connection to this same file; WAL
        // allows concurrent readers with one writer, and a busy timeout makes the
        // two writers (FUSE thread and syncer) wait for each other instead of
        // erroring with SQLITE_BUSY.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    /// In-memory DB for tests.
    pub fn open_in_memory() -> Result<Self> {
        let db = Self {
            conn: Connection::open_in_memory()?,
        };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS nodes (
                inode     INTEGER PRIMARY KEY,
                parent    INTEGER NOT NULL,
                name      TEXT    NOT NULL,
                path      TEXT    NOT NULL UNIQUE,
                is_dir    INTEGER NOT NULL,
                size      INTEGER NOT NULL DEFAULT 0,
                etag      TEXT    NOT NULL DEFAULT '',
                mtime     INTEGER NOT NULL DEFAULT 0,
                file_id   INTEGER,
                hydration INTEGER NOT NULL DEFAULT 0,  -- legacy, unused (cache state lives in blob sidecars)
                sync      INTEGER NOT NULL DEFAULT 0,  -- legacy, unused (kept so old DBs open unchanged)
                children_loaded INTEGER NOT NULL DEFAULT 0,  -- has this dir been PROPFIND'd?
                loaded_at INTEGER NOT NULL DEFAULT 0,         -- unix seconds of the last listing
                permissions TEXT NOT NULL DEFAULT '',         -- oc:permissions letters
                UNIQUE(parent, name)
            );
            CREATE INDEX IF NOT EXISTS idx_nodes_parent ON nodes(parent);

            -- Pins ("always keep offline"): a path plus whether it is a directory.
            -- Path-based (not per-inode) so a pin survives reconciliation and a
            -- directory pin covers its whole subtree, including entries not yet
            -- listed. The empty path is the account root — pinning it means "all".
            CREATE TABLE IF NOT EXISTS pins (
                path   TEXT PRIMARY KEY,
                is_dir INTEGER NOT NULL DEFAULT 0
            );

            -- Create the root node idempotently.
            INSERT OR IGNORE INTO nodes(inode, parent, name, path, is_dir)
            VALUES (1, 1, '', '', 1);
            "#,
        )?;
        // Migration for databases created before `permissions` existed. The
        // state DB is regenerable, but this avoids a needless rebuild. Only the
        // *expected* "duplicate column name" error (the column already exists)
        // may be ignored — swallowing everything would hide a locked, corrupt or
        // read-only DB here and let it fail later, far from the cause.
        if let Err(e) = self.conn.execute(
            "ALTER TABLE nodes ADD COLUMN permissions TEXT NOT NULL DEFAULT ''",
            [],
        ) {
            if !e.to_string().contains("duplicate column name") {
                return Err(e.into());
            }
        }
        Ok(())
    }

    /// Reconciles a directory's children against a fresh PROPFIND: update/insert
    /// present entries, **delete** those no longer on the server (with their
    /// subtree), and record the listing time. A changed `etag` here is what makes
    /// the content cache re-fetch (it validates against this ETag).
    pub fn reconcile_children(
        &mut self,
        parent: u64,
        parent_path: &str,
        children: &[RemoteEntry],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for c in children {
            let name = c.path.rsplit('/').next().unwrap_or(&c.path);
            seen.insert(name.to_string());
            let path = if parent_path.is_empty() {
                c.path.clone()
            } else {
                format!("{parent_path}/{name}")
            };
            // A server-side type flip (a file replaced by a same-named directory,
            // or vice versa) is a genuinely different object — different file id,
            // and for a former directory a whole subtree that no longer exists.
            // The upsert below never updates `is_dir`, so drop the old row's
            // subtree first and let the insert create a fresh row. (SQLite may
            // hand the fresh row a recycled inode number; open handles on the
            // replaced object are stale either way.)
            let existing: Option<(u64, bool)> = tx
                .query_row(
                    "SELECT inode, is_dir FROM nodes
                     WHERE parent = ?1 AND name = ?2 AND inode != ?1",
                    rusqlite::params![parent, name],
                    |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? != 0)),
                )
                .optional()?;
            if let Some((inode, was_dir)) = existing {
                if was_dir != c.is_dir {
                    delete_subtree(&tx, inode)?;
                }
            }
            tx.execute(
                r#"INSERT INTO nodes(parent, name, path, is_dir, size, etag, mtime, file_id, permissions)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                   ON CONFLICT(parent, name) DO UPDATE SET
                       size=excluded.size, etag=excluded.etag,
                       mtime=excluded.mtime, file_id=excluded.file_id,
                       permissions=excluded.permissions"#,
                rusqlite::params![
                    parent,
                    name,
                    path,
                    c.is_dir as i64,
                    c.size as i64,
                    c.etag,
                    c.mtime,
                    c.file_id.map(|v| v as i64),
                    c.permissions,
                ],
            )?;
        }

        // Delete children that vanished from the server, together with their
        // subtree (iterative, by inode — no LIKE, so names with %/_ are safe).
        // A child with no `file_id` was never on the server — a deferred create
        // not yet flushed (see `insert_local_file`) — so the PROPFIND naturally
        // omits it; preserve it instead of deleting the file out from under the
        // editor that just created it. Once flushed, it gains a file id and
        // reconciles normally.
        let gone: Vec<u64> = {
            let mut stmt = tx.prepare(
                "SELECT inode, name, file_id FROM nodes WHERE parent = ?1 AND inode != ?1",
            )?;
            let rows = stmt.query_map([parent], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            })?;
            let mut v = Vec::new();
            for row in rows {
                let (inode, name, file_id) = row?;
                if !seen.contains(&name) && file_id.is_some() {
                    v.push(inode);
                }
            }
            v
        };
        for root in gone {
            delete_subtree(&tx, root)?;
        }

        tx.execute(
            "UPDATE nodes SET children_loaded = 1, loaded_at = ?2 WHERE inode = ?1",
            rusqlite::params![parent, now_secs() as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Insert a brand-new **local** file that does not yet exist on the server —
    /// a deferred create, materialised on the first flush. It has no file id and
    /// an empty ETag; `reconcile_children` preserves it (a `NULL` file id) until
    /// the upload gives it a server identity, at which point the next reconcile
    /// updates this same row (same inode) in place. Empty permissions ⇒ writable.
    pub fn insert_local_file(&mut self, parent: u64, name: &str) -> Result<NodeRow> {
        let parent_path: String =
            self.conn
                .query_row("SELECT path FROM nodes WHERE inode = ?1", [parent], |r| {
                    r.get(0)
                })?;
        let path = if parent_path.is_empty() {
            name.to_string()
        } else {
            format!("{parent_path}/{name}")
        };
        self.conn.execute(
            r#"INSERT INTO nodes(parent, name, path, is_dir, size, etag, mtime, file_id, permissions)
               VALUES (?1, ?2, ?3, 0, 0, '', ?4, NULL, '')"#,
            rusqlite::params![parent, name, path, now_secs() as i64],
        )?;
        let inode = self.conn.last_insert_rowid() as u64;
        self.node_by_inode(inode)?
            .ok_or_else(|| crate::Error::Other("insert_local_file: node vanished".into()))
    }

    /// Whether a directory should be (re-)listed. Reasons:
    /// * never loaded;
    /// * its last listing is older than `ttl_secs` (the no-push staleness bound);
    /// * it was listed at/before `invalidate_after` (a notify_push signal — pass
    ///   `0` when there is none) **and** its last listing is older than
    ///   `push_floor_secs`.
    ///
    /// The floor is the key throttle: notify_push events are coarse (path-less on
    /// most servers) and frequent, so without it a single change — or a background
    /// indexer walking the tree — would force a fresh PROPFIND on essentially every
    /// `stat`/`readdir`. Bounding push-triggered re-lists to one per floor window
    /// per directory collapses that storm while still reflecting changes within a
    /// few seconds. Drives revalidation in the provider.
    pub fn dir_needs_reload(
        &self,
        inode: u64,
        ttl_secs: u64,
        invalidate_after: i64,
        push_floor_secs: u64,
    ) -> Result<bool> {
        let row: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT children_loaded, loaded_at FROM nodes WHERE inode = ?1",
                [inode],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            None => Ok(true),
            Some((0, _)) => Ok(true),
            Some((_, loaded_at)) => {
                let age = now_secs().saturating_sub(loaded_at as u64);
                let pushed = loaded_at <= invalidate_after && age >= push_floor_secs;
                Ok(age >= ttl_secs || pushed)
            }
        }
    }

    /// Children of a directory (for `readdir`).
    pub fn children_of(&self, parent: u64) -> Result<Vec<NodeRow>> {
        let mut stmt = self.conn.prepare(SELECT_NODE_COLS_WHERE_PARENT)?;
        let rows = stmt
            .query_map([parent], NodeRow::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// A single node by inode (root included). `None` if unknown.
    pub fn node_by_inode(&self, inode: u64) -> Result<Option<NodeRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT inode, parent, name, path, is_dir, size, etag, mtime, file_id, permissions FROM nodes WHERE inode = ?1")?;
        Ok(stmt.query_row([inode], NodeRow::from_row).optional()?)
    }

    /// A child of `parent` by name (for `lookup`). `None` if not present.
    pub fn child_by_name(&self, parent: u64, name: &str) -> Result<Option<NodeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT inode, parent, name, path, is_dir, size, etag, mtime, file_id, permissions
             FROM nodes WHERE parent = ?1 AND name = ?2 AND inode != ?1",
        )?;
        Ok(stmt
            .query_row(rusqlite::params![parent, name], NodeRow::from_row)
            .optional()?)
    }

    /// Whether this directory has ever been listed (PROPFIND'd), i.e. its
    /// children are cached. Drives the lazy-PROPFIND decision: the provider
    /// loads a never-listed directory synchronously (there is nothing to serve
    /// yet) but revalidates an already-listed one in the background — no
    /// PROPFIND in the hot path. An unknown inode reads as "not listed".
    pub fn children_loaded(&self, inode: u64) -> Result<bool> {
        let loaded: Option<i64> = self
            .conn
            .query_row(
                "SELECT children_loaded FROM nodes WHERE inode = ?1",
                [inode],
                |r| r.get(0),
            )
            .optional()?;
        Ok(loaded.unwrap_or(0) != 0)
    }

    // --- Writing ------------------------------------------------------------

    /// Update a node's size (e.g. while a write grows the local scratch).
    pub fn set_size(&self, inode: u64, size: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE nodes SET size = ?2 WHERE inode = ?1",
            rusqlite::params![inode, size as i64],
        )?;
        Ok(())
    }

    /// Update a node's mtime (unix seconds), e.g. from a `setattr`.
    pub fn set_mtime(&self, inode: u64, mtime: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE nodes SET mtime = ?2 WHERE inode = ?1",
            rusqlite::params![inode, mtime],
        )?;
        Ok(())
    }

    /// Rename/move a **local** node (no children) within the tree: update its
    /// parent, name and path. Used to move a local-only file (a deferred create
    /// or an ignored file) before promoting it. Files only — local nodes never
    /// have a subtree.
    pub fn rename_node(&self, inode: u64, new_parent: u64, new_name: &str) -> Result<()> {
        let parent_path: String = self.conn.query_row(
            "SELECT path FROM nodes WHERE inode = ?1",
            [new_parent],
            |r| r.get(0),
        )?;
        let path = if parent_path.is_empty() {
            new_name.to_string()
        } else {
            format!("{parent_path}/{new_name}")
        };
        self.conn.execute(
            "UPDATE nodes SET parent = ?2, name = ?3, path = ?4 WHERE inode = ?1",
            rusqlite::params![inode, new_parent, new_name, path],
        )?;
        Ok(())
    }

    /// Move a node — and, for a directory, rewrite its whole subtree's `path`
    /// column — to a new parent/name, **keeping every inode**. Used after a
    /// server-side MOVE: reconcile matches rows by `(parent, name)`, so moving
    /// the row first lets the follow-up re-list update it in place. Letting
    /// reconcile delete the old row and insert a fresh one instead would orphan
    /// open file handles and any pending write buffer keyed by the old inode.
    /// Descendant paths are rewritten iteratively via the parent links (no
    /// `LIKE` on the old prefix, so names containing `%`/`_` are safe).
    pub fn move_subtree(&mut self, inode: u64, new_parent: u64, new_name: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        let parent_path: String = tx.query_row(
            "SELECT path FROM nodes WHERE inode = ?1",
            [new_parent],
            |r| r.get(0),
        )?;
        let path = if parent_path.is_empty() {
            new_name.to_string()
        } else {
            format!("{parent_path}/{new_name}")
        };
        tx.execute(
            "UPDATE nodes SET parent = ?2, name = ?3, path = ?4 WHERE inode = ?1",
            rusqlite::params![inode, new_parent, new_name, path],
        )?;
        let mut stack = vec![(inode, path)];
        while let Some((dir, dir_path)) = stack.pop() {
            let children: Vec<(u64, String, bool)> = {
                let mut stmt = tx.prepare(
                    "SELECT inode, name, is_dir FROM nodes WHERE parent = ?1 AND inode != ?1",
                )?;
                let rows = stmt.query_map([dir], |r| {
                    Ok((
                        r.get::<_, i64>(0)? as u64,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)? != 0,
                    ))
                })?;
                let mut v = Vec::new();
                for row in rows {
                    v.push(row?);
                }
                v
            };
            for (child, name, is_dir) in children {
                let child_path = format!("{dir_path}/{name}");
                tx.execute(
                    "UPDATE nodes SET path = ?2 WHERE inode = ?1",
                    rusqlite::params![child, child_path],
                )?;
                if is_dir {
                    stack.push((child, child_path));
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete every file node that was never materialised on the server (no file
    /// id). Called at startup, alongside wiping the scratch dir: a local-only file
    /// (a deferred create or an ignored file) whose write buffer is gone is dead,
    /// and would otherwise linger forever (reconcile deliberately preserves
    /// file-id-less nodes). Directories are untouched (they always have a file id
    /// once listed; the root has none but is excluded).
    pub fn remove_unmaterialized_files(&self) -> Result<()> {
        self.conn.execute(
            "DELETE FROM nodes WHERE file_id IS NULL AND is_dir = 0 AND inode != ?1",
            [ROOT_INODE],
        )?;
        Ok(())
    }

    /// Remove a node and its entire subtree (after a server-side delete/rename).
    pub fn remove_subtree(&mut self, inode: u64) -> Result<()> {
        let tx = self.conn.transaction()?;
        delete_subtree(&tx, inode)?;
        tx.commit()?;
        Ok(())
    }

    /// Number of known nodes (root included) — a cheap cache-size indicator.
    pub fn node_count(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// A single node by its full remote path (`""` = the root). `None` if unknown.
    pub fn node_by_path(&self, path: &str) -> Result<Option<NodeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT inode, parent, name, path, is_dir, size, etag, mtime, file_id, permissions
             FROM nodes WHERE path = ?1",
        )?;
        Ok(stmt
            .query_row([path.trim_matches('/')], NodeRow::from_row)
            .optional()?)
    }

    /// Forget a directory's cached listing, recursively: delete every descendant
    /// row and mark the directory itself as never listed, so the next access
    /// re-lists from the server. The directory's own row survives — it is still
    /// a real server object; only our knowledge of its contents is dropped.
    /// Returns the number of forgotten nodes. (Diagnostic aid: `cache clear`.)
    pub fn forget_children(&mut self, inode: u64) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let kids: Vec<u64> = {
            let mut stmt =
                tx.prepare("SELECT inode FROM nodes WHERE parent = ?1 AND inode != ?1")?;
            let kids = stmt
                .query_map([inode], |r| r.get::<_, i64>(0).map(|v| v as u64))?
                .collect::<std::result::Result<_, _>>()?;
            kids
        };
        let mut dropped = 0;
        for kid in kids {
            dropped += delete_subtree(&tx, kid)?;
        }
        tx.execute(
            "UPDATE nodes SET children_loaded = 0, loaded_at = 0 WHERE inode = ?1",
            [inode],
        )?;
        tx.commit()?;
        Ok(dropped)
    }

    /// Record the server's new ETag and size after a successful upload, so the
    /// content cache stays valid and change detection does not re-download it.
    pub fn set_etag_size(&self, inode: u64, etag: &str, size: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE nodes SET etag = ?2, size = ?3 WHERE inode = ?1",
            rusqlite::params![inode, etag, size as i64],
        )?;
        Ok(())
    }

    // --- Pins ("always keep offline") ---------------------------------------

    /// Record a pin on `path` (empty string = the account root).
    pub fn set_pin(&self, path: &str, is_dir: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pins(path, is_dir) VALUES(?1, ?2)
             ON CONFLICT(path) DO UPDATE SET is_dir = excluded.is_dir",
            rusqlite::params![path.trim_matches('/'), is_dir as i64],
        )?;
        Ok(())
    }

    /// Remove a pin on exactly `path` (does not touch pins on its subtree).
    pub fn remove_pin(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM pins WHERE path = ?1", [path.trim_matches('/')])?;
        Ok(())
    }

    /// Whether `path` is kept offline: pinned itself, under a pinned directory,
    /// or covered by a root pin.
    pub fn is_pinned(&self, path: &str) -> Result<bool> {
        let path = path.trim_matches('/');
        let mut stmt = self.conn.prepare("SELECT path, is_dir FROM pins")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0))
        })?;
        for row in rows {
            let (pin, is_dir) = row?;
            if pin == path {
                return Ok(true);
            }
            // A directory pin covers its subtree; the root pin ("") covers all.
            if is_dir && (pin.is_empty() || path.starts_with(&format!("{pin}/"))) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// All pins as `(path, is_dir)`.
    pub fn pins(&self) -> Result<Vec<(String, bool)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, is_dir FROM pins ORDER BY path")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// `(path, file_id)` for known nodes at/under `path` that have a file id.
    /// Filtered by path component in Rust (no `LIKE`, so `%`/`_` are safe).
    pub fn descendant_file_ids(&self, path: &str) -> Result<Vec<(String, u64)>> {
        let path = path.trim_matches('/');
        let prefix = format!("{path}/");
        let mut stmt = self
            .conn
            .prepare("SELECT path, file_id FROM nodes WHERE file_id IS NOT NULL")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (p, fid) = row?;
            if path.is_empty() || p == path || p.starts_with(&prefix) {
                out.push((p, fid));
            }
        }
        Ok(out)
    }
}

const SELECT_NODE_COLS_WHERE_PARENT: &str =
    "SELECT inode, parent, name, path, is_dir, size, etag, mtime, file_id, permissions
     FROM nodes WHERE parent = ?1 AND inode != ?1";

/// Delete `inode` and its entire subtree, iteratively and by inode (no `LIKE`,
/// so file names containing `%` or `_` are safe). Returns the rows deleted.
fn delete_subtree(tx: &rusqlite::Transaction, inode: u64) -> Result<usize> {
    // Prepare the child query once; re-parsing the same SQL per visited node
    // would only add avoidable work to a loop that may walk a large subtree.
    let mut stmt = tx.prepare("SELECT inode FROM nodes WHERE parent = ?1 AND inode != ?1")?;
    let mut stack = vec![inode];
    let mut to_delete = Vec::new();
    while let Some(ino) = stack.pop() {
        to_delete.push(ino);
        let kids: Vec<u64> = stmt
            .query_map([ino], |r| r.get::<_, i64>(0).map(|v| v as u64))?
            .collect::<std::result::Result<_, _>>()?;
        stack.extend(kids);
    }
    for ino in &to_delete {
        tx.execute("DELETE FROM nodes WHERE inode = ?1", [ino])?;
    }
    Ok(to_delete.len())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A row from `nodes`, as the FUSE layer needs it.
#[derive(Debug, Clone)]
pub struct NodeRow {
    pub inode: u64,
    pub parent: u64,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub etag: String,
    pub mtime: i64,
    /// Nextcloud's stable file id — a good cache key (survives renames).
    pub file_id: Option<u64>,
    /// Raw `oc:permissions` letters (empty if the server did not report them).
    pub permissions: String,
}

impl NodeRow {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(NodeRow {
            inode: row.get::<_, i64>(0)? as u64,
            parent: row.get::<_, i64>(1)? as u64,
            name: row.get(2)?,
            path: row.get(3)?,
            is_dir: row.get::<_, i64>(4)? != 0,
            size: row.get::<_, i64>(5)? as u64,
            etag: row.get(6)?,
            mtime: row.get(7)?,
            file_id: row.get::<_, Option<i64>>(8)?.map(|v| v as u64),
            permissions: row.get(9)?,
        })
    }

    /// Whether the server's permissions allow modifying this entry. Drives the
    /// read-only bits in the FUSE mode and, later, write gating (Priority 7).
    pub fn is_writable(&self) -> bool {
        crate::model::is_writable(&self.permissions, self.is_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, is_dir: bool) -> RemoteEntry {
        RemoteEntry {
            path: path.into(),
            is_dir,
            size: 10,
            etag: "e".into(),
            mtime: 0,
            file_id: Some(1),
            permissions: String::new(),
        }
    }

    #[test]
    fn upsert_and_list_children() {
        let mut db = StateDb::open_in_memory().unwrap();
        db.reconcile_children(
            ROOT_INODE,
            "",
            &[entry("Notes.txt", false), entry("Photos", true)],
        )
        .unwrap();

        let kids = db.children_of(ROOT_INODE).unwrap();
        assert_eq!(kids.len(), 2);
        assert!(kids.iter().any(|k| k.name == "Notes.txt" && !k.is_dir));
        assert!(kids.iter().any(|k| k.name == "Photos" && k.is_dir));

        // Lookups the FUSE layer relies on.
        let notes = db.child_by_name(ROOT_INODE, "Notes.txt").unwrap().unwrap();
        assert!(!notes.is_dir);
        let same = db.node_by_inode(notes.inode).unwrap().unwrap();
        assert_eq!(same.name, "Notes.txt");
        assert!(db
            .child_by_name(ROOT_INODE, "does-not-exist")
            .unwrap()
            .is_none());

        // Root is now marked as listed; a fresh directory is not.
        assert!(db.children_loaded(ROOT_INODE).unwrap());
        let photos = db.child_by_name(ROOT_INODE, "Photos").unwrap().unwrap();
        assert!(!db.children_loaded(photos.inode).unwrap());
    }

    #[test]
    fn forget_children_clears_the_listing_but_keeps_the_directory() {
        let mut db = StateDb::open_in_memory().unwrap();
        db.reconcile_children(
            ROOT_INODE,
            "",
            &[entry("Notes.txt", false), entry("Photos", true)],
        )
        .unwrap();
        let photos = db.node_by_path("Photos").unwrap().unwrap();
        db.reconcile_children(
            photos.inode,
            "Photos",
            &[entry("Photos/Birdie.jpg", false), entry("Photos/Raw", true)],
        )
        .unwrap();

        // Forget only Photos: its listing is gone and it is "never listed"
        // again, but the directory itself and the rest of the tree survive.
        let dropped = db.forget_children(photos.inode).unwrap();
        assert_eq!(dropped, 2, "Birdie.jpg + Raw");
        assert!(db.children_of(photos.inode).unwrap().is_empty());
        assert!(
            !db.children_loaded(photos.inode).unwrap(),
            "re-list next time"
        );
        assert!(db.node_by_path("Photos").unwrap().is_some(), "dir row kept");
        assert!(
            db.node_by_path("Notes.txt").unwrap().is_some(),
            "sibling kept"
        );
        assert!(db.children_loaded(ROOT_INODE).unwrap(), "root untouched");

        // Forgetting the root drops everything below it.
        let dropped = db.forget_children(ROOT_INODE).unwrap();
        assert_eq!(dropped, 2, "Notes.txt + Photos");
        assert!(!db.children_loaded(ROOT_INODE).unwrap());
        assert!(db.node_by_path("Photos").unwrap().is_none());
    }

    #[test]
    fn move_subtree_keeps_inodes_and_rewrites_descendant_paths() {
        let mut db = StateDb::open_in_memory().unwrap();
        db.reconcile_children(
            ROOT_INODE,
            "",
            &[entry("Docs", true), entry("Archive", true)],
        )
        .unwrap();
        let docs = db.node_by_path("Docs").unwrap().unwrap();
        let archive = db.node_by_path("Archive").unwrap().unwrap();
        db.reconcile_children(
            docs.inode,
            "Docs",
            &[entry("Docs/a.txt", false), entry("Docs/Sub", true)],
        )
        .unwrap();
        let sub = db.node_by_path("Docs/Sub").unwrap().unwrap();
        db.reconcile_children(sub.inode, "Docs/Sub", &[entry("Docs/Sub/deep.txt", false)])
            .unwrap();
        let a = db.node_by_path("Docs/a.txt").unwrap().unwrap();
        let deep = db.node_by_path("Docs/Sub/deep.txt").unwrap().unwrap();

        // Move Docs → Archive/Papers: every inode survives, every path follows.
        db.move_subtree(docs.inode, archive.inode, "Papers")
            .unwrap();
        let moved = db.node_by_inode(docs.inode).unwrap().unwrap();
        assert_eq!(
            (moved.parent, moved.name.as_str()),
            (archive.inode, "Papers")
        );
        assert_eq!(moved.path, "Archive/Papers");
        let a2 = db.node_by_path("Archive/Papers/a.txt").unwrap().unwrap();
        assert_eq!(a2.inode, a.inode, "file inode survives the move");
        let deep2 = db
            .node_by_path("Archive/Papers/Sub/deep.txt")
            .unwrap()
            .unwrap();
        assert_eq!(deep2.inode, deep.inode, "nested inode survives the move");
        assert!(db.node_by_path("Docs").unwrap().is_none(), "old path gone");

        // The follow-up reconcile (the re-list after a MOVE) must now update
        // the moved row in place instead of replacing it.
        db.reconcile_children(archive.inode, "Archive", &[entry("Archive/Papers", true)])
            .unwrap();
        let after = db.node_by_path("Archive/Papers").unwrap().unwrap();
        assert_eq!(after.inode, docs.inode, "reconcile keeps the moved inode");
    }

    #[test]
    fn permissions_persist_and_drive_writability() {
        let mut db = StateDb::open_in_memory().unwrap();
        let writable = RemoteEntry {
            path: "notes.txt".into(),
            is_dir: false,
            size: 1,
            etag: "e".into(),
            mtime: 0,
            file_id: Some(1),
            permissions: "RGDNVW".into(),
        };
        let readonly = RemoteEntry {
            path: "readonly.txt".into(),
            permissions: "GR".into(),
            ..writable.clone()
        };
        db.reconcile_children(ROOT_INODE, "", &[writable, readonly])
            .unwrap();

        let w = db.child_by_name(ROOT_INODE, "notes.txt").unwrap().unwrap();
        assert_eq!(w.permissions, "RGDNVW");
        assert!(w.is_writable());

        let r = db
            .child_by_name(ROOT_INODE, "readonly.txt")
            .unwrap()
            .unwrap();
        assert!(!r.is_writable(), "no W letter → not writable");
    }

    #[test]
    fn reconcile_deletes_gone_entries_with_subtree() {
        let mut db = StateDb::open_in_memory().unwrap();
        // Root has Photos/; Photos has a child.
        db.reconcile_children(ROOT_INODE, "", &[entry("Photos", true)])
            .unwrap();
        let photos = db.child_by_name(ROOT_INODE, "Photos").unwrap().unwrap();
        db.reconcile_children(photos.inode, "Photos", &[entry("Photos/pic.jpg", false)])
            .unwrap();
        assert_eq!(db.children_of(photos.inode).unwrap().len(), 1);

        // A later listing of root no longer has Photos → it and its subtree go.
        db.reconcile_children(ROOT_INODE, "", &[entry("Notes.txt", false)])
            .unwrap();
        assert!(db.child_by_name(ROOT_INODE, "Photos").unwrap().is_none());
        assert!(db.node_by_inode(photos.inode).unwrap().is_none());
        // The grandchild is gone too (subtree deleted) — its unique path is free again.
        db.reconcile_children(
            ROOT_INODE,
            "",
            &[entry("Notes.txt", false), entry("Photos", true)],
        )
        .unwrap();
        let photos2 = db.child_by_name(ROOT_INODE, "Photos").unwrap().unwrap();
        db.reconcile_children(photos2.inode, "Photos", &[entry("Photos/pic.jpg", false)])
            .unwrap();
        assert_eq!(db.children_of(photos2.inode).unwrap().len(), 1);
    }

    #[test]
    fn reconcile_replaces_a_row_on_a_type_flip() {
        let mut db = StateDb::open_in_memory().unwrap();
        // "Report" starts as a directory with a child.
        db.reconcile_children(ROOT_INODE, "", &[entry("Report", true)])
            .unwrap();
        let dir = db.child_by_name(ROOT_INODE, "Report").unwrap().unwrap();
        db.reconcile_children(dir.inode, "Report", &[entry("Report/draft.txt", false)])
            .unwrap();
        let draft = db.node_by_path("Report/draft.txt").unwrap().unwrap();

        // The server replaced the directory with a same-named FILE. The upsert
        // must not leave the row marked as a directory (with a phantom subtree);
        // a type flip is a different object and gets a fresh row. (The fresh
        // row's inode *number* may be recycled by SQLite's rowid allocation, so
        // the observable guarantees are the type and the vanished subtree.)
        db.reconcile_children(ROOT_INODE, "", &[entry("Report", false)])
            .unwrap();
        let file = db.child_by_name(ROOT_INODE, "Report").unwrap().unwrap();
        assert!(!file.is_dir, "the local type follows the server's");
        assert!(
            db.node_by_inode(draft.inode).unwrap().is_none(),
            "the former directory's subtree is gone"
        );

        // And the reverse flip: file → directory.
        db.reconcile_children(ROOT_INODE, "", &[entry("Report", true)])
            .unwrap();
        let dir2 = db.child_by_name(ROOT_INODE, "Report").unwrap().unwrap();
        assert!(dir2.is_dir, "file → directory flips back too");
    }

    #[test]
    fn pins_cover_files_dirs_subtrees_and_root() {
        let db = StateDb::open_in_memory().unwrap();

        // A single file pin matches only that file.
        db.set_pin("Docs/Report.pdf", false).unwrap();
        assert!(db.is_pinned("Docs/Report.pdf").unwrap());
        assert!(!db.is_pinned("Docs/Other.pdf").unwrap());

        // A directory pin covers the whole subtree, but not siblings.
        db.set_pin("Photos", true).unwrap();
        assert!(db.is_pinned("Photos").unwrap());
        assert!(db.is_pinned("Photos/2024/pic.jpg").unwrap());
        assert!(
            !db.is_pinned("PhotosOld/pic.jpg").unwrap(),
            "prefix must be a path component"
        );

        // Removing a pin takes effect.
        db.remove_pin("Photos").unwrap();
        assert!(!db.is_pinned("Photos/2024/pic.jpg").unwrap());

        // The root pin ("") is the legacy "download everything".
        db.set_pin("", true).unwrap();
        assert!(db.is_pinned("anything/at/all.txt").unwrap());

        assert_eq!(db.pins().unwrap().len(), 2, "Docs/Report.pdf and root");
    }

    #[test]
    fn dir_needs_reload_respects_ttl() {
        let mut db = StateDb::open_in_memory().unwrap();
        assert!(
            db.dir_needs_reload(ROOT_INODE, 10, 0, 0).unwrap(),
            "never loaded → reload"
        );
        db.reconcile_children(ROOT_INODE, "", &[entry("a.txt", false)])
            .unwrap();
        assert!(
            !db.dir_needs_reload(ROOT_INODE, 3600, 0, 0).unwrap(),
            "just loaded, long TTL → no reload"
        );
        assert!(
            db.dir_needs_reload(ROOT_INODE, 0, 0, 0).unwrap(),
            "zero TTL → always reload"
        );
    }

    #[test]
    fn dir_needs_reload_honours_push_invalidation() {
        let mut db = StateDb::open_in_memory().unwrap();
        db.reconcile_children(ROOT_INODE, "", &[entry("a.txt", false)])
            .unwrap();
        // Fresh within the TTL and no push → no reload.
        assert!(!db.dir_needs_reload(ROOT_INODE, 3600, 0, 0).unwrap());
        // A notify_push stamped "now or later" forces a re-list despite the TTL
        // (no floor).
        let future = now_secs() as i64 + 5;
        assert!(
            db.dir_needs_reload(ROOT_INODE, 3600, future, 0).unwrap(),
            "loaded_at <= invalidate_after → reload"
        );
    }

    #[test]
    fn push_floor_throttles_reloads_of_a_fresh_dir() {
        let mut db = StateDb::open_in_memory().unwrap();
        db.reconcile_children(ROOT_INODE, "", &[entry("a.txt", false)])
            .unwrap();
        let future = now_secs() as i64 + 5;
        // Just loaded (age ~0): a push cannot force a re-list inside the floor.
        assert!(
            !db.dir_needs_reload(ROOT_INODE, 3600, future, 60).unwrap(),
            "push invalidation within the floor window is suppressed"
        );
        // With no floor, the same push does force a re-list.
        assert!(db.dir_needs_reload(ROOT_INODE, 3600, future, 0).unwrap());
    }
}
