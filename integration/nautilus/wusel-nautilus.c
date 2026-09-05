/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2026 IT Beratung Hermann GmbH
 */

// wusel Nautilus extension — per-file emblems for the virtual Nextcloud mount.
//
// A native `libnautilus-extension` module (no scripting runtime): Nautilus loads
// this `.so` and, for every file it draws, asks us to annotate it. We read the
// engine's per-file state from the FUSE xattr `user.wusel.state` (see
// wusel_core::provider::FileState) and add an emblem accordingly. The read is a
// plain getxattr(2) against the mount — cheap and local; the engine guarantees
// it never triggers a network round-trip.
//
// Every state gets a distinct, always-visible emblem (the OneDrive model:
// cloud = online-only, check = available, filled check = kept offline, arrow =
// pending upload). We ship the icons ourselves (see emblems/) because current
// Adwaita no longer provides suitable stock ones. Menu actions (make offline /
// free space) live in a second interface below.

#include <nautilus-extension.h>

#include <gio/gio.h>
#include <glib-object.h>
#include <string.h>
#include <sys/types.h>
#include <sys/xattr.h>

#define STATE_XATTR "user.wusel.state"
// The object's kind, independent of its sync state. Present only on the root
// of a Team/Group folder, so its mere presence is the answer; the value is
// read anyway, so a later kind can be told apart without changing this side.
#define KIND_XATTR "user.wusel.kind"
#define KIND_GROUP_FOLDER "group-folder"

// --- GObject type: one object implementing the provider interfaces ----------

typedef struct
{
    GObject parent_instance;
} WuselExt;

typedef struct
{
    GObjectClass parent_class;
} WuselExtClass;

static GType wusel_ext_get_type(void);

// --- NautilusInfoProvider: add an emblem based on the file's state ----------

// Map a state string to one of our emblem icons, or NULL for "no emblem".
//
// We ship our own icons (see emblems/) rather than reuse stock ones: current
// Adwaita has pruned its emblem set to a handful, so there is no reliable
// "cloud" or "check" to reuse — and every state should be *visible*, the whole
// point of a status emblem.
static const char *emblem_for_state(const char *state)
{
    if (strcmp(state, "online-only") == 0)
    {
        return "wusel-emblem-cloud"; // lives on the server, not downloaded
    }
    if (strcmp(state, "cached") == 0)
    {
        return "wusel-emblem-cached"; // a copy is on disk now, but evictable
    }
    if (strcmp(state, "pinned") == 0)
    {
        return "wusel-emblem-pinned"; // kept offline on purpose, always available
    }
    if (strcmp(state, "pinned-stale") == 0)
    {
        // Kept offline, but the copy is older than the server's. Still
        // available — that promise holds — just not the newest.
        return "wusel-emblem-pinned-stale";
    }
    if (strcmp(state, "modified") == 0)
    {
        return "wusel-emblem-modified"; // being edited locally, not yet flushed
    }
    if (strcmp(state, "uploading") == 0)
    {
        // Committed and on its way to the server (the async write-back). Reuse
        // the synchronising emblem so the user sees it is in flight.
        return "wusel-emblem-uploading";
    }
    if (strcmp(state, "sync-error") == 0)
    {
        // Committed, but the upload failed for good — the bytes are safe locally
        // and need the user's attention.
        return "wusel-emblem-sync-error";
    }
    return NULL;
}

// --- Live emblem refresh -----------------------------------------------------
//
// The daemon emits a D-Bus signal `at.itbh.Wusel.FileChanged(path)` when a
// file's cache state changes in the background (e.g. it just finished
// hydrating). We keep a weak map path → NautilusFileInfo of the files Nautilus
// has shown us and, on the signal, re-read that one so its emblem updates
// without a manual refresh. All of this runs on Nautilus's main thread (module
// init + update_file_info + the signal callback share the default main context),
// so the map needs no locking. Weak refs: we never keep a file alive ourselves.

static GHashTable *tracked;     // path (owned) -> GWeakRef* (owned)
static GDBusConnection *bus;    // session bus, for the signal subscription
static guint file_changed_sub;  // subscription id, so shutdown can unsubscribe

