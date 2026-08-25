# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 IT Beratung Hermann GmbH

# Build + install the native Nautilus extension. No scripting runtime: this
# produces a single shared object Nautilus loads directly.
#
#   make            # build libwusel-nautilus.so
#   make install    # install into Nautilus's extension dir (needs root system-wide)
#   make uninstall
#
# Requires the Nautilus and GLib development packages:
#   Fedora:        sudo dnf install nautilus-devel glib2-devel gcc make
#   Debian/Ubuntu: sudo apt install libnautilus-extension-dev libglib2.0-dev gcc make

# Nautilus 4 (GTK4) ships libnautilus-extension-4; fall back to the unversioned
# name on older setups.
PKG := $(shell pkg-config --exists libnautilus-extension-4 \
         && echo libnautilus-extension-4 || echo libnautilus-extension)

CC      ?= cc
CFLAGS  ?= -O2 -Wall -Wextra
CFLAGS  += -fPIC $(shell pkg-config --cflags $(PKG) gio-2.0)
LDLIBS  := $(shell pkg-config --libs $(PKG) gio-2.0)

# Nautilus advertises its extension dir in the .pc file (e.g.
# /usr/lib64/nautilus/extensions-4). DESTDIR supports staged/packaged installs.
EXTDIR  := $(shell pkg-config --variable=extensiondir $(PKG))

# Where the emblem SVGs go. The hicolor theme is the cross-desktop icon home;
# override ICONDIR for a non-standard prefix.
ICONDIR   ?= /usr/share/icons/hicolor
EMBLEMDIR := $(ICONDIR)/scalable/emblems

LIB := libwusel-nautilus.so

$(LIB): wusel-nautilus.c
	$(CC) $(CFLAGS) -shared -o $@ $< $(LDLIBS)

install: $(LIB)
	@test -n "$(EXTDIR)" || { echo "could not determine Nautilus extension dir"; exit 1; }
	install -d "$(DESTDIR)$(EXTDIR)"
	install -m 0644 $(LIB) "$(DESTDIR)$(EXTDIR)/$(LIB)"
	install -d "$(DESTDIR)$(EMBLEMDIR)"
	install -m 0644 emblems/wusel-emblem-*.svg "$(DESTDIR)$(EMBLEMDIR)/"
ifeq ($(DESTDIR),)
	# Refresh the icon cache so Nautilus finds the new emblems immediately —
	# only for a direct, live install. A DESTDIR build stages files for a
	# package (RPM/AUR/DEB), which must own only what it ships; baking a
	# generated icon-theme.cache in here made a real AUR install fail with
	# "exists in filesystem" against the hicolor-icon-theme package, which
	# owns that file system-wide. A packaged install relies on the
	# hicolor-icon-theme post-install hook to do this instead.
	-gtk4-update-icon-cache -q -t -f "$(ICONDIR)" 2>/dev/null \
	  || gtk-update-icon-cache -q -t -f "$(ICONDIR)" 2>/dev/null || true
endif
	@echo ">> Installed. Restart Nautilus: nautilus -q"

uninstall:
	rm -f "$(DESTDIR)$(EXTDIR)/$(LIB)"
	rm -f "$(DESTDIR)$(EMBLEMDIR)"/wusel-emblem-*.svg
ifeq ($(DESTDIR),)
	-gtk4-update-icon-cache -q -t -f "$(ICONDIR)" 2>/dev/null \
	  || gtk-update-icon-cache -q -t -f "$(ICONDIR)" 2>/dev/null || true
endif
	@echo ">> Removed. Restart Nautilus: nautilus -q"

clean:
	rm -f $(LIB)

.PHONY: install uninstall clean
