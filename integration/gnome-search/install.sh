#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Install the GNOME Shell search provider registration (system-wide → needs
# root). GNOME Shell then D-Bus-activates `wusel search-provider` on demand,
# which queries Nextcloud Unified Search. Requires `wusel` on an absolute path
# matching the .service file (default /usr/local/bin/wusel).
#
#   sudo ./install.sh            # install
#   sudo ./install.sh --uninstall
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
app_dir=/usr/share/applications
dbus_dir=/usr/share/dbus-1/services
prov_dir=/usr/share/gnome-shell/search-providers

app=at.itbh.Wusel.desktop
svc=at.itbh.Wusel.SearchProvider.service
prov=wusel-search-provider.desktop

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$app_dir/$app" "$dbus_dir/$svc" "$prov_dir/$prov"
    echo ">> Removed. Restart GNOME Shell (log out/in on Wayland) to apply."
    exit 0
fi

install -Dm644 "$here/$app"  "$app_dir/$app"
install -Dm644 "$here/$svc"  "$dbus_dir/$svc"
install -Dm644 "$here/$prov" "$prov_dir/$prov"

echo ">> Installed. Check the Exec path in $dbus_dir/$svc points at your wusel."
echo ">> Restart GNOME Shell to load it: log out/in (Wayland), or Alt+F2 → r (X11)."
echo ">> Then search in the Activities overview; enable it under Settings → Search if needed."