// Coalescing the FileChanged storm.
//
// The daemon emits FileChanged whenever a file's cache state changes — and it
// can change a great many times in a short burst (a folder full of images being
// thumbnailed, a background re-list reconciling a busy share). Acting on each
// signal *synchronously* means one `invalidate_extension_info` per signal on
// Nautilus's main thread, and a large enough burst wedges the UI hard — reported
// as a full freeze when navigating back into a directory full of media.
//
// So a signal does not act; it records the path and arms a short timer. When the
// timer fires, each distinct path is refreshed exactly once, no matter how many
// signals named it. One coalesced pass per window instead of a storm.
#define FILECHANGED_DEBOUNCE_MS 150
static GHashTable *pending;     // path (owned) -> unused; the set of paths to refresh
static guint flush_source;      // the armed timer, 0 when none
static gboolean flush_pending(gpointer user_data);

// Keeping `tracked` bounded.
//
// An entry costs a strdup'd path plus a GWeakRef, and update_file_info runs for
// every file Nautilus draws — browsing a large mount in a session that stays
// open for days would otherwise grow the table forever. Nothing removes an
// entry on its own: a weak ref going dead does not notify us, and the
// FileChanged handler only reaps the one path it was told about.
//
// So sweep the dead refs ourselves, but not on every insertion — the sweep is
// O(n) and update_file_info sits in the draw path. Once per PRUNE_EVERY
// insertions amortises it to O(1) per file. The sweep alone is not a bound
// (a window full of *live* files is all live refs), hence the hard cap on top.
#define TRACKED_PRUNE_EVERY 64
#define TRACKED_MAX 4096

static guint tracked_since_prune;

static void free_weakref(gpointer p)
{
    g_weak_ref_clear((GWeakRef *)p);
    g_free(p);
}

// GHRFunc for the sweep: TRUE drops the entry (and frees key + GWeakRef).
static gboolean weakref_is_dead(gpointer key, gpointer value, gpointer user_data)
{
    (void)key;
    (void)user_data;
    NautilusFileInfo *file = g_weak_ref_get((GWeakRef *)value);
    if (!file)
    {
        return TRUE;
    }
    g_object_unref(file);
    return FALSE;
}

static void track_file(const char *path, NautilusFileInfo *file)
{
    if (!tracked)
    {
        return;
    }
    if (++tracked_since_prune >= TRACKED_PRUNE_EVERY)
    {
        tracked_since_prune = 0;
        g_hash_table_foreach_remove(tracked, weakref_is_dead, NULL);
        if (g_hash_table_size(tracked) > TRACKED_MAX)
        {
            // Everything in here is live, so there is no "least useful" entry
            // to pick — and the table is a refresh *optimisation*, not state:
            // the worst a reset can cost is one missed emblem update, and the
            // next time Nautilus draws a file it registers it again. Bounded
            // memory is worth more than that.
            g_hash_table_remove_all(tracked);
        }
    }
    GWeakRef *wr = g_new0(GWeakRef, 1);
    g_weak_ref_init(wr, file);
    g_hash_table_replace(tracked, g_strdup(path), wr); // frees any old key+value
}

static void on_file_changed(GDBusConnection *conn, const char *sender,
                            const char *object_path, const char *interface,
                            const char *signal, GVariant *params, gpointer user_data)
{
    (void)conn;
    (void)sender;
    (void)object_path;
    (void)interface;
    (void)signal;
    (void)user_data;
    // The subscription matches any sender emitting this signal name, so the
    // parameters are untrusted: extracting "(&s)" from a differently-typed
    // variant is undefined behaviour inside the user's file manager. Check
    // the type before touching it.
    if (!g_variant_is_of_type(params, G_VARIANT_TYPE("(s)")))
    {
        return;
    }
    const char *path = NULL;
    g_variant_get(params, "(&s)", &path);
    if (!path || !tracked)
    {
        return;
    }
    // Only paths Nautilus is actually showing are worth refreshing; an untracked
    // one would just be recorded and swept later for nothing.
    if (!g_hash_table_contains(tracked, path))
    {
        return;
    }
    // Record it and arm the timer if it is not already armed. Coalescing happens
    // for free: `pending` is a set, so repeated signals for one path collapse.
    g_hash_table_add(pending, g_strdup(path));
    if (flush_source == 0)
    {
        flush_source = g_timeout_add(FILECHANGED_DEBOUNCE_MS, flush_pending, NULL);
    }
}

