<!-- SPDX-License-Identifier: Apache-2.0 -->
# Wusel on the Open Build Service

Not set up yet — this needs a personal [build.opensuse.org](https://build.opensuse.org)
account (see [`../README.md`](../README.md)'s account table). This documents
what to do once one exists, so setting it up is an afternoon, not a research
project.

## Why OBS, and what it adds

One project, building **both** the RPM and the DEB, for Fedora, Debian,
Ubuntu and openSUSE, hosting the resulting repositories — so `dnf`/`apt`
users get an update channel instead of downloading each release by hand.
Despite the name it is not openSUSE-specific; it is a general-purpose build
service, and the reason this project reaches for it over Fedora-only COPR or
Debian-only Launchpad.

## What NOT to do: source services

OBS supports pulling source straight from a git URL at build time
(`obs-service-tar_scm`, plus `obs-service-cargo` for vendoring Rust
dependencies specifically). Do not reach for this. It duplicates work
`build-rpm.sh`/`build-deb.sh` already do — both already produce a complete,
offline-buildable source package (SRPM with a vendored dependency tarball;
a native `.deb` source tree with `vendor/` inside it) — and it is a second,
independent thing to get right and keep working, tested against a live OBS
project this repository has no access to verify.

Upload what the scripts already produce instead. Simpler, and already
verified end to end (see [`../rpm/README.md`](../rpm/README.md) and
[`../deb/README.md`](../deb/README.md)).

## Setup, once an account exists

1. Install `osc` (the OBS command-line client) and configure it with the
   account's credentials.
2. Create the project (via the web UI or `osc meta prj`), with one repository
   per target — Fedora (matching the version in
   [`../rpm/Containerfile`](../rpm/Containerfile)), Debian trixie, the current
   Ubuntu LTS, and openSUSE Tumbleweed/Leap — each with `x86_64` and
   `aarch64` enabled.
3. Create a `wusel` package inside it, once per repository family, or one
   package with both `wusel.spec` and `debian/control` checked in — OBS
   builds each repository with whichever recipe matches its format.
4. For each release:
   ```sh
   ./packaging/rpm/build-rpm.sh   # produces the SRPM (as a side effect, in
                                   # $RPMTOP/SRPMS before it gets rebuilt —
                                   # see the script if this needs exposing as
                                   # a first-class output)
   ./packaging/deb/build-deb.sh   # produces dist/*.deb directly; OBS wants
                                   # the *source* package instead — the
                                   # staged tree build-deb.sh builds from,
                                   # before dpkg-buildpackage, is what to
                                   # `osc add`/`osc commit`
   osc checkout <project>/wusel
   # copy the produced SRPM / source tree in, osc add, osc commit
   ```
   The exact copy step is worth scripting (`build-obs.sh`?) once someone is
   actually doing this by hand a second time — not written speculatively here
   against an OBS project that does not exist yet.

## COPR, cheaply, once this works

COPR is deliberately not a prerequisite for any of this — see the packaging
plan. Once the spec builds from source (already true), pointing COPR at the
same SRPM is small, additional work for Fedora users specifically, purely so
they get a `copr.fedorainfracloud.org` URL instead of an `opensuse.org` one.
