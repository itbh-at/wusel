<!-- SPDX-License-Identifier: Apache-2.0 -->
# Wusel DEB packaging

A `debian/` tree for Debian and Ubuntu, building `wusel` and the native
Nautilus extension **from source**, offline, in the buildroot — the same
approach as [the RPM](../rpm), for the same reason: `sbuild`/`pbuilder`/OBS
build in a network-less chroot, so anything not built by `dh` itself there
cannot exist. This is a *native* Debian package (`3.0 (native)`, see
`debian/source/format`) — there is no separate upstream tarball to track,
since this repository *is* the upstream.

## Build

On a **Debian/Ubuntu** machine (needs `build-essential debhelper cargo
rustc libnautilus-extension-dev libglib2.0-dev libfuse3-dev pkgconf`):

```sh
./packaging/deb/build-deb.sh
```

From a **macOS** host (no Debian/Ubuntu needed — builds in a Debian container
via podman, same plumbing as the `fuse-*`/`rpm` tasks):

```sh
mise run deb
```

Either way the result lands in `./dist/*.deb`.

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
building, same as the RPM's `%{_libdir}`).

Only `fuse3` and the auto-detected shared-library deps
(`dpkg-shlibdeps` — `libnautilus-extension4` among them, since the extension
links against it) are hard `Depends`; `nautilus` and `gnome-shell` themselves
are `Suggests` — same reasoning as the RPM's `Suggests` over `Recommends`.
`libnautilus-extension4` being a hard dependency does not defeat this: it is
the small runtime library the `.so` needs to load, not the GTK4 file-manager
application itself, which stays optional.

Verified by installing the built `.deb` into a clean `debian:trixie-slim`
container: `dpkg -L wusel` matches the intended file list exactly, and
`wusel --version` runs.

## Publishing

Not done by this repository. A signed, `apt`-installable repository (a launch
target of its own, e.g. via the same Open Build Service project as the RPM —
see the packaging plan) needs a place to host it and a signing key; neither
exists yet. Until then, `./dist/*.deb` is installed by hand:
`sudo apt install ./wusel_*.deb`.
