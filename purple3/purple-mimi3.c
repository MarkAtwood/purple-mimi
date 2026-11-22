/* SPDX-License-Identifier: GPL-3.0-or-later
 *
 * The libpurple 3 glue of purple-mimi: the GPlugin plugin, the
 * process-wide library and the event pump
 * (docs/design/purple-mimi.md §9).
 *
 * Step 4 of §13: the plugin loads, an account connects and disconnects,
 * and every failure reaches the user. Rooms, messages and the user's
 * actions are steps 5 and 6.
 *
 * The library is process-wide (§5.4, §5.5): mimi_init when the plugin
 * loads, mimi_shutdown when it unloads, and one GSource on
 * mimi_event_fd() draining the queue for every account. Events name
 * their account by its opaque handle, which `connections` maps back to
 * the PurpleMimiConnection.
 */

#include <glib.h>
#include <glib-unix.h>
#include <glib/gi18n-lib.h>
#include <string.h>

#include <gplugin.h>
#include <gplugin-native.h>

#include <purple.h>

#include "mimiplugin.h"

#include "mimiconnection.h"
#include "mimiprotocol.h"

#define PURPLE_MIMI_DOMAIN (g_quark_from_static_string("purple-mimi"))

#ifndef PURPLE_MIMI_VERSION
#error "PURPLE_MIMI_VERSION must be defined by the build (see CMakeLists.txt)"
#endif

static PurpleProtocol *mimi_protocol;

/** mimi_account* -> PurpleMimiConnection*, for routing events. */
static GHashTable *connections;

/** The watch on mimi_event_fd(), or NULL. */
static GSource *event_source;

/** Whether mimi_init succeeded, so unload knows to shut down. */
static gboolean library_started;

mimi_invite_policy purple_mimi_invite_policy_of(const char *value)
{
    if (g_strcmp0(value, PURPLE_MIMI_INVITES_ANY) == 0)
        return MIMI_INVITES_ANY;
    if (g_strcmp0(value, PURPLE_MIMI_INVITES_NEVER) == 0)
        return MIMI_INVITES_NEVER;
    return MIMI_INVITES_FROM_ALLOWED;
}

const char *purple_mimi_account_user(PurpleAccount *account)
{
    PurpleAccountSettings *settings = NULL;
    const char *user = NULL;

    g_return_val_if_fail(PURPLE_IS_ACCOUNT(account), NULL);

    settings = purple_account_get_settings(account);
    if (settings != NULL) {
        user = purple_account_settings_get_string(
            settings, PURPLE_MIMI_SETTING_USER, NULL);
    }
    if (user == NULL || *user == '\0')
        user = purple_account_get_name(account);
    if (user == NULL || *user == '\0')
        return NULL;
    return user;
}

void purple_mimi_plugin_track(mimi_account *handle, gpointer connection)
{
    if (connections != NULL && handle != NULL)
        g_hash_table_insert(connections, handle, connection);
}

void purple_mimi_plugin_forget(mimi_account *handle)
{
    if (connections != NULL && handle != NULL)
        g_hash_table_remove(connections, handle);
}

static void log_event(const mimi_event *event)
{
    const char *text = event->text ? event->text : "";

    switch (event->level) {
    case MIMI_LOG_ERROR:
        g_critical("%s", text);
        break;
    case MIMI_LOG_WARN:
        g_warning("%s", text);
        break;
    case MIMI_LOG_INFO:
        g_message("%s", text);
        break;
    default:
        g_debug("%s", text);
        break;
    }
}

static void handle_event(const mimi_event *event)
{
    PurpleMimiConnection *connection = NULL;

    if (event->kind == MIMI_EV_LOG) {
        log_event(event);
        return;
    }
    if (event->account != NULL)
        connection = g_hash_table_lookup(connections, event->account);
    if (connection == NULL)
        return;    /* closed between the event and this drain */

    switch (event->kind) {
    case MIMI_EV_PROGRESS:
        /*
         * libpurple 3 has no per-step connection progress of 2.14's
         * kind, so the steps go to the debug log.
         */
        g_debug("connecting: %u/%u %s", event->step, event->steps,
                event->text ? event->text : "");
        break;
    case MIMI_EV_CONNECTED:
        purple_mimi_connection_report(connection, MIMI_OK, FALSE, NULL);
        break;
    case MIMI_EV_DISCONNECTED:
        /* The handle dies with this event (§5.5). */
        purple_mimi_plugin_forget(event->account);
        purple_mimi_connection_forget(connection);
        purple_mimi_connection_report(connection, event->status,
                                      event->temporary, event->text);
        break;
    default:
        g_debug("unknown event kind %d", event->kind);
        break;
    }
}

