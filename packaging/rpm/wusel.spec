# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH
#
# RPM spec for Wusel. This packages *prebuilt* artefacts: the release binary and
# the native Nautilus extension are compiled by packaging/rpm/build-rpm.sh (with
# the mise-pinned toolchain), staged into a source tarball, and installed here.
# Building the Rust code inside rpmbuild's sandbox is deliberately avoided so the
# pinned toolchain and the packaging stay decoupled and reproducible.

# Version is injected by build-rpm.sh: rpmbuild --define "wusel_version X.Y.Z".
%global wusel_version %{?wusel_version}%{!?wusel_version:0.0.0}

# No debuginfo/-source subpackage: the tarball ships a stripped binary, there are
# no source files in the buildroot for the debug extractor to work with.
%global debug_package %{nil}

Name:           wusel
Version:        %{wusel_version}
Release:        1%{?dist}
Summary:        Virtual Nextcloud filesystem — Nextcloud, woven into your desktop

License:        Apache-2.0
URL:            https://itbh.at/
Source0:        %{name}-%{version}.tar.gz

# Prebuilt binary + .so, so no compiler/toolchain BuildRequires. The tarball is
# already architecture-specific.
ExclusiveArch:  x86_64 aarch64

# fusermount3 (unprivileged mount) at runtime. libfuse3 / libnautilus-extension
# come in automatically as auto-generated soname dependencies of the binary/.so.
Requires:       fuse3
# The Nautilus extension and GNOME Shell search provider live in this package;
# they are inert without their host, so pull them softly rather than hard.
Recommends:     nautilus
Recommends:     gnome-shell

%description
Wusel makes a Nextcloud appear as an ordinary folder on Linux — VFS-first:
files are online-only by default and are fetched on access (on-demand
hydration), instead of mirroring everything locally. It is more than just FUSE:
this package also ships the GNOME/Nautilus integration (per-file emblems, a
pin/unpin context menu, sidebar cloud-provider status) and a GNOME Shell search
provider backed by Nextcloud Unified Search.

The mount runs as a systemd *user* service, one instance per account:
    wusel login https://cloud.example.org
    systemctl --user enable --now wusel@default
Then log out and back in so Nautilus loads the extension and GNOME Shell picks
up the search provider. See the Installation page in the documentation.

%prep
%setup -q

%build
# Nothing to build: artefacts are prebuilt and staged into the tarball.

%install
mkdir -p %{buildroot}
cp -a usr %{buildroot}/

%postun
# On a real uninstall ($1 == 0, not an upgrade) remove the cloud-provider
# registrations the *binary* wrote at runtime for named accounts — the package
# never owned those, so rpm would otherwise leave them orphaned in /usr. The
# default account's file is packaged and already gone with the rest.
if [ "$1" -eq 0 ]; then
    rm -f %{_datadir}/applications/org.freedesktop.CloudProviders.wusel.*.desktop
    update-desktop-database %{_datadir}/applications >/dev/null 2>&1 || :
fi

%files
%license LICENSE
%doc README.md
%{_bindir}/wusel
# systemd user unit template (per-account instances: wusel@<account>).
# Path hardcoded so the build needs no systemd-rpm-macros for the unit-dir macro.
/usr/lib/systemd/user/wusel@.service
# Native Nautilus extension + its emblem icons.
%{_libdir}/nautilus/extensions-4/libwusel-nautilus.so
%{_datadir}/icons/hicolor/scalable/emblems/wusel-emblem-*.svg
# GNOME Shell search provider registration + its launcher app.
%{_datadir}/applications/at.itbh.Wusel.desktop
%{_datadir}/dbus-1/services/at.itbh.Wusel.SearchProvider.service
%{_datadir}/gnome-shell/search-providers/wusel-search-provider.desktop
# Cloud-provider (Nautilus sidebar) registration for the default account.
%{_datadir}/applications/org.freedesktop.CloudProviders.wusel.default.desktop

# Icon cache and desktop database are refreshed by Fedora's own RPM file
# triggers (hicolor-icon-theme / desktop-file-utils) when files land in those
# dirs — no per-package scriptlet needed. The systemd user unit is a template
# with no system-wide preset to apply; each user enables their own instance.

%changelog
* Wed Aug 19 2026 Christoph D. Hermann <christoph.hermann@itbh.at> - 0.2.0-1
- See the Changelog page in the documentation for the full notes; in brief:
- Uploads are asynchronous: a save returns once the change is durable locally
  and the upload runs in the background, with automatic retries for transient
  failures and a parked state plus a notification for permanent ones. Set
  [sync] upload = sync for the old behaviour.
- New `wusel status`: what the mount is doing right now, by file name, including
  uploads still owed to the server. `--watch` redraws once a second.
- The desktop says when the server cannot be reached, and when it is back.
- Stopping the mount takes seconds instead of running into a systemd timeout.
- Opening a file no longer hangs when a reader arrives at a flow being given up.
- A background refresh no longer makes a read wait behind it, and a small file
  is cached once rather than on every read.
- A network hiccup at start-up no longer costs live updates for the session.
- A missing keyring entry is reported as missing, not as a broken keyring.

- Concurrency: every FUSE callback is now an intent handed to a state machine
  that decides and performs no I/O, with database readers, a single writer, and
  network and file pools underneath. 0.1.0 served one request at a time.
- Whole-file hydration is a single streamed GET instead of one range GET per
  chunk.
- Atomic saves (GNOME Text Editor and anything on g_file_replace) work: a rename
  replaces its destination and inherits its server identity, instead of failing
  with EIO and then producing a conflicted copy.
- A background listing refresh no longer makes the next caller wait for it.
- The file-manager emblem changes on every route: a file arriving in the cache
  (read, hydration or write) and a file leaving it (eviction, unpin) are both
  announced.
- The kernel is told when a file's content changed on the server, so reopening
  shows the current version rather than a cached one.
- A failed operation says what failed in the log, not only an errno.
- Pins live in <config>/pins.json instead of the state database: they survive
  `cache clear`, a rebuilt database, and a roaming home directory.
- The state database is moved off NFS/CIFS to local storage, loudly, because
  SQLite cannot lock reliably there; override with [state] db_path.
- Stale pinned files: a fifth state, an "Update now" action, and
  [sync] refresh_pinned = manual | ask | auto, where auto fetches only on an
  unmetered connection.
- [sync] open_pinned = newest | newest-unmetered | offline chooses what opening
  an outdated pinned file serves; an outdated offline copy is read-only.
- A pinned file stays readable when the server is unreachable, even if its copy
  is out of date.
- Nautilus context-menu entries are prefixed "Wusel - ".

* Tue Jul 28 2026 Christoph D. Hermann <christoph.hermann@itbh.at> - 0.1.0-1
- First public release: VFS-first Nextcloud mount (online-only, on-demand
  hydration, caching, pinning, write-back) with the GNOME desktop integration
  (Nautilus sidebar + emblems + pin/unpin menu, Shell search) and keyring
  credentials by default.

* Mon Jul 27 2026 Christoph D. Hermann <christoph.hermann@itbh.at> - 0.0.1-1
- Initial RPM: wusel binary, systemd user unit, Nautilus extension + emblems,
  GNOME Shell search provider, and the default cloud-provider registration.
