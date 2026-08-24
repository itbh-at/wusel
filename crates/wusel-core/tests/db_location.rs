// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Where the state database ends up on *this* machine.
//!
//! The unit tests in `storage` decide the policy against a handwritten mount
//! table. This one asks the real system, so it covers the wiring the policy
//! hangs from: `XDG_STATE_HOME` → [`Account::state_dir`] → the mount table →
//! [`Account::db_location`] → the path that is actually opened.
//!
//! It asserts in both directions, so it is worth running in both worlds:
//!
//! * On an ordinary machine the state directory is local and the database must
//!   stay exactly where it was — a guard against relocating for no reason,
//!   which would silently orphan everybody's cache.
//! * Under `scripts/check-network-home.sh`, which runs it in a mount namespace
//!   whose `/proc/self/mounts` says the home is on NFS, the same assertion
//!   demands the move. That is the only way to exercise the real path without a
//!   file server.

use wusel_core::config::Account;
use wusel_core::storage::{self, DbLocation};

#[test]
fn the_database_goes_local_and_says_so_when_it_has_to_move() {
    let account = Account::new("default");
    let nominal = account.state_dir().join("state.sqlite");
    let location = account.db_location();

    let Some(mounts) = storage::mount_table() else {
        // No mount table (not Linux): the documented answer is "change
        // nothing", and that is worth holding to.
        assert_eq!(
            location,
            DbLocation::Local(nominal),
            "without a mount table nothing may be moved"
        );
        return;
    };

    let fstype = storage::fstype_at(&nominal, &mounts)
        .unwrap_or_else(|| panic!("no mount covers {}:\n{mounts}", nominal.display()));

    println!("state dir is on {fstype} -> {location:?}");
    if storage::is_network_fs(&fstype) {
        let DbLocation::Relocated { from, to, .. } = &location else {
            panic!("state dir is on {fstype}, but the database stayed put: {location:?}");
        };
        assert_eq!(from, &nominal);
        assert!(
            to.starts_with("/var/tmp"),
            "a relocated database belongs on local storage, not {to:?}"
        );
        assert!(
            !storage::is_network_fs(
                &storage::fstype_at(to, &mounts).expect("the target is under some mount")
            ),
            "moved from one network filesystem to another: {to:?}"
        );
        assert!(location.message().is_some(), "a move is never silent");
        // And the path everything else uses is the moved one — a caller that
        // opened the old one would address a database nobody writes to.
        assert_eq!(account.state_db_path(), *to);
    } else {
        assert_eq!(
            location,
            DbLocation::Local(nominal),
            "the state dir is on {fstype}, which is local: leave it alone"
        );
        assert_eq!(location.message(), None, "nothing happened, so say nothing");
    }
}