// Refresh each pending path once, then disarm. Runs on the main thread, like
// everything here, so it needs no locking.
static gboolean flush_pending(gpointer user_data)
{
    (void)user_data;
    GHashTableIter it;
    gpointer key;
    g_hash_table_iter_init(&it, pending);
    while (g_hash_table_iter_next(&it, &key, NULL))
    {
        const char *path = key;
        GWeakRef *wr = tracked ? g_hash_table_lookup(tracked, path) : NULL;
        if (!wr)
        {
            continue;
        }
        NautilusFileInfo *file = g_weak_ref_get(wr);
        if (file)
        {
            nautilus_file_info_invalidate_extension_info(file);
            g_object_unref(file);
        }
        else
        {
            g_hash_table_remove(tracked, path); // file gone — drop the stale entry
        }
    }
    g_hash_table_remove_all(pending);
    flush_source = 0;
    return G_SOURCE_REMOVE;
}

static void live_refresh_init(void)
{
    tracked = g_hash_table_new_full(g_str_hash, g_str_equal, g_free, free_weakref);
    pending = g_hash_table_new_full(g_str_hash, g_str_equal, g_free, NULL);
    tracked_since_prune = 0;
    GError *err = NULL;
    bus = g_bus_get_sync(G_BUS_TYPE_SESSION, NULL, &err);
    if (!bus)
    {
        g_warning("wusel: no session bus for live emblem refresh: %s",
                  err ? err->message : "?");
        g_clear_error(&err);
        return;
    }
    file_changed_sub =
        g_dbus_connection_signal_subscribe(bus, NULL, "at.itbh.Wusel", "FileChanged",
                                           NULL, NULL, G_DBUS_SIGNAL_FLAGS_NONE,
                                           on_file_changed, NULL, NULL);
}

static NautilusOperationResult
wusel_ext_update_file_info(NautilusInfoProvider *provider,
                             NautilusFileInfo *file,
                             GClosure *update_complete,
                             NautilusOperationHandle **handle)
{
    (void)provider;
    (void)update_complete;
    (void)handle;

    // Only real local files live on the FUSE mount; skip trash:, recent:, etc.
    char *scheme = nautilus_file_info_get_uri_scheme(file);
    gboolean is_file = scheme && strcmp(scheme, "file") == 0;
    g_free(scheme);
    if (!is_file)
    {
        return NAUTILUS_OPERATION_COMPLETE;
    }

    GFile *location = nautilus_file_info_get_location(file);
    char *path = location ? g_file_get_path(location) : NULL;
    if (location)
    {
        g_object_unref(location);
    }
    if (!path)
    {
        return NAUTILUS_OPERATION_COMPLETE;
    }

    char value[32];
    ssize_t n = getxattr(path, STATE_XATTR, value, sizeof(value) - 1);
    if (n > 0)
    {
        value[n] = '\0';
        track_file(path, file); // remember it, so a FileChanged can refresh it
        const char *emblem = emblem_for_state(value);
        if (emblem)
        {
            nautilus_file_info_add_emblem(file, emblem);
        }
    }
    // else: no xattr → not one of our files (or an unpinned directory).

    // The kind is a separate attribute and a separate emblem: a Team/Group
    // folder still has a sync state, and both belong on it. Absent on
    // everything else, which is why nothing is drawn by default.
    char kind[32];
    ssize_t k = getxattr(path, KIND_XATTR, kind, sizeof(kind) - 1);
    if (k > 0)
    {
        kind[k] = '\0';
        if (strcmp(kind, KIND_GROUP_FOLDER) == 0)
        {
            track_file(path, file);
            nautilus_file_info_add_emblem(file, "wusel-emblem-group-folder");
        }
    }
    g_free(path);
    return NAUTILUS_OPERATION_COMPLETE;
}

