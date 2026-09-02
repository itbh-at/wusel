// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! The notify hook is the one channel in this crate that is *not* Linux-gated
//! (see the crate docs) — a subprocess and some environment variables, nothing
//! platform-specific. So unlike the D-Bus channels, it can be verified for real
//! right here, on whatever platform `cargo test` runs on, not only inside the
//! Linux container the FUSE/D-Bus paths need.
//!
//! Each test writes its own tiny shell script that records what it received,
//! calls `wusel_desktop::backend` exactly as the daemon does, fires a real
//! `Notice` through it, and polls for the script's output — proving the
//! environment variables actually round-trip, not just that the code compiles.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use wusel_core::desktop::Notice;

const PATIENCE: Duration = Duration::from_secs(5);

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wusel-notify-hook-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test scratch directory");
    dir
}

/// Write an executable script at `path` that dumps the three `WUSEL_NOTICE_*`
/// variables to `marker`, one per line — plain enough to assert on directly.
fn write_hook_script(path: &Path, marker: &Path) {
    let script = format!(
        "#!/bin/sh\n\
         {{\n\
         echo \"TITLE=$WUSEL_NOTICE_TITLE\"\n\
         echo \"BODY=$WUSEL_NOTICE_BODY\"\n\
         echo \"JSON=$WUSEL_NOTICE_JSON\"\n\
         }} > {marker:?}\n"
    );
    let mut f = std::fs::File::create(path).expect("create the hook script");
    f.write_all(script.as_bytes())
        .expect("write the hook script");
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("make the hook script executable");
    }
}

/// Poll for the marker file `run_hook`'s background thread writes, rather than
/// sleeping a fixed amount — the hook runs asynchronously by design (see
/// `run_hook`'s doc comment), so the test has to wait for it the same way a real
/// admin's monitoring would.
fn wait_for_marker(marker: &Path) -> String {
    let start = Instant::now();
    loop {
        if let Ok(contents) = std::fs::read_to_string(marker) {
            return contents;
        }
        if start.elapsed() > PATIENCE {
            panic!("the notify hook never wrote {}", marker.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_notice_reaches_the_hook_with_localized_text_and_raw_json() {
    let dir = scratch_dir("basic");
    let marker = dir.join("marker.txt");
    let hook = dir.join("hook.sh");
    write_hook_script(&hook, &marker);

    let desktop = wusel_desktop::backend("test-account", Path::new("/nonexistent"), Some(&hook));
    desktop.notify(&Notice::UploadFailed {
        path: "big.iso".into(),
        reason: "quota exceeded".into(),
    });

    let contents = wait_for_marker(&marker);
    assert!(contents.contains("TITLE=Upload failed"), "{contents}");
    assert!(contents.contains("big.iso"), "{contents}");
    assert!(contents.contains("quota exceeded"), "{contents}");
    // The JSON line carries the stable, unlocalized `kind` — what a script
    // matches on instead of parsing the English/German sentence.
    assert!(
        contents.contains("JSON={\"kind\":\"upload-failed\""),
        "{contents}"
    );
}

#[test]
fn no_hook_configured_is_a_silent_no_op() {
    // `backend(..., None)` must not panic, spawn anything, or otherwise behave
    // differently from before this feature existed.
    let desktop = wusel_desktop::backend("test-account", Path::new("/nonexistent"), None);
    desktop.notify(&Notice::ConnectionRestored {
        server: "https://cloud.example.org".into(),
    });
    // Nothing to assert beyond "this returned" — there is no channel left for
    // it to have reached.
}

#[test]
fn a_missing_hook_is_logged_and_skipped_not_fatal() {
    let dir = scratch_dir("missing");
    let hook = dir.join("does-not-exist.sh");

    // Must not panic — `check_hook`'s start-up warning and `exec_hook`'s
    // spawn-error handling are both supposed to swallow this.
    let desktop = wusel_desktop::backend("test-account", Path::new("/nonexistent"), Some(&hook));
    desktop.notify(&Notice::ConnectionLost {
        server: "https://cloud.example.org".into(),
    });
    // Give the background thread a moment to run (and not panic) before the
    // process exits; there is nothing further to observe.
    std::thread::sleep(Duration::from_millis(100));
}

#[test]
fn a_non_executable_hook_is_logged_and_skipped_not_fatal() {
    let dir = scratch_dir("not-executable");
    let hook = dir.join("hook.sh");
    std::fs::write(&hook, "#!/bin/sh\necho should-not-run\n").expect("write the hook script");
    // Deliberately no chmod +x.

    let desktop = wusel_desktop::backend("test-account", Path::new("/nonexistent"), Some(&hook));
    desktop.notify(&Notice::ConnectionLost {
        server: "https://cloud.example.org".into(),
    });
    std::thread::sleep(Duration::from_millis(100));
}
