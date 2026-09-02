<!-- SPDX-License-Identifier: Apache-2.0 -->
# Wusel RPM packaging

Builds a Fedora `.rpm` for Wusel: the `wusel` binary, the systemd *user* unit,
the native Nautilus extension with its emblem icons, the GNOME Shell search
provider, and the default cloud-provider (sidebar) registration.

## Build

On a **Fedora** machine, with `rpm-build gcc make pkgconf nautilus-devel
glib2-devel fuse3-devel` **and `rust cargo` installed via `dnf`** (not only
via `mise` or otherwise on `PATH` — see the comment in `build-rpm.sh` for why
`rpmbuild --rebuild` needs the `dnf` packages specifically, no matter what
else provides a working `cargo`):

```sh
sudo dnf install rpm-build gcc make pkgconf-pkg-config nautilus-devel \
    glib2-devel fuse3-devel rust cargo
./packaging/rpm/build-rpm.sh
```

From a **macOS** host (no Fedora needed — builds in a Fedora container via
podman, same plumbing as the `fuse-*` tasks):

```sh
mise run rpm
```

Either way the result lands in `./dist/*.rpm`.

## How it works

`wusel.spec` builds **from source**, in the buildroot, offline — the same
thing a build service (COPR, OBS, `mock`) does: `%build` runs `cargo build
--offline` and `make -C integration/nautilus`, `%install` stages the result.
`build-rpm.sh` is the entry point, but it does not compile anything itself —
it produces the two sources the spec needs (`git archive` for the tree, `cargo
vendor` for the dependencies — the one point in the whole build that touches
the network), builds an SRPM, and then runs `rpmbuild --rebuild` on it. That
second step is exactly what a build service performs, so this script doubles
as the regression test for submitting the SRPM elsewhere. Architecture-specific
(`x86_64`/`aarch64`); no debuginfo subpackage (see the comment in the spec).

Layout installed by the RPM:

| Path | Contents |
|------|----------|
| `/usr/bin/wusel` | the daemon/CLI binary |
| `/usr/lib/systemd/user/wusel@.service` | per-account mount unit (`wusel@<account>`) |
| `/usr/lib64/nautilus/extensions-4/libwusel-nautilus.so` | Nautilus extension |
| `/usr/share/icons/hicolor/scalable/emblems/wusel-emblem-*.svg` | state emblems |
| `/usr/share/icons/hicolor/scalable/apps/at.itbh.Wusel.svg` | app icon (`.desktop` + sidebar entry) |
| `/usr/share/applications/at.itbh.Wusel.desktop` | search-provider launcher app |
| `/usr/share/dbus-1/services/at.itbh.Wusel.SearchProvider.service` | search D-Bus activation |
| `/usr/share/gnome-shell/search-providers/wusel-search-provider.desktop` | search registration |
| `/usr/share/applications/org.freedesktop.CloudProviders.wusel.default.desktop` | sidebar registration (default account) |

Icon cache and desktop-database refreshes rely on Fedora's own RPM file
triggers. The systemd unit is a template with no system-wide preset — each user
enables their own instance.

Verified by installing the built RPM into a clean `fedora:44` container and
running `wusel --version`: 5 packages total (`wusel` + `fuse3`/`fuse-common`/
`fuse3-libs`/`nautilus-extensions`) — `Suggests`, not `Recommends`, on
`nautilus`/`gnome-shell` is what keeps that count that low on a non-GNOME host.

## Install & first run

See the **Installation** page in the documentation (`documentation/`, page
`installation.adoc`) for the end-to-end steps: `dnf install`, `wusel login`,
`systemctl --user enable --now wusel@default`, and the log-out/in that makes
Nautilus and the search provider pick everything up.

## Possible future refinement

The Nautilus/emblem files could move into a `wusel-nautilus` subpackage so
non-GNOME installs skip the `nautilus` dependency. For the current
Fedora-Workstation target it is one package.

Now that the spec builds from source, submitting the SRPM `build-rpm.sh`
produces to a build service (COPR, for a `dnf`-installable repo instead of a
manually downloaded release RPM) is mechanical, not a separate engineering
effort — the same SRPM `rpmbuild --rebuild` already exercises locally.