static void
wusel_ext_info_provider_iface_init(NautilusInfoProviderInterface *iface)
{
    iface->update_file_info = wusel_ext_update_file_info;
}

// --- NautilusMenuProvider: pin / unpin from the context menu ----------------
//
// Every label carries the "Wusel - " prefix. A context menu is a crowded place
// and these three entries sit among a dozen the file manager and other
// extensions contribute; without the prefix nobody can tell whose commands they
// are, which was the first thing a real user said about them.
//
// "Stop Keeping Offline" rather than "Free Up Space": the latter borrows a
// phrase from another product and describes a side effect, while what the entry
// actually does is withdraw the promise. The space follows.

// Read one file's state xattr into `out` (NUL-terminated). FALSE if the file is
// not a local file, or not on our mount (no xattr).
static gboolean file_state(NautilusFileInfo *file, char *out, size_t out_len)
{
    char *scheme = nautilus_file_info_get_uri_scheme(file);
    gboolean is_file = scheme && strcmp(scheme, "file") == 0;
    g_free(scheme);
    if (!is_file)
    {
        return FALSE;
    }
    GFile *location = nautilus_file_info_get_location(file);
    char *path = location ? g_file_get_path(location) : NULL;
    if (location)
    {
        g_object_unref(location);
    }
    if (!path)
    {
        return FALSE;
    }
    ssize_t n = getxattr(path, STATE_XATTR, out, out_len - 1);
    g_free(path);
    if (n <= 0)
    {
        return FALSE;
    }
    out[n] = '\0';
    return TRUE;
}

// Locate the wusel binary: PATH first, then the usual install locations. The
// menu spawns it, and Nautilus's PATH is not always the user's shell PATH.
static char *find_wusel(void)
{
    char *p = g_find_program_in_path("wusel");
    if (p)
    {
        return p;
    }
    const char *fixed[] = {"/usr/local/bin/wusel", "/usr/bin/wusel", NULL};
    for (int i = 0; fixed[i]; i++)
    {
        if (g_file_test(fixed[i], G_FILE_TEST_IS_EXECUTABLE))
        {
            return g_strdup(fixed[i]);
        }
    }
    char *home = g_build_filename(g_get_home_dir(), ".local", "bin", "wusel", NULL);
    if (g_file_test(home, G_FILE_TEST_IS_EXECUTABLE))
    {
        return home;
    }
    g_free(home);
    return NULL;
}

// Pending `g_child_watch_add_full` source ids (as GUINT_TO_POINTER).
//
// Same hazard as the D-Bus subscription: a watch left registered when the
// module is unloaded would dispatch into unmapped code. A pin/unpin can easily
// still be running at that moment, so shutdown has to be able to find them —
// hence the list rather than a fire-and-forget `g_child_watch_add`.
static GSList *child_watches;

// The command finished — re-read the file's info so its emblem reflects the new
// state (pinned ⇄ not) without a manual refresh. Runs on the GLib main loop.
static void on_action_done(GPid pid, gint status, gpointer data)
{
    (void)status;
    // A child-watch source destroys itself after this one dispatch, so its id
    // is about to become free for reuse — it must leave the shutdown list now,
    // or shutdown would later remove a stranger's source under that number.
    GSource *self = g_main_current_source();
    if (self)
    {
        child_watches =
            g_slist_remove(child_watches, GUINT_TO_POINTER(g_source_get_id(self)));
    }
    NautilusFileInfo *file = NAUTILUS_FILE_INFO(data);
    nautilus_file_info_invalidate_extension_info(file);
    g_spawn_close_pid(pid);
    // No unref here: the source's GDestroyNotify owns the reference, so it is
    // released on both paths — this dispatch and a forced removal at shutdown.
}

