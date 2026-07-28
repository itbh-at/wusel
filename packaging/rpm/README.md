<!-- SPDX-License-Identifier: Apache-2.0 -->
# Wusel RPM packaging

Builds a Fedora `.rpm` for Wusel: the `wusel` binary, the systemd *user* unit,
the native Nautilus extension with its emblem icons, the GNOME Shell search
provider, and the default cloud-provider (sidebar) registration.

## Build

On a **Fedora** machine (needs `rpm-build gcc make pkgconf nautilus-devel
glib2-devel fuse3-devel`, plus a Rust toolchain — `mise` is used if present):

```sh
./packaging/rpm/build-rpm.sh
```

From a **macOS** host (no Fedora needed — builds in a Fedora container via
podman, same plumbing as the `fuse-*` tasks):

```sh
mise run rpm
```

Either way the result lands in `./dist/*.rpm`.

## How it works

The Rust build (mise-pinned toolchain) and the `.so` (`make`) are compiled
first, then staged into a source tarball that [`wusel.spec`](wusel.spec)
installs. Building the Rust code inside rpmbuild's sandbox is deliberately
avoided, so the pinned toolchain and the packaging stay decoupled. The build is
architecture-specific (`x86_64`/`aarch64`); there is no debuginfo subpackage.

Layout installed by the RPM:

| Path | Contents |
|------|----------|
| `/usr/bin/wusel` | the daemon/CLI binary |
| `/usr/lib/systemd/user/wusel@.service` | per-account mount unit (`wusel@<account>`) |
| `/usr/lib64/nautilus/extensions-4/libwusel-nautilus.so` | Nautilus extension |
| `/usr/share/icons/hicolor/scalable/emblems/wusel-emblem-*.svg` | state emblems |
| `/usr/share/applications/at.itbh.Wusel.desktop` | search-provider launcher app |
| `/usr/share/dbus-1/services/at.itbh.Wusel.SearchProvider.service` | search D-Bus activation |
| `/usr/share/gnome-shell/search-providers/wusel-search-provider.desktop` | search registration |
| `/usr/share/applications/org.freedesktop.CloudProviders.wusel.default.desktop` | sidebar registration (default account) |

Icon cache and desktop-database refreshes rely on Fedora's own RPM file
triggers. The systemd unit is a template with no system-wide preset — each user
enables their own instance.

## Install & first run

See the **Installation** page in the documentation (`documentation/`, page
`installation.adoc`) for the end-to-end steps: `dnf install`, `wusel login`,
`systemctl --user enable --now wusel@default`, and the log-out/in that makes
Nautilus and the search provider pick everything up.

## Possible future refinement

The Nautilus/emblem files could move into a `wusel-nautilus` subpackage so
non-GNOME installs skip the `nautilus` dependency. For the current
Fedora-Workstation target it is one package.
