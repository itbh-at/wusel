/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2026 IT Beratung Hermann GmbH
 */

// The status-socket client. See wusel-status.h for why this channel exists.
//
// Three things happen here: finding which daemon owns a path, speaking the
// framed protocol to it, and caching the answers so the draw path is not one
// round-trip per visible file.
//
// Everything runs on Nautilus's main thread — module init, update_file_info and
// the FileChanged callback share the default main context — so none of this
// needs locking. It also means nothing here may block for long: a wedged daemon
// must not freeze the file manager, which is what the socket timeouts are for.

#include "wusel-status.h"

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

// How long a request may take before we give up and draw nothing. Generous against a busy daemon, short enough that a wedged one costs a
// noticeable pause rather than a hang. The engine answers a status read out of
// its database without touching the network, by construction.
#define WUSEL_TIMEOUT_MS 300

// The wire's cap, mirrored: a length prefix asking for more than this is a
// corrupt or hostile stream, and must not become an allocation.
#define WUSEL_MAX_FRAME (8u * 1024u * 1024u)

// Cache entries live until the daemon says otherwise (FileChanged) — a status
// does not go stale on its own, only when the engine changes it. The bound is
// there because a long browsing session touches unboundedly many files, and
// nothing removes an entry by itself.
#define WUSEL_CACHE_MAX 4096

// One mount this user has running: where it is, and the socket serving it.
typedef struct
{
    char *root;   // the mountpoint, e.g. /home/u/Wusel
    char *socket; // the daemon's status socket
} WuselMount;

static GArray *mounts;         // WuselMount, discovered once per process
static GHashTable *cache;      // absolute path -> WuselStatus*
static int conn = -1;          // the current connection, or -1
static char *conn_socket;      // which socket `conn` is to

// --- Paths ------------------------------------------------------------------

// `$XDG_STATE_HOME` or `~/.local/state` — the same derivation wusel_core's
// config.rs does by hand, kept in step with it deliberately: one small
// duplicated rule beats a config file the extension would have to parse.
static char *state_home(void)
{
    const char *xdg = g_getenv("XDG_STATE_HOME");
    if (xdg && *xdg)
    {
        return g_strdup(xdg);
    }
    return g_build_filename(g_get_home_dir(), ".local", "state", NULL);
}

// Where the daemon puts its socket: `$XDG_RUNTIME_DIR/wusel`, else the state
// directory. Mirrors `config::ipc_dir`, including its refusal to fall back to a
// world-traversable temp directory.
static char *ipc_dir(void)
{
    const char *run = g_getenv("XDG_RUNTIME_DIR");
    if (run && *run)
    {
        return g_build_filename(run, "wusel", NULL);
    }
    return g_build_filename(state_home(), "wusel", NULL);
}

// Read the mountpoint a daemon recorded for `account`, or NULL. The marker is
// written when the mount comes up and removed on clean exit, so its presence is
// a good — not perfect — sign of a live daemon; a connect failure settles it.
static char *read_marker(const char *state, const char *account)
{
    char *dir = g_str_equal(account, "default")
                    ? g_build_filename(state, "wusel", NULL)
                    : g_build_filename(state, "wusel", "accounts", account, NULL);
    char *marker = g_build_filename(dir, "mountpoint", NULL);
    g_free(dir);

    char *text = NULL;
    if (!g_file_get_contents(marker, &text, NULL, NULL))
    {
        g_free(marker);
        return NULL;
    }
    g_free(marker);
    g_strstrip(text);
    if (*text == '\0')
    {
        g_free(text);
        return NULL;
    }
    return text;
}

static void add_mount(const char *account, const char *state, const char *sockdir)
{
    char *root = read_marker(state, account);
    if (!root)
    {
        return;
    }
    char *name = g_strdup_printf("ipc-%s.sock", account);
    WuselMount m = {.root = root, .socket = g_build_filename(sockdir, name, NULL)};
    g_free(name);
    g_array_append_val(mounts, m);
}

// Find every account this user has a recorded mountpoint for: the default one,
// plus any under `accounts/`. Done once — a mount that appears later is picked
// up when Nautilus is restarted, the same as the extension itself.
static void discover(void)
{
    if (mounts)
    {
        return;
    }
    mounts = g_array_new(FALSE, FALSE, sizeof(WuselMount));

    char *state = state_home();
    char *sockdir = ipc_dir();

    add_mount("default", state, sockdir);

    char *accounts = g_build_filename(state, "wusel", "accounts", NULL);
    GDir *dir = g_dir_open(accounts, 0, NULL);
    if (dir)
    {
        const char *name;
        while ((name = g_dir_read_name(dir)))
        {
            add_mount(name, state, sockdir);
        }
        g_dir_close(dir);
    }
    g_free(accounts);
    g_free(sockdir);
    g_free(state);
}

