/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2026 IT Beratung Hermann GmbH
 */

// Per-file status over the wusel status socket — the channel that replaces
// reading `user.wusel.state` off the mount.
//
// Why not the extended attribute: a status written onto the file travels with
// the file. Copy one out of the mount and the copy carries a sync state it has
// no business having, because it is not synced at all. The engine's answer to
// "what is this object's status" belongs to the engine, and asking over a
// socket is what keeps it there. It is also one protocol for every platform —
// the macOS File Provider already speaks it — instead of one channel per file
// manager. See documentation/.../explanation/status-protocol.adoc.
//
// The cost of that is latency: a file manager asks per *visible* file, in its
// draw path, and a socket round-trip is more than a syscall. Hence the cache
// below, which the daemon's FileChanged signal keeps honest.

#pragma once

#include <glib.h>

// One object's status, as the socket reports it. Both strings are the wire's
// own spellings, and the emblem table maps them directly.
typedef struct
{
    // "online-only" | "cached" | "pinned" | "pinned-stale" | "modified" |
    // "uploading" | "sync-error", or empty when the object has no content state
    // (a plain directory).
    char state[24];
    // "group_folder" for a Team/Group folder's root, else "plain".
    char kind[24];
} WuselStatus;

// Ask the socket about `path` (an absolute path inside a wusel mount).
//
// Returns FALSE when the path is not on a wusel mount, no daemon is serving, or
// the daemon did not answer in time. There is no second channel to fall back
// to, so the caller simply draws nothing. A miss is cheap and silent: a file
// manager draws directories full of files that are none of ours.
gboolean wusel_status_get(const char *path, WuselStatus *out);

// Drop `path` from the cache, so the next query asks the daemon again. Called
// from the FileChanged handler: the signal that used to mean "re-read this
// file" now means "this cache entry is stale".
void wusel_status_invalidate(const char *path);

// Release the connection, the cache and the discovered mount table.
void wusel_status_shutdown(void);
