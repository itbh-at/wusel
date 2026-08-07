// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Where the state database is allowed to live.
//!
//! Business machines put home directories on NFS and CIFS, and the state
//! database would land there with them. That is not a performance question but
//! a correctness one — SQLite says so itself:
//!
//! > All processes using a database must be on the same host computer; WAL does
//! > not work over a network filesystem.
//!
//! > This locking mechanism might not work correctly if the database file is
//! > kept on an NFS filesystem. This is because fcntl() file locking is broken
//! > on many NFS implementations.
//!
//! Wusel is exactly the case those warnings are about: several connections to
//! one database, WAL on, one writer alongside N readers. A silent corruption
//! here would look like a synchronisation bug for weeks.
//!
//! So the database is moved to local storage when it would otherwise sit on a
//! network filesystem — and **never silently**: the caller is handed a
//! [`DbLocation`] describing what happened, to say so loudly.
//!
//! # Why a text file instead of `statfs`
//!
//! `statfs(2)` reports the filesystem type as a magic number and needs `libc`
//! and a `target_os` gate. `/proc/self/mounts` says the same thing in text that
//! is stable across kernels, costs one small read, and is *absent* on every
//! system that does not have it — which is precisely the answer we want there
//! ("cannot tell", so change nothing). The detection therefore adds no
//! dependency and no platform gate to `wusel-core`.
//!
//! # The seam, for the platforms that come later
//!
//! Only [`mount_table`] and [`uid`] are Linux-shaped. Everything that *decides*
//! — the list below, [`fstype_at`], [`resolve`], [`DbLocation`] — is plain data
//! and works anywhere. macOS asks `getfsstat(2)` instead of reading a file and
//! gets `f_fstypename` back as a string (`nfs`, `smbfs`, `afpfs`), which is the
//! same vocabulary this module already speaks; Windows asks
//! `GetDriveType`/`WNetGetConnection`. So a second platform adds one function
//! that produces the type, and inherits the policy and its tests untouched.
//!
//! That is why the tests split the way they do: the policy tests run
//! everywhere, because the policy is not platform-specific.

use std::path::{Path, PathBuf};

/// Filesystem types the state database must not live on.
///
/// The list is deliberately explicit rather than a "not in a known-local set"
/// rule: a filesystem we have never heard of is far more likely to be a new
/// local one than a new network one, and wrongly relocating somebody's database
/// is worse than leaving it where they put it. Everything here either lacks
/// working `fcntl` locking across hosts or lacks shared memory for WAL, or both.
const NETWORK_FILESYSTEMS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smbfs",
    "smb3",
    "afs",
    "ncpfs",
    "afpfs",
    "9p",
    "lustre",
    "beegfs",
    "glusterfs",
    "ceph",
    "davfs",
    // FUSE filesystems report as `fuse.<name>`. These are network transports
    // wearing a local face — the very trap this check exists for.
    "fuse.sshfs",
    "fuse.davfs2",
    "fuse.rclone",
    "fuse.s3fs",
    "fuse.gcsfuse",
    "fuse.glusterfs",
    "fuse.cephfs",
    // And ourselves: putting the state database inside the mount it describes
    // would be a loop, network filesystem or not.
    "fuse.wusel",
];

/// Whether a filesystem type from the mount table rules out hosting the
/// database.
#[must_use]
pub fn is_network_fs(fstype: &str) -> bool {
    NETWORK_FILESYSTEMS.contains(&fstype)
}

