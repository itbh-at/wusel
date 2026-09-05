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
use wusel_fsm::ObjectId;

use crate::model::{basename, RemoteEntry};
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

    /// Attach another connection to a database that already exists.
    ///
    /// [`Self::open`] ends in `init_schema`, which is DDL and therefore needs
    /// the write lock. That is right for the one connection that establishes
    /// the database and wrong for every later one: a worker opened that way
    /// cannot start while anybody holds a write transaction, and would sit out
    /// the busy timeout to run statements whose only effect is `IF NOT EXISTS`.
    ///
    /// `journal_mode` is left alone for the same reason — it is a property of
    /// the file, set by whoever created it, and setting it again is at best a
    /// no-op and at worst a write.
    ///
    /// # Errors
    /// If the database cannot be opened.
    pub fn open_existing(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self { conn })
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
                group_root  INTEGER NOT NULL DEFAULT 0,       -- root of a Team/Group folder?
                UNIQUE(parent, name)
            );
            CREATE INDEX IF NOT EXISTS idx_nodes_parent ON nodes(parent);

            -- Local changes committed on close but not yet on the server, so an
            -- asynchronous upload survives a crash or restart. Keyed by the
            -- object (its value is the node inode), with the upload target stored
            -- *explicitly* rather than re-derived — a later rename moves this
            -- path, and re-walking the tree at upload time could disagree.
            CREATE TABLE IF NOT EXISTS pending_uploads (
                object_id   INTEGER PRIMARY KEY,
                remote_path TEXT    NOT NULL,
                base_etag   TEXT    NOT NULL DEFAULT '',  -- precondition; '' = must-not-exist
                mtime       INTEGER,                      -- X-OC-Mtime to send, if set
                state       TEXT    NOT NULL DEFAULT 'pending',  -- pending | uploading | error
                attempts    INTEGER NOT NULL DEFAULT 0,
                last_error  TEXT
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
        for stmt in [
            "ALTER TABLE nodes ADD COLUMN permissions TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE nodes ADD COLUMN group_root INTEGER NOT NULL DEFAULT 0",
        ] {
            if let Err(e) = self.conn.execute(stmt, []) {
                if !e.to_string().contains("duplicate column name") {
                    return Err(e.into());
                }
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
            let name = basename(&c.path);
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
                r#"INSERT INTO nodes(parent, name, path, is_dir, size, etag, mtime, file_id, permissions, group_root)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                   ON CONFLICT(parent, name) DO UPDATE SET
                       size=excluded.size, etag=excluded.etag,
                       mtime=excluded.mtime, file_id=excluded.file_id,
                       permissions=excluded.permissions,
                       group_root=excluded.group_root"#,
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
                    // Derived here, once, rather than storing both ingredients:
                    // "is this a group folder's root" is the only question
                    // anything downstream asks.
                    crate::model::is_group_folder_root(&c.mount_type, c.is_mount_root) as i64,
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
            r#"INSERT INTO nodes(parent, name, path, is_dir, size, etag, mtime, file_id, permissions, group_root)
               VALUES (?1, ?2, ?3, 0, 0, '', ?4, NULL, '', 0)"#,
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
            .prepare("SELECT inode, parent, name, path, is_dir, size, etag, mtime, file_id, permissions, group_root FROM nodes WHERE inode = ?1")?;
        Ok(stmt.query_row([inode], NodeRow::from_row).optional()?)
    }

    /// A child of `parent` by name (for `lookup`). `None` if not present.
    pub fn child_by_name(&self, parent: u64, name: &str) -> Result<Option<NodeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT inode, parent, name, path, is_dir, size, etag, mtime, file_id, permissions, group_root
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
    ///
    /// Returns `(old_path, new_path)`: pins are keyed by path and live outside
    /// this database ([`crate::pins`]), so the caller is the one that can carry
    /// them over — a rename must not quietly end a "keep offline" promise. A
    /// move **into the node's own subtree** is refused (see `descends_from`).
    pub fn move_subtree(
        &mut self,
        inode: u64,
        new_parent: u64,
        new_name: &str,
    ) -> Result<(String, String)> {
        let tx = self.conn.transaction()?;
        // A cyclic parent link would make the rewrite walk below push children
        // forever (it would never pop its way out), so this is not a nicety: it
        // is the difference between an error and an unkillable, memory-eating
        // loop. The kernel's own `vfs_rename` rejects such a rename, so we only
        // ever see one from a frontend that does not (or from a direct caller).
        if descends_from(&tx, new_parent, inode)? {
            return Err(crate::Error::Other(format!(
                "move_subtree: refusing to move inode {inode} into its own subtree"
            )));
        }
        let old_path: String =
            tx.query_row("SELECT path FROM nodes WHERE inode = ?1", [inode], |r| {
                r.get(0)
            })?;
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
        // Kept for the answer: the subtree rewrite below consumes `path`.
        let new_path = path.clone();
        // `rename(2)` replaces its destination, and `nodes` has UNIQUE(parent,
        // name) — so an occupant has to go first or the UPDATE below fails the
        // constraint. That is not an exotic case: every atomic save (GNOME's
        // .goutputstream-*, LibreOffice's temporaries) renames onto the file
        // being saved, and the failure surfaced as EIO on Ctrl+S.
        let occupant: Option<(u64, Option<u64>, String, String)> = tx
            .query_row(
                "SELECT inode, file_id, etag, permissions FROM nodes
                 WHERE parent = ?1 AND name = ?2 AND inode != ?3",
                rusqlite::params![new_parent, new_name, inode],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if let Some((old_inode, file_id, etag, permissions)) = &occupant {
            delete_subtree(&tx, *old_inode)?;
            let _ = (file_id, etag, permissions);
        }
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
        // Whoever replaces a file takes over its identity on the server: same
        // remote resource, new contents. Only when the mover has none of its
        // own — a server-side MOVE has already settled that case, and the
        // resource that survives there is the one that moved.
        //
        // Without this, an atomic save uploads as a *creation*: the machine
        // asserts "must not exist", the server answers 412 because the file it
        // is replacing is still there, and the user's save lands in a
        // "(conflicted copy)" instead of in their document. Inheriting the ETag
        // turns it into the ordinary overwrite it is — while still asserting
        // something, so a genuine server-side change in between is still caught.
        if let Some((_, file_id, etag, permissions)) = occupant {
            tx.execute(
                "UPDATE nodes SET file_id = ?2, etag = ?3, permissions = ?4
                 WHERE inode = ?1 AND file_id IS NULL",
                rusqlite::params![inode, file_id, etag, permissions],
            )?;
        }
        tx.commit()?;
        Ok((old_path, new_path))
    }

    /// Delete every file node that was never materialised on the server (no file
    /// id). Called at startup, alongside wiping the scratch dir: a local-only file
    /// (a deferred create or an ignored file) whose write buffer is gone is dead,
    /// and would otherwise linger forever (reconcile deliberately preserves
    /// file-id-less nodes). Directories are untouched (they always have a file id
    /// once listed; the root has none but is excluded).
    pub fn remove_unmaterialized_files(&self) -> Result<()> {
        // A never-materialised file with a pending upload is a deferred create
        // committed on close but not yet sent — the async write-back must keep
        // it, or the change is lost at the next start.
        self.conn.execute(
            "DELETE FROM nodes
             WHERE file_id IS NULL AND is_dir = 0 AND inode != ?1
               AND inode NOT IN (SELECT object_id FROM pending_uploads)",
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
            "SELECT inode, parent, name, path, is_dir, size, etag, mtime, file_id, permissions, group_root
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

    // --- Pending uploads (asynchronous write-back) --------------------------

    /// Record that an object has local content to upload, replacing any earlier
    /// record for it. Resets the state to `pending` and the attempt count: this
    /// is a fresh commit (a new close), so whatever the last attempt was, the
    /// bytes have changed and the uploader should start over.
    pub fn mark_pending_upload(
        &self,
        object: ObjectId,
        remote_path: &str,
        base_etag: &str,
        mtime: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pending_uploads(object_id, remote_path, base_etag, mtime, state, attempts, last_error)
             VALUES (?1, ?2, ?3, ?4, 'pending', 0, NULL)
             ON CONFLICT(object_id) DO UPDATE SET
                 remote_path = excluded.remote_path,
                 base_etag   = excluded.base_etag,
                 mtime       = excluded.mtime,
                 state       = 'pending',
                 attempts    = 0,
                 last_error  = NULL",
            rusqlite::params![object.0 as i64, remote_path, base_etag, mtime],
        )?;
        Ok(())
    }

    /// Every pending upload, for the uploader to work through and for resume at
    /// start-up. Ordered by object so the walk is deterministic.
    pub fn pending_uploads(&self) -> Result<Vec<PendingUpload>> {
        let mut stmt = self.conn.prepare(
            "SELECT object_id, remote_path, base_etag, mtime, state, attempts, last_error
             FROM pending_uploads ORDER BY object_id",
        )?;
        let rows = stmt.query_map([], PendingUpload::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One object's pending upload, if it has one.
    pub fn pending_upload(&self, object: ObjectId) -> Result<Option<PendingUpload>> {
        Ok(self
            .conn
            .query_row(
                "SELECT object_id, remote_path, base_etag, mtime, state, attempts, last_error
                 FROM pending_uploads WHERE object_id = ?1",
                rusqlite::params![object.0 as i64],
                PendingUpload::from_row,
            )
            .optional()?)
    }

    /// Move an object's state, recording (or clearing) the last error with it.
    pub fn set_upload_state(
        &self,
        object: ObjectId,
        state: UploadState,
        last_error: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_uploads SET state = ?2, last_error = ?3 WHERE object_id = ?1",
            rusqlite::params![object.0 as i64, state.as_str(), last_error],
        )?;
        Ok(())
    }

    /// Count a failed attempt, so a backoff has something to grow on.
    pub fn bump_upload_attempt(&self, object: ObjectId) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_uploads SET attempts = attempts + 1 WHERE object_id = ?1",
            rusqlite::params![object.0 as i64],
        )?;
        Ok(())
    }

    /// Follow a rename: the same bytes now belong at a new path. This is what
    /// keeps the office-suite atomic save correct — the buffer committed under
    /// the temporary name uploads to the document it was renamed onto.
    pub fn move_pending_upload(&self, object: ObjectId, new_remote_path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_uploads SET remote_path = ?2 WHERE object_id = ?1",
            rusqlite::params![object.0 as i64, new_remote_path],
        )?;
        Ok(())
    }

    /// The upload landed (or the object was removed): forget the record.
    pub fn clear_pending_upload(&self, object: ObjectId) -> Result<()> {
        self.conn.execute(
            "DELETE FROM pending_uploads WHERE object_id = ?1",
            rusqlite::params![object.0 as i64],
        )?;
        Ok(())
    }

    /// Drop pending uploads whose node no longer exists, returning how many went.
    ///
    /// A removed node's upload is now cleared with the node ([`delete_subtree`]),
    /// but a database written before that fix can still hold such ghosts — rows
    /// the uploader retries forever with no buffer to send, showing as permanent
    /// "waiting" uploads in `wusel status`. A periodic sweep heals them.
    pub fn remove_orphaned_uploads(&self) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM pending_uploads
             WHERE object_id NOT IN (SELECT inode FROM nodes)",
            [],
        )?;
        Ok(n)
    }

    // --- Pins ("always keep offline") ---------------------------------------

    /// The pins of a database written before they moved into their own file.
    ///
    /// Only the migration calls this. A database created by this version has no
    /// such table at all, which is not an error but the ordinary answer: there
    /// is nothing to take over.
    ///
    /// # Errors
    /// If the table exists but cannot be read.
    pub fn legacy_pins(&self) -> Result<Vec<(String, bool)>> {
        let exists: bool = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'pins'",
            [],
            |r| Ok(r.get::<_, i64>(0)? > 0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
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

    /// The node a cache blob belongs to, by its server file id.
    ///
    /// Used to turn an eviction — which knows only the blob's name — back into
    /// something a person can be shown. Rare enough that a scan would do; the
    /// column is indexed anyway because reconcile matches on it.
    ///
    /// # Errors
    /// If the state cannot be read.
    pub fn node_by_file_id(&self, file_id: u64) -> Result<Option<NodeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT inode, parent, name, path, is_dir, size, etag, mtime, file_id, permissions, group_root
             FROM nodes WHERE file_id = ?1",
        )?;
        Ok(stmt.query_row([file_id], NodeRow::from_row).optional()?)
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
    "SELECT inode, parent, name, path, is_dir, size, etag, mtime, file_id, permissions, group_root
     FROM nodes WHERE parent = ?1 AND inode != ?1";

/// How far up the parent chain [`descends_from`] is willing to walk. Far beyond
/// any real nesting; a chain longer than this is already corrupt, and treating
/// it as "cyclic" is the safe answer (it refuses the move instead of looping).
const MAX_ANCESTOR_WALK: u32 = 1024;

/// Whether `inode` is `ancestor` or lies below it, walking the parent links up.
/// The root is its own parent, which is where the walk stops.
fn descends_from(tx: &rusqlite::Transaction, inode: u64, ancestor: u64) -> Result<bool> {
    let mut cur = inode;
    for _ in 0..MAX_ANCESTOR_WALK {
        if cur == ancestor {
            return Ok(true);
        }
        let parent: Option<u64> = tx
            .query_row("SELECT parent FROM nodes WHERE inode = ?1", [cur], |r| {
                r.get::<_, i64>(0).map(|v| v as u64)
            })
            .optional()?;
        match parent {
            // A node that is its own parent is the root — the walk is done.
            Some(p) if p != cur => cur = p,
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// Pins at or below `path` as `(path, is_dir)`. Filtered by **path component**
/// in Rust (`Photos` must not catch `Photos2`) and without `LIKE`, so names
/// containing `%`/`_` are safe. `path` must be non-empty and already trimmed.
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
        // A removed node owes no upload. Drop any so it cannot linger as a ghost
        // "waiting" entry the uploader retries forever with no buffer to send
        // (which showed up as permanent, unexplained uploads in `wusel status`).
        tx.execute("DELETE FROM pending_uploads WHERE object_id = ?1", [ino])?;
    }
    Ok(to_delete.len())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where an asynchronous upload stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadState {
    /// Committed locally, waiting for the uploader.
    Pending,
    /// The uploader has it in flight.
    Uploading,
    /// Given up after a *permanent* failure (wrong permissions, a conflict, no
    /// quota); the bytes are still in the buffer, and the user has been told.
    Error,
}

impl UploadState {
    fn as_str(self) -> &'static str {
        match self {
            UploadState::Pending => "pending",
            UploadState::Uploading => "uploading",
            UploadState::Error => "error",
        }
    }

    /// An unknown value from a newer version is read as `pending` — the safe
    /// reading, since it only means the uploader will look at it again.
    fn from_str(s: &str) -> Self {
        match s {
            "uploading" => UploadState::Uploading,
            "error" => UploadState::Error,
            _ => UploadState::Pending,
        }
    }
}

/// A local change committed on close but not yet on the server — the durable
/// record that makes an asynchronous upload survive a crash. Its precondition
/// (`base_etag`) lives here, not only in memory, so a resumed upload is still a
/// *conditional* one and cannot silently overwrite a newer server version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUpload {
    pub object: ObjectId,
    /// The upload target, account-relative — stored, not re-derived.
    pub remote_path: String,
    /// The version the edit is based on; empty means "must not exist yet".
    pub base_etag: String,
    /// A modification time to send as `X-OC-Mtime`, if one was set.
    pub mtime: Option<i64>,
    pub state: UploadState,
    pub attempts: u32,
    pub last_error: Option<String>,
}

impl PendingUpload {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(PendingUpload {
            object: ObjectId(row.get::<_, i64>(0)? as u64),
            remote_path: row.get(1)?,
            base_etag: row.get(2)?,
            mtime: row.get(3)?,
            state: UploadState::from_str(&row.get::<_, String>(4)?),
            attempts: row.get::<_, i64>(5)? as u32,
            last_error: row.get(6)?,
        })
    }
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
    /// Whether this directory is the root of a Team/Group folder — see
    /// [`crate::model::is_group_folder_root`]. Stored as the answer rather
    /// than its two ingredients: it is the only question anything asks.
    pub group_root: bool,
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
            group_root: row.get::<_, i64>(10).unwrap_or(0) != 0,
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
            mount_type: String::new(),
            is_mount_root: false,
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
    fn deleting_a_node_clears_its_pending_upload() {
        let mut db = StateDb::open_in_memory().unwrap();
        db.reconcile_children(ROOT_INODE, "", &[entry("Invoice.pdf", false)])
            .unwrap();
        let node = db
            .child_by_name(ROOT_INODE, "Invoice.pdf")
            .unwrap()
            .unwrap();
        db.mark_pending_upload(ObjectId(node.inode), "Invoice.pdf", "e", None)
            .unwrap();
        assert_eq!(db.pending_uploads().unwrap().len(), 1);

        // Removing the node (a server-side delete/rename) must take its owed
        // upload with it — otherwise it lingers as a ghost "waiting" entry.
        db.remove_subtree(node.inode).unwrap();
        assert!(
            db.pending_uploads().unwrap().is_empty(),
            "a removed node must not leave a pending upload behind"
        );
    }

    #[test]
    fn the_orphan_sweep_clears_only_ghost_uploads() {
        let mut db = StateDb::open_in_memory().unwrap();
        db.reconcile_children(ROOT_INODE, "", &[entry("Live.pdf", false)])
            .unwrap();
        let live = db.child_by_name(ROOT_INODE, "Live.pdf").unwrap().unwrap();
        db.mark_pending_upload(ObjectId(live.inode), "Live.pdf", "e", None)
            .unwrap();
        // A ghost from an older database: a pending upload whose node is gone.
        db.mark_pending_upload(ObjectId(999_999), "Gone.pdf", "e", None)
            .unwrap();
        assert_eq!(db.pending_uploads().unwrap().len(), 2);

        let removed = db.remove_orphaned_uploads().unwrap();
        assert_eq!(removed, 1, "only the node-less ghost is swept");
        let left = db.pending_uploads().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(
            left[0].object,
            ObjectId(live.inode),
            "the live upload stays"
        );
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

    /// The pins themselves live in [`crate::pins`] now; what this database still
    /// owes them is the pair of paths a rename went between. Without it the
    /// caller cannot carry the promise, and a renamed pinned directory is
    /// silently unpinned.
    #[test]
    fn a_move_reports_the_paths_a_pin_has_to_follow() {
        let mut db = StateDb::open_in_memory().unwrap();
        db.reconcile_children(
            ROOT_INODE,
            "",
            &[entry("Photos", true), entry("Archive", true)],
        )
        .unwrap();
        let photos = db.node_by_path("Photos").unwrap().unwrap();
        let archive = db.node_by_path("Archive").unwrap().unwrap();

        let (from, to) = db
            .move_subtree(photos.inode, archive.inode, "Bilder")
            .unwrap();
        assert_eq!((from.as_str(), to.as_str()), ("Photos", "Archive/Bilder"));
    }

    /// `rename(2)` replaces its destination, and an atomic save always does:
    /// the editor renames its temporary over the document. Two things have to
    /// happen for that, and both were missing.
    #[test]
    fn a_rename_replaces_its_destination_and_takes_over_its_identity() {
        let mut db = StateDb::open_in_memory().unwrap();
        db.reconcile_children(
            ROOT_INODE,
            "",
            &[entry("Notes.txt", false), entry("Sub", true)],
        )
        .unwrap();
        let target = db.node_by_path("Notes.txt").unwrap().unwrap();
        assert!(target.file_id.is_some(), "the document is on the server");

        // The editor's temporary: local only, no server identity.
        let tmp = db
            .insert_local_file(ROOT_INODE, ".goutputstream-ABC")
            .unwrap()
            .inode;
        assert!(db.node_by_inode(tmp).unwrap().unwrap().file_id.is_none());

        // UNIQUE(parent, name) would reject this outright without the occupant
        // being cleared first — which is what surfaced as EIO on Ctrl+S.
        let (from, to) = db.move_subtree(tmp, ROOT_INODE, "Notes.txt").unwrap();
        assert_eq!(
            (from.as_str(), to.as_str()),
            (".goutputstream-ABC", "Notes.txt")
        );

        let now = db.node_by_path("Notes.txt").unwrap().unwrap();
        assert_eq!(now.inode, tmp, "the mover holds the name");
        assert_eq!(
            now.file_id, target.file_id,
            "and inherits the server resource it replaced, so the upload \
             overwrites instead of asserting the file does not exist"
        );
        assert_eq!(now.etag, target.etag, "…including the version to assert");
    }

    /// A node that already has a server identity keeps its own: a server-side
    /// MOVE has settled that case, and the resource that survives there is the
    /// one that moved, not the one it landed on.
    #[test]
    fn a_materialised_mover_keeps_its_own_identity() {
        let mut db = StateDb::open_in_memory().unwrap();
        let with_id = |path: &str, id: u64| RemoteEntry {
            file_id: Some(id),
            ..entry(path, false)
        };
        db.reconcile_children(
            ROOT_INODE,
            "",
            &[with_id("a.txt", 11), with_id("b.txt", 22)],
        )
        .unwrap();
        let a = db.node_by_path("a.txt").unwrap().unwrap();
        let b = db.node_by_path("b.txt").unwrap().unwrap();
        assert_ne!(a.file_id, b.file_id, "two distinct server resources");

        db.move_subtree(a.inode, ROOT_INODE, "b.txt").unwrap();
        let now = db.node_by_path("b.txt").unwrap().unwrap();
        assert_eq!(now.file_id, a.file_id, "the mover's own identity survives");
    }

    /// A database from before pins moved out still has the table; a fresh one
    /// does not. Both must answer, because the migration asks every database it
    /// opens exactly once.
    #[test]
    fn a_database_without_a_pins_table_reports_no_legacy_pins() {
        let db = StateDb::open_in_memory().unwrap();
        assert!(db.legacy_pins().unwrap().is_empty());
    }

    #[test]
    fn the_pins_of_an_older_database_are_still_readable() {
        let db = StateDb::open_in_memory().unwrap();
        db.conn
            .execute_batch(
                "CREATE TABLE pins (path TEXT PRIMARY KEY, is_dir INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO pins VALUES ('Photos', 1), ('a.txt', 0);",
            )
            .unwrap();
        assert_eq!(
            db.legacy_pins().unwrap(),
            vec![("Photos".to_string(), true), ("a.txt".to_string(), false)]
        );
    }
    #[test]
    fn move_subtree_rejects_a_move_into_its_own_subtree() {
        let mut db = StateDb::open_in_memory().unwrap();
        db.reconcile_children(ROOT_INODE, "", &[entry("Docs", true)])
            .unwrap();
        let docs = db.node_by_path("Docs").unwrap().unwrap();
        db.reconcile_children(docs.inode, "Docs", &[entry("Docs/Sub", true)])
            .unwrap();
        let sub = db.node_by_path("Docs/Sub").unwrap().unwrap();

        // Moving a directory below itself would make the parent links cyclic —
        // the path-rewriting walk would then never terminate. Reject it instead.
        let err = db
            .move_subtree(docs.inode, sub.inode, "Docs")
            .expect_err("a move into its own subtree must be refused");
        assert!(
            err.to_string().contains("subtree"),
            "the error says what happened: {err}"
        );
        // And a move onto itself is the degenerate case of the same thing.
        assert!(db.move_subtree(docs.inode, docs.inode, "Docs").is_err());

        // The rejected move left the tree exactly as it was.
        assert_eq!(db.node_by_path("Docs").unwrap().unwrap().inode, docs.inode);
        assert_eq!(
            db.node_by_path("Docs/Sub").unwrap().unwrap().inode,
            sub.inode
        );
    }

    #[test]
    fn only_a_group_folders_root_is_recorded_as_one() {
        let mut db = StateDb::open_in_memory().unwrap();
        let mut team = entry("Team", true);
        team.mount_type = "group".into();
        team.is_mount_root = true;
        // Inside the folder the server reports the same mount-type — the case
        // that would badge a whole subtree if the root flag were not consulted.
        let mut inside = entry("Inside", true);
        inside.mount_type = "group".into();
        inside.is_mount_root = false;
        // A received share is a mount root, but not a group folder.
        let mut shared = entry("FromBob", true);
        shared.mount_type = "shared".into();
        shared.is_mount_root = true;
        let plain = entry("Plain", true);

        db.reconcile_children(ROOT_INODE, "", &[team, inside, shared, plain])
            .unwrap();

        let flag = |name: &str| {
            db.child_by_name(ROOT_INODE, name)
                .unwrap()
                .unwrap_or_else(|| panic!("no node for {name}"))
                .group_root
        };
        assert!(flag("Team"), "the group folder's root is marked");
        assert!(!flag("Inside"), "its contents are not");
        assert!(!flag("FromBob"), "a received share is not a group folder");
        assert!(!flag("Plain"));
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
            mount_type: String::new(),
            is_mount_root: false,
        };
        let readonly = RemoteEntry {
            path: "readonly.txt".into(),
            permissions: "GR".into(),
            mount_type: String::new(),
            is_mount_root: false,
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

    #[test]
    fn a_pending_upload_round_trips_and_a_recommit_resets_it() {
        let db = StateDb::open_in_memory().unwrap();
        let obj = wusel_fsm::ObjectId(42);
        assert!(db.pending_upload(obj).unwrap().is_none());

        db.mark_pending_upload(obj, "Docs/report.odt", "etag-1", Some(1000))
            .unwrap();
        let p = db.pending_upload(obj).unwrap().expect("recorded");
        assert_eq!(p.object, obj);
        assert_eq!(p.remote_path, "Docs/report.odt");
        assert_eq!(p.base_etag, "etag-1");
        assert_eq!(p.mtime, Some(1000));
        assert_eq!(p.state, UploadState::Pending);
        assert_eq!(p.attempts, 0);

        // A failed attempt, then a permanent error parks it.
        db.bump_upload_attempt(obj).unwrap();
        db.set_upload_state(obj, UploadState::Error, Some("403 Forbidden"))
            .unwrap();
        let p = db.pending_upload(obj).unwrap().unwrap();
        assert_eq!(p.attempts, 1);
        assert_eq!(p.state, UploadState::Error);
        assert_eq!(p.last_error.as_deref(), Some("403 Forbidden"));

        // A fresh close of the same file supersedes it: new bytes, clean slate.
        db.mark_pending_upload(obj, "Docs/report.odt", "etag-2", None)
            .unwrap();
        let p = db.pending_upload(obj).unwrap().unwrap();
        assert_eq!(p.state, UploadState::Pending, "re-commit clears the error");
        assert_eq!(p.attempts, 0, "and resets the attempt count");
        assert_eq!(p.base_etag, "etag-2");
        assert_eq!(p.last_error, None);
    }

    #[test]
    fn a_move_follows_the_bytes_and_clear_forgets_them() {
        let db = StateDb::open_in_memory().unwrap();
        let obj = wusel_fsm::ObjectId(7);
        db.mark_pending_upload(obj, ".goutputstream-XYZ", "", None)
            .unwrap();
        // The office-suite atomic save: the temp file is renamed onto the
        // document, so its pending upload must follow to the document's path.
        db.move_pending_upload(obj, "report.odt").unwrap();
        assert_eq!(
            db.pending_upload(obj).unwrap().unwrap().remote_path,
            "report.odt"
        );

        db.clear_pending_upload(obj).unwrap();
        assert!(db.pending_upload(obj).unwrap().is_none());
    }

    #[test]
    fn pending_uploads_survive_a_reopen() {
        // The whole point of persisting them: a crash between close and upload
        // must not lose the record, or the file is silently never uploaded.
        let dir = std::env::temp_dir().join(format!("wusel-pending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.sqlite");
        {
            let db = StateDb::open(&path).unwrap();
            db.mark_pending_upload(wusel_fsm::ObjectId(1), "a.txt", "e1", None)
                .unwrap();
            db.mark_pending_upload(wusel_fsm::ObjectId(2), "b.txt", "e2", Some(5))
                .unwrap();
        }
        let db = StateDb::open(&path).unwrap();
        let all = db.pending_uploads().unwrap();
        assert_eq!(all.len(), 2, "both records survived the reopen");
        assert_eq!(all[0].object, wusel_fsm::ObjectId(1));
        assert_eq!(all[1].remote_path, "b.txt");
        assert_eq!(all[1].mtime, Some(5));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
