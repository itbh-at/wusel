# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH
#
# RPM spec for Wusel. Builds from source, in the buildroot, with no network
# access — the only thing a chroot-based build service (COPR, OBS, `mock`)
# will ever run. Source0 is the source tree (packaging/rpm/build-rpm.sh
# produces it with `git archive`); Source1 is `cargo vendor`'s output for the
# same Cargo.lock, so `%build` can pass `--offline` and mean it.
#
# build-rpm.sh remains the entry point for a local/CI build: it generates both
# sources, builds the SRPM, and rebuilds it with `rpmbuild --rebuild` — the
# same two steps a build service performs, so a spec regression is caught
# here rather than first on a submission to COPR/OBS.

# Version is injected by build-rpm.sh: rpmbuild --define "wusel_version X.Y.Z".
%global wusel_version %{?wusel_version}%{!?wusel_version:0.0.0}

# No debuginfo/-source subpackage: rpmbuild's debug extractor wants build-id
# links this project's plain `cargo build` does not produce, and a from-source
# build's debug story is a separate piece of work, not a byproduct of getting
# `dnf install` to work.
%global debug_package %{nil}

Name:           wusel
Version:        %{wusel_version}
Release:        1%{?dist}
Summary:        Virtual Nextcloud filesystem — Nextcloud, woven into your desktop