/// The filesystem type mounted at `path`, from a `/proc/self/mounts` text.
///
/// Takes the table as a string rather than reading it, so the interesting part
/// — longest-prefix matching and the escaping — is testable without mounting
/// anything.
///
/// `path` need not exist: the answer comes from the *deepest mount point that
/// is an ancestor of it*, which is exactly right for a database file we are
/// about to create.
#[must_use]
pub fn fstype_at(path: &Path, mounts: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for line in mounts.lines() {
        // `device mountpoint fstype options dump pass`
        let mut fields = line.split_whitespace();
        let (Some(_dev), Some(point), Some(fstype)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let point = PathBuf::from(unescape(point));
        if !path.starts_with(&point) {
            continue;
        }
        // Depth, not string length: `/` is a prefix of everything, and the
        // deepest matching mount is the one that actually holds the file.
        let depth = point.components().count();
        if best.as_ref().is_none_or(|(d, _)| depth > *d) {
            best = Some((depth, unescape(fstype)));
        }
    }
    best.map(|(_, fstype)| fstype)
}

/// Undo the octal escapes the kernel writes for spaces, tabs and backslashes.
///
/// A mount point with a space in it is rare but entirely legal, and without
/// this it would be split into two fields and shift the type column.
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits: String = chars.clone().take(3).collect();
        match u8::from_str_radix(&digits, 8) {
            Ok(byte) if digits.len() == 3 => {
                out.push(byte as char);
                for _ in 0..3 {
                    chars.next();
                }
            }
            // Not an escape after all — a literal backslash.
            _ => out.push('\\'),
        }
    }
    out
}

/// What was decided about the database's location.
///
/// An enum rather than a `PathBuf` plus a log line, so the caller cannot use
/// the path without having been told how it was arrived at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbLocation {
    /// Where it was asked to be. Nothing to report.
    Local(PathBuf),
    /// Moved off a network filesystem onto local storage.
    Relocated {
        from: PathBuf,
        to: PathBuf,
        fstype: String,
    },
    /// On a network filesystem, and staying there — either because the user
    /// asked for that path explicitly, or because there was nowhere local to
    /// move it to. Reported loudly; never fixed behind the user's back.
    Risky { path: PathBuf, fstype: String },
}

impl DbLocation {
    /// The path to actually open.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Local(p) | Self::Risky { path: p, .. } => p,
            Self::Relocated { to, .. } => to,
        }
    }

    /// Create the directory this database will live in.
    ///
    /// A relocated one lands in `/var/tmp`, which is shared, so it is made
    /// readable by its owner only — the database holds every path name in the
    /// account. That is not a secret worth a key, but it is nobody else's
    /// business either. A database left where the user put it keeps whatever
    /// permissions their home directory has; tightening those uninvited would
    /// be a surprise, and not ours to make.
    ///
    /// # Errors
    ///
    /// If the directory cannot be created, or its permissions not set.
    pub fn prepare(&self) -> std::io::Result<()> {
        let Some(dir) = self.path().parent() else {
            return Ok(());
        };
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        if matches!(self, Self::Relocated { .. }) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// What to tell the user, or `None` when there is nothing to tell.
    ///
    /// Written out in full rather than as a one-liner: whoever reads this is
    /// looking at a machine whose home directory is on a file server, and the
    /// next question is always "what does that mean for me".
    #[must_use]
    pub fn message(&self) -> Option<String> {
        match self {
            Self::Local(_) => None,
            Self::Relocated { from, to, fstype } => Some(format!(
                "The state database would have been on a {fstype} filesystem \
                 ({}), where SQLite cannot lock reliably and WAL does not work \
                 at all. It has been placed on local storage instead: {}. \
                 It is a cache and is rebuilt from the server, so nothing is \
                 lost — but it is per-machine now, and a first start after this \
                 move is a cold one. Set [state] db_path in config.toml to \
                 choose the location yourself.",
                from.display(),
                to.display()
            )),
            Self::Risky { path, fstype } => Some(format!(
                "The state database is on a {fstype} filesystem ({}). SQLite \
                 cannot lock reliably there and WAL does not work over a \
                 network filesystem at all; corruption will look like a \
                 synchronisation bug. Point [state] db_path in config.toml at \
                 local storage.",
                path.display()
            )),
        }
    }
}