// Run `wusel <verb> <path>` for each selected file. wusel resolves the
// on-disk path to its account + remote path itself. On completion we refresh
// that file's emblem (see on_action_done).
static void run_action(NautilusMenuItem *item, const char *verb)
{
    char *exe = find_wusel();
    if (!exe)
    {
        g_warning("wusel not found (PATH, /usr/local/bin, /usr/bin, ~/.local/bin)");
        return;
    }
    GList *files = g_object_get_data(G_OBJECT(item), "wusel-files");
    for (GList *l = files; l != NULL; l = l->next)
    {
        NautilusFileInfo *file = NAUTILUS_FILE_INFO(l->data);
        GFile *location = nautilus_file_info_get_location(file);
        char *path = location ? g_file_get_path(location) : NULL;
        if (location)
        {
            g_object_unref(location);
        }
        if (!path)
        {
            continue;
        }
        char *argv[] = {exe, (char *)verb, path, NULL};
        GPid pid;
        GError *err = NULL;
        if (g_spawn_async(NULL, argv, NULL, G_SPAWN_DO_NOT_REAP_CHILD, NULL, NULL, &pid, &err))
        {
            // `_full` for the GDestroyNotify: it releases our reference whether
            // the watch fires normally or shutdown tears it down unfired.
            guint watch = g_child_watch_add_full(G_PRIORITY_DEFAULT, pid, on_action_done,
                                                 g_object_ref(file), g_object_unref);
            child_watches = g_slist_prepend(child_watches, GUINT_TO_POINTER(watch));
        }
        else
        {
            g_warning("wusel %s failed to start: %s", verb, err ? err->message : "?");
            g_clear_error(&err);
        }
        g_free(path);
    }
    g_free(exe);
}

static void on_pin_activate(NautilusMenuItem *item, gpointer user_data)
{
    (void)user_data;
    run_action(item, "pin");
}

static void on_unpin_activate(NautilusMenuItem *item, gpointer user_data)
{
    (void)user_data;
    run_action(item, "unpin");
}

static void on_update_activate(NautilusMenuItem *item, gpointer user_data)
{
    (void)user_data;
    // Deliberately its own verb, not unpin+pin: that would drop the eviction
    // marker first, so a failed re-download would leave the file outdated *and*
    // unprotected.
    run_action(item, "update");
}

// Two-letter UI language from the environment (extend the table as needed).
static const char *nc_lang(void)
{
    const char *l = g_getenv("LC_ALL");
    if (!l || !*l)
    {
        l = g_getenv("LC_MESSAGES");
    }
    if (!l || !*l)
    {
        l = g_getenv("LANG");
    }
    return (l && g_str_has_prefix(l, "de")) ? "de" : "en";
}

// Pick the localized string for the current language (English fallback).
static const char *tr(const char *en, const char *de)
{
    return strcmp(nc_lang(), "de") == 0 ? de : en;
}

static NautilusMenuItem *make_item(const char *name, const char *label,
                                   const char *tip, const char *icon,
                                   GCallback cb, GList *files)
{
    NautilusMenuItem *item = nautilus_menu_item_new(name, label, tip, icon);
    // Carry a copy of the selection so the activate handler still has it.
    g_object_set_data_full(G_OBJECT(item), "wusel-files",
                           nautilus_file_info_list_copy(files),
                           (GDestroyNotify)nautilus_file_info_list_free);
    g_signal_connect(item, "activate", cb, NULL);
    return item;
}