License:        Apache-2.0
URL:            https://itbh.at/
Source0:        %{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.gz

ExclusiveArch:  x86_64 aarch64

BuildRequires:  rust >= 1.85
BuildRequires:  cargo
BuildRequires:  gcc
BuildRequires:  make
BuildRequires:  pkgconf-pkg-config
BuildRequires:  nautilus-devel
BuildRequires:  glib2-devel
BuildRequires:  fuse3-devel

# fusermount3 (unprivileged mount) at runtime. libfuse3 / libnautilus-extension
# come in automatically as auto-generated soname dependencies of the binary/.so.
Requires:       fuse3
# The Nautilus extension and GNOME Shell search provider live in this package;
# they are inert without their host. `Suggests` rather than `Recommends`, because
# dnf installs weak dependencies by default: on a desktop these are already
# present and the difference is invisible, but on a server, a minimal install or
# a KDE machine `Recommends` drags in the whole GNOME stack — several hundred
# packages — for a mount that needs `fuse3` and nothing else.
Suggests:       nautilus
Suggests:       gnome-shell

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
# Source0 unpacks to %{name}-%{version} (built with `git archive
# --prefix=%{name}-%{version}/`); Source1 is `cargo vendor`'s own `vendor/`
# directory, added into that same tree with `-T -D -a 1` (skip the default
# unpack-and-cd, keep the directory %setup already created, just extract
# Source1 into it).
%setup -q
%setup -q -T -D -a 1

mkdir -p .cargo
cat > .cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
EOF

%build
# A relative target dir, regardless of what the caller's shell has set
# ($CARGO_TARGET_DIR is a convenience in the dev containers, to keep the
# macOS/Linux/RPM builds from sharing one target dir over the same bind
# mount) — %install below relies on this exact, buildroot-relative path.
export CARGO_TARGET_DIR=target
cargo build --release --offline --features fuse -p wusel
make -C integration/nautilus %{?_smp_mflags}

%install
LIBDIR=%{_libdir}
install -Dm755 target/release/wusel %{buildroot}%{_bindir}/wusel
install -Dm644 packaging/rpm/wusel@.service %{buildroot}/usr/lib/systemd/user/wusel@.service
install -Dm755 integration/nautilus/libwusel-nautilus.so %{buildroot}${LIBDIR}/nautilus/extensions-4/libwusel-nautilus.so

install -d %{buildroot}%{_datadir}/icons/hicolor/scalable/emblems
install -m644 integration/nautilus/emblems/wusel-emblem-*.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/emblems/

install -d %{buildroot}%{_datadir}/icons/hicolor/scalable/apps
install -m644 integration/icons/at.itbh.Wusel.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/

install -d %{buildroot}%{_datadir}/applications
install -d %{buildroot}%{_datadir}/dbus-1/services
install -d %{buildroot}%{_datadir}/gnome-shell/search-providers

# Search-provider launcher app + registration; the .service Exec is rewritten
# from the source default (/usr/local/bin) to the packaged path.
install -m644 integration/gnome-search/at.itbh.Wusel.desktop %{buildroot}%{_datadir}/applications/
install -m644 integration/gnome-search/wusel-search-provider.desktop %{buildroot}%{_datadir}/gnome-shell/search-providers/
sed 's#Exec=.*/wusel search-provider#Exec=%{_bindir}/wusel search-provider#' \
    integration/gnome-search/at.itbh.Wusel.SearchProvider.service \
    > %{buildroot}%{_datadir}/dbus-1/services/at.itbh.Wusel.SearchProvider.service

# Cloud-provider (Nautilus sidebar) registration for the default account —
# generated by the freshly built binary so it never drifts from the code.
# Runs the buildroot copy directly: %install is an ordinary build-machine
# shell script, not a chroot into the buildroot, so this is no different from
# running any other freshly built tool during packaging.
%{buildroot}%{_bindir}/wusel desktop install-provider --account default --dir %{buildroot}%{_datadir}/applications >/dev/null
# install-provider runs update-desktop-database, which drops a mimeinfo.cache
# in the applications dir. That is not ours to ship (Fedora's
# desktop-file-utils trigger regenerates it on install), and rpmbuild rejects
# unpackaged files — so remove it from the buildroot.
rm -f %{buildroot}%{_datadir}/applications/mimeinfo.cache

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
# App icon — resolved by the .desktop file and the cloud-provider sidebar entry.
%{_datadir}/icons/hicolor/scalable/apps/at.itbh.Wusel.svg
# GNOME Shell search provider registration + its launcher app. gnome-shell
# is only Suggested (see above), so this package must own the directory
# itself rather than assume gnome-shell's own package created it — openSUSE's
# build validation (unlike Fedora's) rejects an unowned directory outright.
%{_datadir}/applications/at.itbh.Wusel.desktop
%{_datadir}/dbus-1/services/at.itbh.Wusel.SearchProvider.service
%dir %{_datadir}/gnome-shell
%dir %{_datadir}/gnome-shell/search-providers
%{_datadir}/gnome-shell/search-providers/wusel-search-provider.desktop
# Cloud-provider (Nautilus sidebar) registration for the default account.
%{_datadir}/applications/org.freedesktop.CloudProviders.wusel.default.desktop

# Icon cache and desktop database are refreshed by Fedora's own RPM file
# triggers (hicolor-icon-theme / desktop-file-utils) when files land in those
# dirs — no per-package scriptlet needed. The systemd user unit is a template
# with no system-wide preset to apply; each user enables their own instance.

%changelog
* Wed Aug 26 2026 Christoph D. Hermann <christoph.hermann@itbh.at> - 0.3.1-1
- The Debian and Ubuntu packages build again; the documentation installs from
  the signed repository rather than from a downloaded file.

* Tue Aug 25 2026 Christoph D. Hermann <christoph.hermann@itbh.at> - 0.3.0-1
- Debian/Ubuntu and Arch packages join the Fedora RPM, built from the same
  recipes and published through the Open Build Service.
- The documentation is restructured onto the Diataxis framework.

* Mon Aug 24 2026 Christoph D. Hermann <christoph.hermann@itbh.at> - 0.2.2-1
- The GNOME hosts are suggested rather than recommended, so installing on a
  machine without GNOME no longer pulls in the whole desktop stack.

* Mon Aug 24 2026 Christoph D. Hermann <christoph.hermann@itbh.at> - 0.2.1-1
- Wusel ships its own application icon; the file-manager sidebar entry and the
  GNOME search entry no longer fall back to the generic folder-remote icon.

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
