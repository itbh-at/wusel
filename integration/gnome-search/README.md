<!-- SPDX-License-Identifier: Apache-2.0 -->
# GNOME Shell search provider

Makes the GNOME **Activities overview** search your Nextcloud directly — via the
server's Unified Search (including server-side full text, if the
`fulltextsearch` app is installed) — instead of a local file indexer walking the
mount. This is why the mount is excluded from Tracker by default (opening a file
caches it, so indexing it would be a download storm).

## How it works

- `wusel search-provider` is a D-Bus service implementing
  `org.gnome.Shell.SearchProvider2`. GNOME Shell **activates it on demand** (no
  running mount needed — it loads the account credentials itself).
- On a query it calls `GET {server}/ocs/v2.php/search/providers/files/search`.
- Clicking a result opens the file **locally** in the mount
  (`~/Nextcloud/<path>`) when the path resolves and exists, otherwise the
  Nextcloud **web** page.

## Install (system-wide, needs root)

```sh
sudo ./install.sh          # or --uninstall
```

It installs three files:

| File | Destination |
|------|-------------|
| `at.itbh.Wusel.SearchProvider.service` | `/usr/share/dbus-1/services/` (D-Bus activation) |
| `wusel-search-provider.desktop` | `/usr/share/gnome-shell/search-providers/` (registration) |
| `at.itbh.Wusel.desktop` | `/usr/share/applications/` (the `DesktopId` the registration references) |

Then restart GNOME Shell (log out/in on Wayland; `Alt+F2` → `r` on X11) and, if
needed, enable it under **Settings → Search**.

## Notes

- The `.service` `Exec=` must be the **absolute** path to `wusel` (default
  `/usr/local/bin/wusel`; packaging uses `/usr/bin`). Adjust it to match your
  install, or the provider will not start.
- Uses the **default** account. Multi-account search is a later refinement.
- Speed is dominated by the server's Unified Search (slow on large instances /
  trees with `node_modules` etc.). The provider only queries once the term is ≥ 3
  characters and bounds each request to ~12 s, so a slow search returns empty
  rather than hanging the overview — but the real fix is server-side (tune the
  `fulltextsearch` index, exclude bulky dirs).
- This is a first cut; if results or "open" behave oddly, capture a raw response
  to refine parsing:
  `curl -s -u USER:APP-PW -H "OCS-APIRequest: true" -H "Accept: application/json" "$SERVER/ocs/v2.php/search/providers/files/search?term=foo" | jq .`