static gboolean drain_events(G_GNUC_UNUSED gint fd,
                             G_GNUC_UNUSED GIOCondition condition,
                             G_GNUC_UNUSED gpointer data)
{
    mimi_event *event;

    while ((event = mimi_next_event()) != NULL) {
        handle_event(event);
        mimi_event_free(event);
    }
    return G_SOURCE_CONTINUE;
}

static GPluginPluginInfo *purple_mimi_plugin_query(
    G_GNUC_UNUSED GError **error)
{
    const char *authors[] = { "Mark Atwood <mark@reviewcommit.com>", NULL };

    return purple_plugin_info_new(
        "id", PURPLE_MIMI_PROTOCOL_ID, "name", "MIMI Protocol Plugin",
        "authors", authors, "version", PURPLE_MIMI_VERSION, "category",
        N_("Protocol"), "summary",
        N_("MIMI (IETF More Instant Messaging Interoperability)"),
        "description",
        N_("Chat in MIMI rooms over MLS, through mimid's agent embedded in "
           "this process."),
        "website", "https://github.com/MarkAtwood/purple-mimi", "abi-version",
        PURPLE_ABI_VERSION, NULL);
}

static gboolean purple_mimi_plugin_load(GPluginPlugin *plugin, GError **error)
{
    PurpleProtocolManager *manager = NULL;
    mimi_init_options options;
    char *library_error = NULL;
    mimi_status status;
    int fd;

    if (PURPLE_IS_PROTOCOL(mimi_protocol)) {
        g_set_error_literal(error, PURPLE_MIMI_DOMAIN, 0,
                            "plugin was not cleaned up properly");
        return FALSE;
    }

    /*
     * §10.1: the header and the static library are built together, so a
     * mismatch means a stale build. Refuse rather than run an ABI other
     * than the one this file was compiled against.
     */
    if (mimi_abi_version() != MIMI_ABI_VERSION) {
        g_set_error(error, PURPLE_MIMI_DOMAIN, 0,
                    "ABI %u in the library, %u in the plugin; rebuild "
                    "purple-mimi",
                    mimi_abi_version(), (unsigned) MIMI_ABI_VERSION);
        return FALSE;
    }

    memset(&options, 0, sizeof options);
    options.abi_version = MIMI_ABI_VERSION;
    options.log_level = MIMI_LOG_INFO;

    status = mimi_init(&options, &library_error);
    if (status != MIMI_OK) {
        g_set_error(error, PURPLE_MIMI_DOMAIN, 0,
                    "purple-mimi's library did not start: %s",
                    library_error ? library_error : "no reason given");
        mimi_string_free(library_error);
        return FALSE;
    }
    mimi_string_free(library_error);
    library_started = TRUE;

    connections = g_hash_table_new(g_direct_hash, g_direct_equal);

    fd = mimi_event_fd();
    if (fd < 0) {
        g_set_error_literal(error, PURPLE_MIMI_DOMAIN, 0,
                            "the library has no event descriptor");
        return FALSE;
    }
    event_source = g_unix_fd_source_new(fd, G_IO_IN);
    g_source_set_callback(event_source, G_SOURCE_FUNC(drain_events), NULL,
                          NULL);
    g_source_attach(event_source, NULL);

    purple_mimi_connection_register(GPLUGIN_NATIVE_PLUGIN(plugin));
    purple_mimi_protocol_register(GPLUGIN_NATIVE_PLUGIN(plugin));

    manager = purple_core_get_protocol_manager(purple_core_get_default());
    mimi_protocol = purple_mimi_protocol_new();
    if (!purple_protocol_manager_add(manager, mimi_protocol, error)) {
        g_clear_object(&mimi_protocol);
        return FALSE;
    }

    g_message("purple-mimi loaded: %s", mimi_version());
    return TRUE;
}

static gboolean purple_mimi_plugin_unload(
    G_GNUC_UNUSED GPluginPlugin *plugin, G_GNUC_UNUSED gboolean shutdown,
    GError **error)
{
    PurpleProtocolManager *manager = NULL;

    if (!PURPLE_IS_PROTOCOL(mimi_protocol)) {
        g_set_error_literal(error, PURPLE_MIMI_DOMAIN, 0,
                            "plugin was not setup properly");
        return FALSE;
    }

    manager = purple_core_get_protocol_manager(purple_core_get_default());
    if (!purple_protocol_manager_remove(manager, mimi_protocol, error))
        return FALSE;
    g_clear_object(&mimi_protocol);

    if (event_source != NULL) {
        g_source_destroy(event_source);
        g_source_unref(event_source);
        event_source = NULL;
    }
    if (library_started) {
        /* Closes every account and waits for their release (§5.4). */
        mimi_shutdown();
        library_started = FALSE;
    }
    if (connections != NULL) {
        g_hash_table_destroy(connections);
        connections = NULL;
    }
    return TRUE;
}

GPLUGIN_NATIVE_PLUGIN_DECLARE(purple_mimi_plugin)
