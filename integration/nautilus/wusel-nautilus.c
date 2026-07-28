// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 IT Beratung Hermann GmbH
//
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
    if (strcmp(state, "modified") == 0)
    {
        return "wusel-emblem-modified"; // local edit pending upload
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

static void free_weakref(gpointer p)
{
    g_weak_ref_clear((GWeakRef *)p);
    g_free(p);
}

static void track_file(const char *path, NautilusFileInfo *file)
{
    if (!tracked)
    {
        return;
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
    GWeakRef *wr = g_hash_table_lookup(tracked, path);
    if (!wr)
    {
        return;
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

static void live_refresh_init(void)
{
    tracked = g_hash_table_new_full(g_str_hash, g_str_equal, g_free, free_weakref);
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
    g_free(path);
    return NAUTILUS_OPERATION_COMPLETE;
}

static void
wusel_ext_info_provider_iface_init(NautilusInfoProviderInterface *iface)
{
    iface->update_file_info = wusel_ext_update_file_info;
}

// --- NautilusMenuProvider: pin / unpin from the context menu ----------------

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

// The command finished — re-read the file's info so its emblem reflects the new
// state (pinned ⇄ not) without a manual refresh. Runs on the GLib main loop.
static void on_action_done(GPid pid, gint status, gpointer data)
{
    (void)status;
    NautilusFileInfo *file = NAUTILUS_FILE_INFO(data);
    nautilus_file_info_invalidate_extension_info(file);
    g_object_unref(file);
    g_spawn_close_pid(pid);
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
            g_child_watch_add(pid, on_action_done, g_object_ref(file));
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
    if (any_pinnable)
    {
        items = g_list_append(
            items, make_item("Wusel::pin",
                             tr("Make Available Offline", "Offline verfügbar machen"),
                             tr("Download and keep this available offline",
                                "Herunterladen und offline verfügbar halten"),
                             "wusel-emblem-pinned", G_CALLBACK(on_pin_activate), files));
    }
    if (any_unpinnable)
    {
        items = g_list_append(
            items, make_item("Wusel::unpin",
                             tr("Free Up Space", "Speicherplatz freigeben"),
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
    g_clear_pointer(&tracked, g_hash_table_destroy);
    g_clear_object(&bus);
}

void nautilus_module_list_types(const GType **types, int *num_types)
{
    static GType type_list[1];
    type_list[0] = wusel_ext_get_type();
    *types = type_list;
    *num_types = 1;
}
