// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Stamp the binary with the commit it was built from and when, so a log line
//! or `--version` can say exactly which engine is running — the on-disk binary
//! and the live process are otherwise impossible to tell apart, which has cost
//! real debugging time on macOS (a stale bundled engine looked identical).
//!
//! Dependency-free on purpose (see the project's "as few deps as possible"
//! rule): it shells out to `git` and never fails the build — a missing git or a
//! tarball checkout just yields `unknown`.

use std::process::Command;

fn main() {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let hash = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    // Repo-wide: any uncommitted change means the binary is not exactly `hash`.
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    let commit = if dirty { format!("{hash}-dirty") } else { hash };

    // The bundle task injects a precise build time; a plain `cargo build` falls
    // back to the commit's own date, so the stamp is always meaningful.
    let time = std::env::var("WUSEL_BUILD_TIME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| git(&["show", "-s", "--format=%cI", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env=WUSEL_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=WUSEL_BUILD_TIME={time}");

    // Re-stamp when the commit moves or the injected time changes; otherwise the
    // cached build.rs output (and a stale commit) would stick. This has to catch
    // a `git pull` as well, which moves the *branch ref* and leaves `.git/HEAD`
    // untouched (HEAD keeps pointing at `refs/heads/<branch>`) — watching HEAD
    // alone let a pulled-then-rebuilt binary keep the pre-pull commit, the exact
    // stale-stamp confusion this whole file exists to end. `--git-path` resolves
    // each file correctly inside a worktree, where `.git` is a file.
    println!("cargo:rerun-if-env-changed=WUSEL_BUILD_TIME");
    let watch = |path: &str| {
        if let Some(p) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={p}");
        }
    };
    watch("HEAD"); // a branch switch, or a detached HEAD moving
    watch("packed-refs"); // the branch ref while refs are packed
    watch("logs/HEAD"); // the reflog: appended on every move of HEAD's target

    // The current branch's own ref file, rewritten on commit / pull / merge /
    // reset even from a packed state. Skipped on a detached HEAD, which `HEAD`
    // above already covers.
    if let Some(git_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch(&git_ref); // e.g. refs/heads/main
    }
}