static GList *
wusel_ext_get_file_items(NautilusMenuProvider *provider, GList *files)
{
    (void)provider;

    // Offer "make offline" if anything selected can be pinned, and "free space"
    // if anything selected is pinned; ignore a selection that is not ours.
    gboolean any_ours = FALSE, any_pinnable = FALSE, any_unpinnable = FALSE;
    gboolean any_stale = FALSE;
    for (GList *l = files; l != NULL; l = l->next)
    {
        char state[32];
        if (!file_state(NAUTILUS_FILE_INFO(l->data), state, sizeof(state)))
        {
            continue;
        }
        any_ours = TRUE;
        if (strcmp(state, "pinned") == 0)
        {
            any_unpinnable = TRUE;
        }
        else if (strcmp(state, "pinned-stale") == 0)
        {
            // Still pinned, so freeing space is still on offer — and now there
            // is something to bring up to date.
            any_unpinnable = TRUE;
            any_stale = TRUE;
        }
        else if (strcmp(state, "online-only") == 0 || strcmp(state, "cached") == 0)
        {
            any_pinnable = TRUE;
        }
    }
    if (!any_ours)
    {
        return NULL;
    }

    GList *items = NULL;
    if (any_stale)
    {
        // First in the list: it is the only entry that answers a problem the
        // emblem is already showing.
        items = g_list_append(
            items, make_item("Wusel::update",
                             tr("Wusel - Update Now", "Wusel - Jetzt aktualisieren"),
                             tr("Fetch the current version; the offline copy is out of date",
                                "Aktuelle Fassung holen; die Offline-Kopie ist veraltet"),
                             "wusel-emblem-pinned-stale", G_CALLBACK(on_update_activate),
                             files));
    }
    if (any_pinnable)
    {
        items = g_list_append(
            items, make_item("Wusel::pin",
                             tr("Wusel - Make Available Offline",
                                "Wusel - Offline verfügbar machen"),
                             tr("Download and keep this available offline",
                                "Herunterladen und offline verfügbar halten"),
                             "wusel-emblem-pinned", G_CALLBACK(on_pin_activate), files));
    }
    if (any_unpinnable)
    {
        items = g_list_append(
            items, make_item("Wusel::unpin",
                             tr("Wusel - Stop Keeping Offline",
                                "Wusel - Offline-Verfügbarkeit aufheben"),
                             tr("Remove the local copy; keep it online-only",
                                "Lokale Kopie entfernen; nur online behalten"),
                             "wusel-emblem-cloud", G_CALLBACK(on_unpin_activate), files));
    }
    return items;
}

static void
wusel_ext_menu_provider_iface_init(NautilusMenuProviderInterface *iface)
{
    iface->get_file_items = wusel_ext_get_file_items;
}

// --- Boilerplate: dynamic type registration for a Nautilus module -----------

static void wusel_ext_class_init(WuselExtClass *klass) { (void)klass; }
static void wusel_ext_class_finalize(WuselExtClass *klass) { (void)klass; }
static void wusel_ext_init(WuselExt *self) { (void)self; }

G_DEFINE_DYNAMIC_TYPE_EXTENDED(
    WuselExt, wusel_ext, G_TYPE_OBJECT, 0,
    G_IMPLEMENT_INTERFACE_DYNAMIC(NAUTILUS_TYPE_INFO_PROVIDER,
                                  wusel_ext_info_provider_iface_init)
    G_IMPLEMENT_INTERFACE_DYNAMIC(NAUTILUS_TYPE_MENU_PROVIDER,
                                  wusel_ext_menu_provider_iface_init))

// --- Module entry points Nautilus calls -------------------------------------

void nautilus_module_initialize(GTypeModule *module)
{
    wusel_ext_register_type(module);
    live_refresh_init(); // subscribe to the daemon's FileChanged signal
}

void nautilus_module_shutdown(void)
{
    // Undo live_refresh_init symmetrically: an unloaded module must leave no
    // signal callback registered on the bus (D-Bus would otherwise dispatch
    // into unmapped code), and no leaked map or connection.
    if (bus && file_changed_sub != 0)
    {
        g_dbus_connection_signal_unsubscribe(bus, file_changed_sub);
        file_changed_sub = 0;
    }
    // Same reasoning for a pin/unpin still running: its child watch points at
    // on_action_done, which is about to be unmapped. Removing the source drops
    // the callback and, via its GDestroyNotify, our reference to the file. The
    // child itself is left unreaped — it is exiting anyway, and there is no
    // main loop left to notice.
    for (GSList *l = child_watches; l != NULL; l = l->next)
    {
        g_source_remove(GPOINTER_TO_UINT(l->data));
    }
    g_clear_pointer(&child_watches, g_slist_free);
    // The debounce timer holds a callback into this soon-to-be-unmapped module,
    // so it has to go the same way as the signal subscription above.
    if (flush_source != 0)
    {
        g_source_remove(flush_source);
        flush_source = 0;
    }
    g_clear_pointer(&pending, g_hash_table_destroy);
    g_clear_pointer(&tracked, g_hash_table_destroy);
    tracked_since_prune = 0;
    g_clear_object(&bus);
}

void nautilus_module_list_types(const GType **types, int *num_types)
{
    static GType type_list[1];
    type_list[0] = wusel_ext_get_type();
    *types = type_list;
    *num_types = 1;
}
