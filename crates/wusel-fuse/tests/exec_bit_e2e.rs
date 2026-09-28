// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! `chmod +x` through a real mount.
//!
//! WebDAV has no mode bits, so the executable bit is kept in the local state
//! only — the way the official desktop client keeps it in its local tree. What
//! the mount owes the user is that it *holds* locally: a script marked
//! executable runs, stays executable across the ways files are saved and
//! renamed, and `test -x` agrees with what `execve` does. It is never sent to
//! the server, so nothing arrives executable from another device.
//!
//! Every property is checked and the failures are reported together, so one
//! run shows the whole picture instead of the first broken step.
//!
//! Linux only, and needs `/dev/fuse` — it runs in the container.

mod common;

use std::fs::{self, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const SCRIPT: &[u8] = b"#!/bin/sh\nexit 7\n";

fn mode(p: &Path) -> u32 {
    fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0)
}

fn is_exec(p: &Path) -> bool {
    mode(p) & 0o111 != 0
}

/// What `test -x` says — `access(X_OK)`, the question shells, `which` and build
/// tools ask before running anything.
fn access_x(p: &Path) -> bool {
    Command::new("test")
        .arg("-x")
        .arg(p)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Whether the file actually runs: the script exits 7.
fn runs(p: &Path) -> bool {
    Command::new(p)
        .status()
        .map(|s| s.code() == Some(7))
        .unwrap_or(false)
}

fn chmod(p: &Path, mode: u32) -> std::io::Result<()> {
    fs::set_permissions(p, Permissions::from_mode(mode))
}

fn write_file(p: &Path, bytes: &[u8]) {
    let mut f = fs::File::create(p).expect("create");
    f.write_all(bytes).expect("write");
}

/// Wait until the server holds `bytes` at `name`, so a check afterwards sees
/// the state after the upload and its re-listing, not before.
fn landed(m: &common::MountFixture, name: &str, bytes: &[u8]) {
    common::eventually(&format!("{name} reaches the server"), || {
        fs::read(m.fixture.join(name)).ok().as_deref() == Some(bytes)
    });
    // The re-listing behind an upload runs after the bytes are there.
    std::thread::sleep(Duration::from_millis(500));
}

#[test]
fn the_executable_bit_is_kept_locally() {
    // Short revalidation, so a server-side change is picked up within the test.
    std::env::set_var("WUSEL_TEST_REVALIDATE_SECS", "1");
    let m = common::MountFixture::start("execbit");
    let mut broken: Vec<String> = Vec::new();
    let mut check = |ok: bool, what: &str| {
        if !ok {
            broken.push(what.to_string());
        }
    };

    // 1. A plain new file is not executable, and `test -x` says so.
    let script = m.mnt.join("script.sh");
    write_file(&script, SCRIPT);
    check(!is_exec(&script), "a new file starts without the x bit");
    check(
        !access_x(&script),
        "`test -x` is false for a file without the x bit",
    );

    // 2. `chmod +x` makes it executable, and it runs.
    check(chmod(&script, 0o755).is_ok(), "chmod 755 succeeds");
    check(is_exec(&script), "chmod 755 shows in the mode");
    check(access_x(&script), "`test -x` is true after chmod +x");
    check(runs(&script), "the script runs after chmod +x");

    // 3. …and stays so once it is uploaded and the directory re-listed.
    landed(&m, "script.sh", SCRIPT);
    check(is_exec(&script), "the x bit survives the upload");

    // 4. The bit is local: the server's copy is untouched by it.
    check(
        mode(&m.fixture.join("script.sh")) & 0o111 == 0,
        "the x bit is not sent to the server",
    );

    // 5. A file created executable (cp, install, tar, unzip) is executable.
    let born = m.mnt.join("born.sh");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&born)
            .expect("create with mode 0755");
        f.write_all(SCRIPT).unwrap();
    }
    check(
        is_exec(&born),
        "a file created with mode 0755 is executable",
    );
    check(runs(&born), "a file created with mode 0755 runs");

    // 6. A rename keeps it.
    let renamed = m.mnt.join("renamed.sh");
    fs::rename(&script, &renamed).expect("rename");
    check(is_exec(&renamed), "the x bit survives a rename");

    // 7. An in-place edit keeps it: `sed -i` writes a temporary, copies the
    //    mode onto it and renames it over the original.
    let sed = Command::new("sed")
        .args(["-i", "s/exit 7/exit 7 # edited/"])
        .arg(&renamed)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    check(sed, "sed -i succeeds");
    check(is_exec(&renamed), "the x bit survives `sed -i`");
    check(runs(&renamed), "the edited script still runs");

    // 8. A new version from the server keeps it.
    landed(&m, "renamed.sh", b"#!/bin/sh\nexit 7 # edited\n");
    fs::write(
        m.fixture.join("renamed.sh"),
        b"#!/bin/sh\nexit 7 # server\n",
    )
    .unwrap();
    common::eventually("the server's version reaches the mount", || {
        let _ = fs::read_dir(&m.mnt).map(|d| d.count());
        fs::read(&renamed).ok().as_deref() == Some(b"#!/bin/sh\nexit 7 # server\n".as_slice())
    });
    check(
        is_exec(&renamed),
        "the x bit survives a new version from the server",
    );

    // 9. `chmod -x` takes it away again.
    check(chmod(&renamed, 0o644).is_ok(), "chmod 644 succeeds");
    check(!is_exec(&renamed), "chmod -x clears the x bit");
    check(!access_x(&renamed), "`test -x` is false after chmod -x");

    // 10. A file that came from the server can be made executable too.
    let notes = m.mnt.join("Notes.txt");
    check(
        chmod(&notes, 0o755).is_ok(),
        "chmod +x on a server file succeeds",
    );
    check(is_exec(&notes), "a server file takes the x bit");

    // 11. A directory's mode is not ours to change: accepted, and unchanged.
    let dir = m.mnt.join("Sub Folder");
    check(chmod(&dir, 0o700).is_ok(), "chmod on a directory succeeds");
    check(mode(&dir) == 0o755, "a directory keeps its mode");

    assert!(broken.is_empty(), "broken:\n  {}", broken.join("\n  "));
}
