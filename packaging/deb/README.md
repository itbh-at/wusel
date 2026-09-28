<!-- SPDX-License-Identifier: Apache-2.0 -->
# Wusel DEB packaging

A `debian/` tree for Debian and Ubuntu, building two binary packages **from
source** — `wusel` (the binary, the systemd user unit and the D-Bus desktop
integration) and `wusel-nautilus` (the native Nautilus extension and its
emblem icons) — offline, in the buildroot — the same
approach as [the RPM](../rpm), for the same reason: `sbuild`/`pbuilder`/OBS
build in a network-less chroot, so anything not built by `dh` itself there
cannot exist. This is a *native* Debian package (`3.0 (native)`, see
`debian/source/format`) — there is no separate upstream tarball to track,
since this repository *is* the upstream.

## Build

On a **Debian/Ubuntu** machine (needs `build-essential debhelper cargo
rustc libnautilus-extension-dev libglib2.0-dev libgtk-4-dev libfuse3-dev pkgconf`):

```sh
./packaging/deb/build-deb.sh
```

From a **macOS** host (no Debian/Ubuntu needed — builds in a Debian container
via podman, same plumbing as the `fuse-*`/`rpm` tasks):

```sh
mise run deb
```

Either way both packages land in `./dist/*.deb`.

## How it works

`packaging/deb/debian/` is **not** at the repository root — dpkg-buildpackage
always expects `./debian` relative to its working directory, so
`build-deb.sh` stages a source tree the same way `build-rpm.sh` stages an
SRPM's sources: `git archive HEAD` for the tree (commit first if you are
iterating on something not yet committed — this mirrors what an actual
release tag would contain), this directory's `debian/` copied to its root,
and `cargo vendor` for the dependencies (the one step that touches the
network) — then `dpkg-buildpackage -b`.

`debian/rules` overrides `dh_auto_configure` to point `.cargo/config.toml` at
the vendored tree, `dh_auto_build` to run `cargo build --offline` and `make -C
integration/nautilus`, and `dh_auto_install` to stage the same file layout as
the RPM (see [`../rpm/README.md`](../rpm/README.md)'s table — identical
except for the multiarch `/usr/lib/<triplet>/nautilus/...` extension path,
which `dpkg-buildpackage`/`pkg-config` resolve for whatever architecture is
building, same as the RPM's `%{_libdir}`). The extension and emblems go into
`debian/wusel-nautilus/` through the Makefile's `install DESTDIR=...` target;
everything else into `debian/wusel/`.

`wusel` depends on `fuse3` and the auto-detected shared-library deps
(`dpkg-shlibdeps`), which for the binary are only libc, libm and libgcc_s:
notifications, the sidebar entry and the search provider speak D-Bus from
inside the binary. It `Suggests` `wusel-nautilus` and `gnome-shell` — same
reasoning as the RPM's `Suggests` over `Recommends`. `wusel-nautilus` depends
on `wusel (= ${binary:Version})` and its own shlibdeps —
`libnautilus-extension4` and GLib, not GTK: the extension is compiled against
GTK but not linked, since the `gdk_*` clipboard calls resolve inside Nautilus.

Debian has no counterpart to the RPM's `Supplements`: nothing installs
`wusel-nautilus` automatically, so a GNOME desktop installs both
(`sudo apt install wusel wusel-nautilus`).

Verified by installing the built `.deb` into a clean `debian:trixie-slim`
container: `dpkg -L wusel` and `dpkg -L wusel-nautilus` match the intended
file lists exactly, and `wusel --version` runs. `wusel` alone installs 4
packages; with `wusel-nautilus`, 13 — none of them GTK.

## Publishing

Not done by this repository. A signed, `apt`-installable repository (a launch
target of its own, e.g. via the same Open Build Service project as the RPM —
see the packaging plan) needs a place to host it and a signing key; neither
exists yet. Until then, `./dist/*.deb` is installed by hand:
`sudo apt install ./wusel_*.deb ./wusel-nautilus_*.deb`.
