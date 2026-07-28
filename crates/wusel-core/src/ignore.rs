// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH

//! Ignore patterns for ephemeral editor/OS files.
//!
//! Editors and office suites litter a directory with throwaway files — vim swap
//! files, LibreOffice/MS Office lock files, backup and temp files. A live VFS
//! would faithfully upload every one, which is pointless (they are local session
//! state), noisy (each is a server round-trip), and occasionally breaks on server
//! quirks. Like the reference client's `sync-exclude.lst`, we keep files whose
//! **basename** matches an ignore pattern purely local (see the provider's
//! local-only handling); they never reach the server.
//!
//! Matching is a tiny glob — `*` (any run, incl. empty) and `?` (exactly one
//! character) — against the basename only. No character classes: the patterns we
//! need (`.*.sw?`, `~$*`, `.~lock.*#`, …) do not use them, and staying minimal
//! keeps us dependency-free.

/// The built-in default patterns, used unless `[sync] ignore_patterns` overrides
/// them. Curated for the common editors and desktops.
pub const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    ".*.sw?",           // vim swap files (.foo.swp/.swo/.swn/…)
    "4913",             // vim's writability-probe file
    "*~",               // backup files (vim, emacs, gedit)
    ".~lock.*#",        // LibreOffice lock files
    "~$*",              // MS Office owner/lock files
    ".#*",              // emacs lock files
    "*.tmp",            // generic temporary files
    ".goutputstream-*", // GNOME/gvfs atomic-save temporaries
    ".DS_Store",        // macOS directory metadata
    "Thumbs.db",        // Windows thumbnail cache
];

/// Whether `name` (a basename) matches any of `patterns`.
pub fn is_ignored(name: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|pat| glob_match(pat.as_bytes(), name.as_bytes()))
}

/// Glob match with `*` and `?`, over bytes. Linear-ish with single-star
/// backtracking — patterns and names are short, so this is more than fast enough.
fn glob_match(pattern: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    // Remember the last `*` so we can backtrack: extend what it consumed by one.
    let mut star: Option<(usize, usize)> = None; // (pattern index after '*', name index)
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p + 1, n));
            p += 1; // try to match `*` as empty first
        } else if let Some((next_p, next_n)) = star {
            p = next_p;
            n = next_n + 1; // let the last `*` swallow one more character
            star = Some((next_p, next_n + 1));
        } else {
            return false;
        }
    }
    // Trailing `*`s in the pattern match the empty remainder.
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Vec<String> {
        DEFAULT_IGNORE_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn glob_star_and_question() {
        assert!(glob_match(b"*", b"anything"));
        assert!(glob_match(b"*", b""));
        assert!(glob_match(b".*.sw?", b".test.txt.swp"));
        assert!(glob_match(b".*.sw?", b".a.swo"));
        assert!(!glob_match(b".*.sw?", b".a.swpx")); // ? is exactly one char
        assert!(glob_match(b"~$*", b"~$report.docx"));
        assert!(glob_match(b".~lock.*#", b".~lock.notes.odt#"));
        assert!(glob_match(b"4913", b"4913"));
        assert!(!glob_match(b"4913", b"4913x"));
        assert!(glob_match(b"*.tmp", b"lu1234.tmp"));
        assert!(!glob_match(b"*.tmp", b"file.tmpx"));
    }

    #[test]
    fn defaults_catch_the_usual_suspects() {
        let p = defaults();
        for name in [
            ".test.txt.swp",
            "4913",
            "notes.odt~",
            ".~lock.notes.odt#",
            "~$budget.xlsx",
            ".#main.rs",
            "scratch.tmp",
            ".goutputstream-ABCD12",
            ".DS_Store",
            "Thumbs.db",
        ] {
            assert!(is_ignored(name, &p), "should ignore {name}");
        }
        // Real files must NOT be ignored.
        for name in [
            "notes.odt",
            "report.docx",
            "main.rs",
            "photo.jpg",
            "swp.txt",
        ] {
            assert!(!is_ignored(name, &p), "must not ignore {name}");
        }
    }
}
