<!-- SPDX-License-Identifier: Apache-2.0 -->
# wusel Nautilus extension

A native `libnautilus-extension` module (a `.so`, no scripting runtime) that
integrates the virtual Nextcloud mount into GNOME Files (Nautilus). It reads each
file's state from the FUSE xattr `user.wusel.state` and adds:

- **Emblems** (`InfoProvider`), one per state (the OneDrive model), shipped as our
  own icons under `emblems/` since current Adwaita has no suitable stock ones:
  `online-only` → cloud, `cached` → cloud with a check (downloaded, still
  evictable), `pinned` → green check (kept offline), `modified` → up-arrow
  (pending upload).
- **Context-menu actions** (`MenuProvider`): a per-file toggle — *Make Available
  Offline* on an online-only/cached file, *Free Up Space* on a pinned one — which
  runs `wusel pin`/`unpin`. Labels are localized (currently de/en, from the
  locale). `wusel` is located on `PATH` or in `/usr/local/bin`, `/usr/bin`,
  `~/.local/bin`. When the command finishes, the file's emblem is refreshed
  (`invalidate_extension_info`) — no manual reload.

## Build & install

Development packages:

- Fedora: `sudo dnf install nautilus-devel glib2-devel gcc make`
- Debian/Ubuntu: `sudo apt install libnautilus-extension-dev libglib2.0-dev gcc make`

```sh
make
sudo make install   # into $(pkg-config --variable=extensiondir libnautilus-extension-4)
nautilus -q         # restart Nautilus so it loads the extension
```

`make uninstall` removes it. Packaging (RPM/DEB) builds and installs the same
`.so`, so end users never run `make`.

## Notes

- `make install` also installs the emblem SVGs into the hicolor icon theme
  (`ICONDIR`, default `/usr/share/icons/hicolor`) and refreshes the icon cache.
- The `.so` is tied to the Nautilus **major** version (`extensions-4/` for
  Nautilus 4 / GTK4); distro packages build it against their own Nautilus.
- A pin/unpin from the menu refreshes that file's emblem when the command
  finishes. A background hydration (opening a file → cached) is refreshed by the
  daemon's kernel invalidation; if a given desktop does not pick that up live,
  the emblem updates on the next view reload. A daemon→extension push (for fully
  live updates everywhere) is a later refinement.
- The xattr read is a local `getxattr(2)` — the engine guarantees it never
  triggers a network round-trip, so browsing stays cheap.

Full documentation: the **File-manager integration** page in the Antora docs
(`documentation/`).