// Which mount holds `path`, and what is the path relative to its root?
//
// The match is on whole path components: `/home/u/Wusel2/x` must not be taken
// for a child of `/home/u/Wusel`, which a plain prefix test would do.
static const WuselMount *mount_for(const char *path, const char **rel_out)
{
    discover();
    for (guint i = 0; i < mounts->len; i++)
    {
        const WuselMount *m = &g_array_index(mounts, WuselMount, i);
        size_t n = strlen(m->root);
        if (strncmp(path, m->root, n) != 0)
        {
            continue;
        }
        if (path[n] == '\0')
        {
            *rel_out = "/";
            return m;
        }
        if (path[n] == '/')
        {
            *rel_out = path + n;
            return m;
        }
    }
    return NULL;
}

// --- The connection ---------------------------------------------------------

static void disconnect(void)
{
    if (conn >= 0)
    {
        close(conn);
        conn = -1;
    }
    g_clear_pointer(&conn_socket, g_free);
}

// Connect to `socket`, reusing the open connection when it is already the right
// one. Kept open across queries: a directory of files is a burst of them, and a
// connect per file would triple the syscalls for nothing.
static gboolean ensure_conn(const char *socket_path)
{
    if (conn >= 0 && conn_socket && g_str_equal(conn_socket, socket_path))
    {
        return TRUE;
    }
    disconnect();

    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    // A path that does not fit cannot be connected to at all; refusing here
    // keeps a silently truncated path from reaching some *other* socket.
    if (g_strlcpy(addr.sun_path, socket_path, sizeof(addr.sun_path)) >= sizeof(addr.sun_path))
    {
        return FALSE;
    }

    int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0)
    {
        return FALSE;
    }
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0)
    {
        close(fd);
        return FALSE;
    }
    // Bound both directions: this runs on the thread that draws the window.
    struct timeval tv = {.tv_sec = WUSEL_TIMEOUT_MS / 1000,
                         .tv_usec = (WUSEL_TIMEOUT_MS % 1000) * 1000};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
    setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv));

    conn = fd;
    conn_socket = g_strdup(socket_path);
    return TRUE;
}

// write(2)/read(2) loops. A short count is ordinary on a stream socket, and a
// timeout arrives as a short count too — so a partial frame is a failure, and
// the connection is dropped rather than resynchronised: the stream is no longer
// at a frame boundary and nothing good can be read from it again.
static gboolean write_all(int fd, const void *buf, size_t len)
{
    const char *p = buf;
    while (len > 0)
    {
        ssize_t n = write(fd, p, len);
        if (n <= 0)
        {
            if (n < 0 && errno == EINTR)
            {
                continue;
            }
            return FALSE;
        }
        p += n;
        len -= (size_t)n;
    }
    return TRUE;
}

static gboolean read_all(int fd, void *buf, size_t len)
{
    char *p = buf;
    while (len > 0)
    {
        ssize_t n = read(fd, p, len);
        if (n <= 0)
        {
            if (n < 0 && errno == EINTR)
            {
                continue;
            }
            return FALSE;
        }
        p += n;
        len -= (size_t)n;
    }
    return TRUE;
}

// One request, one response frame. The protocol is a 4-byte big-endian length
// followed by that many bytes of JSON.
static char *call(const char *socket_path, const char *request)
{
    if (!ensure_conn(socket_path))
    {
        return NULL;
    }
    size_t len = strlen(request);
    guint32 prefix = GUINT32_TO_BE((guint32)len);
    if (!write_all(conn, &prefix, 4) || !write_all(conn, request, len))
    {
        disconnect();
        return NULL;
    }
    if (!read_all(conn, &prefix, 4))
    {
        disconnect();
        return NULL;
    }
    guint32 n = GUINT32_FROM_BE(prefix);
    if (n == 0 || n > WUSEL_MAX_FRAME)
    {
        disconnect();
        return NULL;
    }
    char *body = g_malloc(n + 1);
    if (!read_all(conn, body, n))
    {
        g_free(body);
        disconnect();
        return NULL;
    }
    body[n] = '\0';
    return body;
}

// --- Just enough JSON -------------------------------------------------------

// Copy the string value of `"key":"..."` into `out`.
//
// A hand-written scan rather than a JSON parser, because what is being read is
// two flat string fields out of a message this project defines and the daemon
// on the other end produced. Pulling in json-glib to find `"state"` would be a
// dependency for one `strstr`. It is deliberately strict — anything that does
// not look exactly like a quoted string after the key is a miss, not a guess —
// and it never writes past `out`.
static gboolean json_string(const char *json, const char *key, char *out, size_t out_len)
{
    char needle[32];
    if (g_snprintf(needle, sizeof(needle), "\"%s\":\"", key) >= (int)sizeof(needle))
    {
        return FALSE;
    }
    const char *p = strstr(json, needle);
    if (!p)
    {
        return FALSE;
    }
    p += strlen(needle);
    const char *end = strchr(p, '"');
    // Our values are plain lower-case identifiers; a backslash means either an
    // escape we do not handle or a field that is not what we thought it was.
    if (!end || memchr(p, '\\', (size_t)(end - p)))
    {
        return FALSE;
    }
    size_t n = (size_t)(end - p);
    if (n >= out_len)
    {
        return FALSE;
    }
    memcpy(out, p, n);
    out[n] = '\0';
    return TRUE;
}