/// Decide where the database goes.
///
/// Every input is a parameter — the mount table, the fallback directory, the
/// user's override — so the whole policy is decidable without touching a disk,
/// and the tests below cover the cases that are hard to produce for real.
///
/// * `mounts` is `None` when the table could not be read. Unknown is not
///   "network": we change nothing rather than move somebody's database on a
///   guess.
/// * `configured` wins over everything. If the user names a path on a file
///   server, that is their decision to make — but they hear about it.
#[must_use]
pub fn resolve(
    nominal: PathBuf,
    configured: Option<PathBuf>,
    fallback: Option<PathBuf>,
    mounts: Option<&str>,
) -> DbLocation {
    let path = configured.clone().unwrap_or(nominal);
    let Some(mounts) = mounts else {
        return DbLocation::Local(path);
    };
    let Some(fstype) = fstype_at(&path, mounts) else {
        return DbLocation::Local(path);
    };
    if !is_network_fs(&fstype) {
        return DbLocation::Local(path);
    }
    // An explicit choice is honoured. Overriding it would make the setting a
    // lie, and someone who sets it has a reason we do not know.
    if configured.is_some() {
        return DbLocation::Risky { path, fstype };
    }
    match fallback {
        // The fallback is only worth having if it is not the same problem
        // again: on a machine where /var/tmp is *also* a network mount, moving
        // there would trade one broken location for another.
        Some(to)
            if fstype_at(&to, mounts)
                .as_deref()
                .is_none_or(|t| !is_network_fs(t)) =>
        {
            DbLocation::Relocated {
                from: path,
                to,
                fstype,
            }
        }
        _ => DbLocation::Risky { path, fstype },
    }
}

/// The mount table, or `None` where there is none to read.
#[must_use]
pub fn mount_table() -> Option<String> {
    std::fs::read_to_string("/proc/self/mounts").ok()
}

/// Where a relocated database goes: `/var/tmp/wusel-<uid>/<account>`.
///
/// `/var/tmp` rather than `/tmp`: `/tmp` is frequently a tmpfs, which would put
/// the database in RAM and throw it away at every reboot — a cold start every
/// morning, which is the behaviour we are trying to avoid, not a fix for it.
///
/// The uid is part of the path because `/var/tmp` is shared. Without an owner
/// in the name, two users on one machine would collide on the directory, and
/// the second one would simply fail to start.
#[must_use]
pub fn fallback_dir(account: &str) -> Option<PathBuf> {
    // No uid means no safe per-user directory, and a shared one is not worth
    // having. The caller then reports the problem instead of relocating.
    let uid = uid()?;
    Some(PathBuf::from(format!("/var/tmp/wusel-{uid}")).join(account))
}

