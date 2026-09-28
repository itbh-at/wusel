#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Warn about changelog entries that were not left the way `mise run
# release` wrote them: a weekday that does not match its date (rpmbuild
# only warns: "bogus date in %changelog"), or a TODO line still waiting for the
# release notes. Runs as part of `mise run headers-check`; it warns and never
# fails the gate.
#
# The weekday is computed in awk (Sakamoto's method) rather than with `date -d`,
# which is GNU-only: the check has to run on a macOS host, too.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"

awk '
function weekday(y, m, d,    t) {
    # 0 = Sunday. The month offsets absorb the varying month lengths; January
    # and February count as months of the previous year for the leap day.
    split("0 3 2 5 0 3 5 1 4 6 2 4", t, " ")
    if (m < 3) y--
    return (y + int(y / 4) - int(y / 100) + int(y / 400) + t[m] + d) % 7
}
function check(wd, mon, d, y,    i, want) {
    i = index(MONTHS, mon)
    if (i == 0 || (i - 1) % 3 != 0) {
        printf "%s:%d: unknown month %s\n", FILENAME, FNR, mon
        return
    }
    want = substr(DAYS, weekday(y, (i - 1) / 3 + 1, d) * 3 + 1, 3)
    if (wd != want) {
        printf "%s:%d: weekday %s does not match the date (expected %s)\n", FILENAME, FNR, wd, want
    }
}
FNR == 1 { in_changelog = 0 }
BEGIN { DAYS = "SunMonTueWedThuFriSat"; MONTHS = "JanFebMarAprMayJunJulAugSepOctNovDec" }
# RPM:    * Mon Sep 14 2026 Name <mail> - 0.4.1-1
/^\* [A-Z][a-z][a-z] [A-Z][a-z][a-z] [0-9]+ [0-9][0-9][0-9][0-9] / && in_changelog {
    check($2, $3, $4 + 0, $5 + 0)
}
/^%changelog/ { in_changelog = 1 }
# Debian: " -- Name <mail>  Mon, 14 Sep 2026 12:00:00 +0200"
/^ -- / {
    match($0, /  [A-Z][a-z][a-z], [0-9]+ [A-Z][a-z][a-z] [0-9][0-9][0-9][0-9] /)
    if (RSTART == 0) {
        printf "%s:%d: no RFC 2822 date in the trailer line\n", FILENAME, FNR
        next
    }
    split(substr($0, RSTART + 2, RLENGTH - 3), f, /,? /)
    check(f[1], f[3], f[2] + 0, f[4] + 0)
}
/TODO: describe the release/ { printf "%s:%d: release notes still missing (TODO)\n", FILENAME, FNR }
' "$REPO/packaging/rpm/wusel.spec" "$REPO/packaging/deb/debian/changelog" \
    "$REPO/documentation/modules/ROOT/pages/project/changelog.adoc" |
    sed 's/^/WARNING: /' >&2