// Escape `s` for use inside a JSON string.
//
// Not `g_strescape`: that renders every byte outside printable ASCII as an
// octal escape (`\303`), which is C's syntax and not JSON's — a single umlaut
// in a file name would have produced a request the daemon could not parse. JSON
// wants UTF-8 passed through untouched, and only the quote, the backslash and
// the C0 controls escaped. Control bytes cannot occur in a path (the kernel
// forbids NUL and they are absurd in a name), but a file manager displays what
// the filesystem hands it, and a malformed request is worse than a long
// function.
static char *json_escape(const char *s)
{
    GString *out = g_string_sized_new(strlen(s) + 8);
    for (const unsigned char *p = (const unsigned char *)s; *p; p++)
    {
        switch (*p)
        {
        case '"':
            g_string_append(out, "\\\"");
            break;
        case '\\':
            g_string_append(out, "\\\\");
            break;
        case '\n':
            g_string_append(out, "\\n");
            break;
        case '\r':
            g_string_append(out, "\\r");
            break;
        case '\t':
            g_string_append(out, "\\t");
            break;
        default:
            if (*p < 0x20)
            {
                g_string_append_printf(out, "\\u%04x", *p);
            }
            else
            {
                // Everything else, UTF-8 included, goes through as it is.
                g_string_append_c(out, (char)*p);
            }
        }
    }
    return g_string_free(out, FALSE);
}

// --- The cache --------------------------------------------------------------

static void cache_put(const char *path, const WuselStatus *st)
{
    if (!cache)
    {
        cache = g_hash_table_new_full(g_str_hash, g_str_equal, g_free, g_free);
    }
    if (g_hash_table_size(cache) >= WUSEL_CACHE_MAX)
    {
        // Every entry is equally valid, so there is no "least useful" one to
        // evict — and this is an optimisation, not state: dropping it all costs
        // one round-trip per file next time it is drawn. Bounded memory in a
        // process the user did not start for us is worth more.
        g_hash_table_remove_all(cache);
    }
    g_hash_table_replace(cache, g_strdup(path), g_memdup2(st, sizeof(*st)));
}

void wusel_status_invalidate(const char *path)
{
    if (cache)
    {
        g_hash_table_remove(cache, path);
    }
}

// --- The query ---------------------------------------------------------------

gboolean wusel_status_get(const char *path, WuselStatus *out)
{
    if (!path || !out)
    {
        return FALSE;
    }
    if (cache)
    {
        const WuselStatus *hit = g_hash_table_lookup(cache, path);
        if (hit)
        {
            *out = *hit;
            return TRUE;
        }
    }

    const char *rel = NULL;
    const WuselMount *m = mount_for(path, &rel);
    if (!m)
    {
        return FALSE; // not one of ours
    }

    char *escaped = json_escape(rel);
    char *request = g_strdup_printf("{\"op\":\"stat\",\"path\":\"%s\"}", escaped);
    g_free(escaped);

    char *reply = call(m->socket, request);
    g_free(request);
    if (!reply)
    {
        return FALSE;
    }

    WuselStatus st;
    st.state[0] = '\0';
    st.kind[0] = '\0';
    // An `error` response is a legitimate answer — the object is not in the
    // engine's state — and not something to cache or draw.
    gboolean ok = !strstr(reply, "\"kind\":\"error\"");
    if (ok)
    {
        // Both fields are optional in effect: a directory carries no content
        // state, and `folder_kind` only matters when it is a group folder.
        json_string(reply, "state", st.state, sizeof(st.state));
        json_string(reply, "folder_kind", st.kind, sizeof(st.kind));
    }
    g_free(reply);
    if (!ok)
    {
        return FALSE;
    }

    cache_put(path, &st);
    *out = st;
    return TRUE;
}

void wusel_status_shutdown(void)
{
    disconnect();
    g_clear_pointer(&cache, g_hash_table_destroy);
    if (mounts)
    {
        for (guint i = 0; i < mounts->len; i++)
        {
            WuselMount *m = &g_array_index(mounts, WuselMount, i);
            g_free(m->root);
            g_free(m->socket);
        }
        g_array_free(mounts, TRUE);
        mounts = NULL;
    }
}