/// This process's real uid, read from `/proc` rather than through `libc`.
///
/// Same reasoning as the mount table: no dependency, no platform gate, and its
/// absence answers the question by itself.
fn uid() -> Option<String> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("Uid:"))?;
    // `Uid:\t<real>\t<effective>\t<saved>\t<fs>` — the real one is ours.
    let real = line.split_whitespace().nth(1)?;
    real.chars()
        .all(|c| c.is_ascii_digit())
        .then(|| real.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plausible table: root local, home on NFS, a CIFS share with a space in
    /// its mount point, and /var/tmp local.
    const MOUNTS: &str = "\
/dev/sda2 / ext4 rw,relatime 0 0
proc /proc proc rw,nosuid 0 0
/dev/sda3 /var/tmp xfs rw,relatime 0 0
fileserver:/export/home /home nfs4 rw,relatime 0 0
//server/share /mnt/Team\\040Share cifs rw 0 0
/dev/sda2 /home/local ext4 rw 0 0
";

    #[test]
    fn a_path_takes_the_type_of_the_deepest_mount_that_holds_it() {
        assert_eq!(
            fstype_at(
                Path::new("/home/eva/.local/state/wusel/state.sqlite"),
                MOUNTS
            )
            .as_deref(),
            Some("nfs4")
        );
        // Deeper mount wins over the NFS home it sits inside.
        assert_eq!(
            fstype_at(Path::new("/home/local/db.sqlite"), MOUNTS).as_deref(),
            Some("ext4")
        );
        // And over `/`, which is an ancestor of everything.
        assert_eq!(
            fstype_at(Path::new("/var/tmp/wusel-1000/db"), MOUNTS).as_deref(),
            Some("xfs")
        );
    }

    #[test]
    fn a_mount_point_containing_a_space_is_still_one_field() {
        assert_eq!(
            fstype_at(Path::new("/mnt/Team Share/wusel/state.sqlite"), MOUNTS).as_deref(),
            Some("cifs")
        );
    }

    #[test]
    fn a_prefix_that_is_not_a_path_prefix_does_not_match() {
        // `/home/localish` must not be taken for the `/home/local` mount.
        assert_eq!(
            fstype_at(Path::new("/home/localish/db"), MOUNTS).as_deref(),
            Some("nfs4")
        );
    }

    #[test]
    fn a_local_home_is_left_exactly_where_it_is() {
        let db = PathBuf::from("/home/local/wusel/state.sqlite");
        assert_eq!(
            resolve(db.clone(), None, Some("/var/tmp/w".into()), Some(MOUNTS)),
            DbLocation::Local(db)
        );
    }

    #[test]
    fn a_network_home_is_moved_to_local_storage() {
        let db = PathBuf::from("/home/eva/.local/state/wusel/state.sqlite");
        let to = PathBuf::from("/var/tmp/wusel-1000/default/state.sqlite");
        let location = resolve(db.clone(), None, Some(to.clone()), Some(MOUNTS));
        assert_eq!(
            location,
            DbLocation::Relocated {
                from: db,
                to: to.clone(),
                fstype: "nfs4".into()
            }
        );
        assert_eq!(location.path(), to);
        assert!(location.message().is_some(), "a move is always announced");
    }

    #[test]
    fn an_unreadable_mount_table_changes_nothing() {
        let db = PathBuf::from("/home/eva/state.sqlite");
        assert_eq!(
            resolve(db.clone(), None, Some("/var/tmp/w".into()), None),
            DbLocation::Local(db)
        );
    }

    #[test]
    fn an_explicit_path_is_honoured_even_on_a_file_server() {
        let chosen = PathBuf::from("/home/eva/wusel/state.sqlite");
        let location = resolve(
            "/home/eva/.local/state/wusel/state.sqlite".into(),
            Some(chosen.clone()),
            Some("/var/tmp/w/state.sqlite".into()),
            Some(MOUNTS),
        );
        assert_eq!(
            location,
            DbLocation::Risky {
                path: chosen.clone(),
                fstype: "nfs4".into()
            }
        );
        assert_eq!(location.path(), chosen, "the user's choice is what opens");
        assert!(location.message().is_some(), "and is still warned about");
    }

    #[test]
    fn nowhere_local_to_move_to_means_a_warning_not_a_worse_move() {
        // A machine whose /var/tmp is itself on the file server.
        let all_network = "fileserver:/export / nfs4 rw 0 0\n";
        let db = PathBuf::from("/home/eva/state.sqlite");
        assert_eq!(
            resolve(
                db.clone(),
                None,
                Some("/var/tmp/wusel-1000/db".into()),
                Some(all_network)
            ),
            DbLocation::Risky {
                path: db,
                fstype: "nfs4".into()
            }
        );
    }

    #[test]
    fn no_fallback_directory_means_a_warning_too() {
        let db = PathBuf::from("/home/eva/state.sqlite");
        assert_eq!(
            resolve(db.clone(), None, None, Some(MOUNTS)),
            DbLocation::Risky {
                path: db,
                fstype: "nfs4".into()
            }
        );
    }

    #[test]
    fn a_fuse_transport_counts_as_a_network_filesystem() {
        assert!(is_network_fs("fuse.sshfs"));
        assert!(is_network_fs("cifs"));
        // But an ordinary local one does not, and neither does an unknown one:
        // we do not move a database on a guess.
        assert!(!is_network_fs("btrfs"));
        assert!(!is_network_fs("something-new"));
    }

    /// The handcrafted table above proves the *rules*; this proves the parser
    /// still fits the kernel's actual output, which is the part that could drift
    /// without anybody noticing.
    ///
    /// Where there is no mount table it checks the other half of the contract
    /// instead — that "cannot tell" really means "change nothing" — rather than
    /// passing without having looked at anything.
    #[test]
    fn the_real_mount_table_parses_or_is_absent_and_harmless() {
        let Some(mounts) = mount_table() else {
            assert_eq!(
                resolve(
                    "/anywhere/state.sqlite".into(),
                    None,
                    Some("/var/tmp/w/state.sqlite".into()),
                    None
                ),
                DbLocation::Local("/anywhere/state.sqlite".into()),
                "with no way to tell, nothing may be moved"
            );
            return;
        };
        assert!(
            fstype_at(Path::new("/"), &mounts).is_some(),
            "every system has a root filesystem: {mounts}"
        );
        // And the place a relocated database would go must resolve to something,
        // otherwise the fallback check silently degrades to "cannot tell".
        assert!(
            fstype_at(Path::new("/var/tmp/wusel-0/default/state.sqlite"), &mounts).is_some(),
            "/var/tmp is covered by some mount, if only by /"
        );
    }

    /// The uid comes out of `/proc/self/status`, so it is worth checking that we
    /// read the file the kernel actually writes rather than the one imagined.
    #[test]
    fn the_fallback_directory_is_per_user_and_under_var_tmp() {
        let Some(dir) = fallback_dir("default") else {
            // No /proc: not a system we relocate on, which is the documented
            // behaviour rather than a gap.
            assert!(mount_table().is_none(), "/proc/self/mounts without a uid?");
            return;
        };
        assert!(dir.starts_with("/var/tmp"), "{dir:?}");
        assert!(dir.ends_with("default"), "{dir:?}");
        let owner = dir.parent().unwrap().file_name().unwrap().to_str().unwrap();
        let digits = owner.strip_prefix("wusel-").expect("wusel-<uid>");
        assert!(
            !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()),
            "the owner segment must be a uid, got {owner:?}"
        );
    }

    #[test]
    fn a_relocated_database_gets_a_directory_only_its_owner_can_read() {
        let base = std::env::temp_dir().join(format!("wusel-storage-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let to = base.join("state.sqlite");
        let location = DbLocation::Relocated {
            from: "/home/eva/.local/state/wusel/state.sqlite".into(),
            to: to.clone(),
            fstype: "nfs4".into(),
        };
        location.prepare().expect("prepare the directory");
        assert!(base.is_dir());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&base).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "/var/tmp is shared with other users");
        }
        // Idempotent: a second start must not trip over its own directory.
        location.prepare().expect("prepare again");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_database_left_alone_keeps_the_permissions_it_had() {
        let base = std::env::temp_dir().join(format!("wusel-storage-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let location = DbLocation::Local(base.join("state.sqlite"));
        location.prepare().expect("prepare the directory");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&base).unwrap().permissions().mode();
            assert_ne!(
                mode & 0o777,
                0o700,
                "tightening a directory in the user's own home is not ours to do                  (unless their umask already did it — then this test is moot)"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn escapes_that_are_not_escapes_survive_unharmed() {
        assert_eq!(unescape("/mnt/a\\040b"), "/mnt/a b");
        assert_eq!(unescape("/mnt/back\\slash"), "/mnt/back\\slash");
        assert_eq!(unescape("/plain"), "/plain");
    }
}
