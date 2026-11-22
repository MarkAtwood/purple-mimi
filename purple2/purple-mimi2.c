/* SPDX-License-Identifier: GPL-3.0-or-later
 *
 * The libpurple 2.14 glue of purple-mimi: MIMI rooms through mimid
 * (docs/design/purple-mimi.md §8 in mimid).
 *
 * Step 4 of §13: the plugin loads, an account connects and disconnects,
 * and every failure reaches the user through libpurple. Rooms, messages
 * and the user's actions are steps 5 and 6.
 *
 * The library is process-wide: one runtime, one event queue, one
 * descriptor to watch (§5.4, §5.5). So mimi_init runs when the plugin
 * loads and mimi_shutdown when it unloads, and one input watcher drains
 * the queue for every account. Events name their account by its opaque
 * handle, which `accounts` maps back to the PurpleConnection.
 */

#include <glib.h>
#include <string.h>

/* libpurple's umbrella header; pkg-config --cflags purple finds it. */
#include <purple.h>

#include "purple-mimi.h"

#ifndef PURPLE_MIMI_VERSION
#error "PURPLE_MIMI_VERSION must be defined by the build (see CMakeLists.txt)"
#endif

/** The protocol id libpurple knows us by. INVENTED. */
#define MIMI_PRPL_ID "prpl-mimi"

/** Account option keys. INVENTED names, stable once released. */
#define OPT_PROVIDER_URL "provider_url"
#define OPT_INVITES "invites"

/** `OPT_INVITES` values, as stored in the account. */
#define INVITES_FROM_ALLOWED "from-allowed"
#define INVITES_ANY "any"
#define INVITES_NEVER "never"

/** mimi_account* -> PurpleConnection*, for routing events (§5.5). */
static GHashTable *accounts;

/** The watcher on mimi_event_fd(), or 0. */
static guint event_watcher;

/** Whether mimi_init succeeded, so unload knows to shut down. */
static gboolean library_started;

/*
 * §8.2: how a disconnection reaches libpurple. A reason marked not fatal
 * in 2.14 lets libpurple reconnect on its own schedule; the library also
 * says whether it thinks the failure is temporary, and the two must
 * agree or libpurple would retry something hopeless for ever.
 */
static PurpleConnectionError connection_error_for(mimi_status status)
{
    switch (status) {
    case MIMI_ERR_NETWORK:
        return PURPLE_CONNECTION_ERROR_NETWORK_ERROR;
    case MIMI_ERR_UNAUTHORIZED:
        return PURPLE_CONNECTION_ERROR_AUTHENTICATION_FAILED;
    case MIMI_ERR_STREAM_OFF:
        return PURPLE_CONNECTION_ERROR_INVALID_SETTINGS;
    case MIMI_ERR_WRONG_USER:
        return PURPLE_CONNECTION_ERROR_INVALID_USERNAME;
    case MIMI_ERR_TAKEN_OVER:
        return PURPLE_CONNECTION_ERROR_NAME_IN_USE;
    case MIMI_ERR_LOCKED:
    case MIMI_ERR_LOCK_LOST:
    case MIMI_ERR_STATE:
    case MIMI_ERR_PANIC:
    default:
        return PURPLE_CONNECTION_ERROR_OTHER_ERROR;
    }
}

static mimi_invite_policy invite_policy_of(const char *value)
{
    if (purple_strequal(value, INVITES_ANY))
        return MIMI_INVITES_ANY;
    if (purple_strequal(value, INVITES_NEVER))
        return MIMI_INVITES_NEVER;
    return MIMI_INVITES_FROM_ALLOWED;
}

static void log_event(const mimi_event *event)
{
    const char *text = event->text ? event->text : "";

    switch (event->level) {
    case MIMI_LOG_ERROR:
        purple_debug_error("mimi", "%s\n", text);
        break;
    case MIMI_LOG_WARN:
        purple_debug_warning("mimi", "%s\n", text);
        break;
    case MIMI_LOG_INFO:
        purple_debug_info("mimi", "%s\n", text);
        break;
    default:
        purple_debug_misc("mimi", "%s\n", text);
        break;
    }
}

/* One event, routed to its account. */
static void handle_event(const mimi_event *event)
{
    PurpleConnection *gc = NULL;

    if (event->kind == MIMI_EV_LOG) {
        log_event(event);
        return;
    }
    if (event->account != NULL)
        gc = g_hash_table_lookup(accounts, event->account);
    if (gc == NULL) {
        /* An account closed between the event and this drain. */
        return;
    }

    switch (event->kind) {
    case MIMI_EV_PROGRESS:
        purple_connection_update_progress(gc, event->text ? event->text : "",
                                          event->step, event->steps);
        break;
    case MIMI_EV_CONNECTED:
        purple_connection_set_state(gc, PURPLE_CONNECTED);
        /*
         * §12.3: the token now lives in the state directory, so the
         * copy libpurple remembers is not needed and is cleared.
         */
        purple_account_set_remember_password(purple_connection_get_account(gc),
                                             FALSE);
        purple_account_set_password(purple_connection_get_account(gc), NULL);
        break;
    case MIMI_EV_DISCONNECTED:
        /*
         * The handle is dead once the library reports this, so it must
         * leave the map before purple_connection_error_reason, which can
         * call back into close().
         */
        g_hash_table_remove(accounts, event->account);
        purple_connection_set_protocol_data(gc, NULL);
        purple_connection_error_reason(
            gc, connection_error_for(event->status),
            event->text ? event->text : "purple-mimi failed");
        break;
    default:
        purple_debug_misc("mimi", "unknown event kind %d\n", event->kind);
        break;
    }
}

/* The descriptor is readable: drain every event that waits. */
static void drain_events(gpointer data, gint fd, PurpleInputCondition cond)
{
    mimi_event *event;

    (void) data;
    (void) fd;
    (void) cond;

    while ((event = mimi_next_event()) != NULL) {
        handle_event(event);
        mimi_event_free(event);
    }
}

static void mimi_login(PurpleAccount *account)
{
    PurpleConnection *gc = purple_account_get_connection(account);
    mimi_account_config config;
    mimi_account *handle = NULL;
    char *error = NULL;
    mimi_status status;
    const char *password;

    if (!library_started) {
        purple_connection_error_reason(
            gc, PURPLE_CONNECTION_ERROR_OTHER_ERROR,
            "purple-mimi's library did not start; see the debug window");
        return;
    }

    memset(&config, 0, sizeof config);
    config.base_dir = purple_user_dir();
    config.user = purple_account_get_username(account);
    config.provider_url = purple_account_get_string(account, OPT_PROVIDER_URL,
                                                    NULL);
    password = purple_connection_get_password(gc);
    /* An empty field means "use the token in the state directory". */
    config.token = (password != NULL && *password != '\0') ? password : NULL;
    config.invite_policy = invite_policy_of(
        purple_account_get_string(account, OPT_INVITES, INVITES_FROM_ALLOWED));
    config.key_packages = 0;    /* the library's default */
    config.sync_interval_ms = 0;
    /*
     * §6.6: Pidgin 2 has no account action to take an account back, so
     * asking to connect is taken as asking for it.
     */
    config.take_over = TRUE;

    purple_connection_set_state(gc, PURPLE_CONNECTING);
    purple_connection_update_progress(gc, "Opening the account", 0, 3);

    status = mimi_account_open(&config, &handle, &error);
    if (status != MIMI_OK) {
        purple_connection_error_reason(
            gc, connection_error_for(status),
            error ? error : "the account could not be opened");
        mimi_string_free(error);
        return;
    }
    mimi_string_free(error);

    g_hash_table_insert(accounts, handle, gc);
    purple_connection_set_protocol_data(gc, handle);
}

static void mimi_close(PurpleConnection *gc)
{
    mimi_account *handle = purple_connection_get_protocol_data(gc);

    if (handle == NULL)
        return;
    /* Out of the map first: the handle is invalid once close returns. */
    g_hash_table_remove(accounts, handle);
    purple_connection_set_protocol_data(gc, NULL);
    mimi_account_close(handle);
}

/* §8.1: available and offline, both settable; MIMI has no presence. */
static GList *mimi_status_types(PurpleAccount *account)
{
    GList *types = NULL;

    (void) account;
    types = g_list_append(types,
                          purple_status_type_new(PURPLE_STATUS_AVAILABLE,
                                                 "available", "Available",
                                                 TRUE));
    types = g_list_append(types,
                          purple_status_type_new(PURPLE_STATUS_OFFLINE,
                                                 "offline", "Offline", TRUE));
    return types;
}

static const char *mimi_list_icon(PurpleAccount *account, PurpleBuddy *buddy)
{
    (void) account;
    (void) buddy;
    return "mimi";
}

static PurplePluginProtocolInfo prpl_info = {
    /*
     * The password is optional: an account whose token is already in the
     * state directory connects with an empty field (§8.1, §12.3).
     */
    .options = OPT_PROTO_PASSWORD_OPTIONAL,
    .user_splits = NULL,
    .protocol_options = NULL,   /* filled in by init_plugin */
    .icon_spec = NO_BUDDY_ICONS,
    .list_icon = mimi_list_icon,
    .status_types = mimi_status_types,
    .login = mimi_login,
    .close = mimi_close,
    .struct_size = sizeof(PurplePluginProtocolInfo),
};

static gboolean plugin_load(PurplePlugin *plugin)
{
    mimi_init_options options;
    char *error = NULL;
    mimi_status status;
    int fd;

    (void) plugin;

    /*
     * §10.1: the header and the static library are built together, so a
     * mismatch means a stale build. Refuse rather than run a different
     * ABI than the one this file was compiled against.
     */
    if (mimi_abi_version() != MIMI_ABI_VERSION) {
        purple_debug_error("mimi",
                           "ABI %u in the library, %u in the plugin; "
                           "rebuild purple-mimi\n",
                           mimi_abi_version(), (unsigned) MIMI_ABI_VERSION);
        return FALSE;
    }

    memset(&options, 0, sizeof options);
    options.abi_version = MIMI_ABI_VERSION;
    options.log_level = MIMI_LOG_INFO;

    status = mimi_init(&options, &error);
    if (status != MIMI_OK) {
        purple_debug_error("mimi", "the library did not start: %s\n",
                           error ? error : "no reason given");
        mimi_string_free(error);
        return FALSE;
    }
    mimi_string_free(error);
    library_started = TRUE;

    accounts = g_hash_table_new(g_direct_hash, g_direct_equal);

    fd = mimi_event_fd();
    if (fd < 0) {
        purple_debug_error("mimi", "the library has no event descriptor\n");
        return FALSE;
    }
    event_watcher = purple_input_add(fd, PURPLE_INPUT_READ, drain_events, NULL);

    purple_debug_info("mimi", "purple-mimi loaded: %s\n", mimi_version());
    return TRUE;
}

static gboolean plugin_unload(PurplePlugin *plugin)
{
    (void) plugin;

    if (event_watcher != 0) {
        purple_input_remove(event_watcher);
        event_watcher = 0;
    }
    if (library_started) {
        /* Closes every account and waits for their release (§5.4). */
        mimi_shutdown();
        library_started = FALSE;
    }
    if (accounts != NULL) {
        g_hash_table_destroy(accounts);
        accounts = NULL;
    }
    return TRUE;
}

static PurplePluginInfo info = {
    .magic = PURPLE_PLUGIN_MAGIC,
    .major_version = PURPLE_MAJOR_VERSION,
    .minor_version = PURPLE_MINOR_VERSION,
    .type = PURPLE_PLUGIN_PROTOCOL,
    .priority = PURPLE_PRIORITY_DEFAULT,
    .id = MIMI_PRPL_ID,
    .name = "MIMI",
    .version = PURPLE_MIMI_VERSION,
    .summary = "MIMI (IETF More Instant Messaging Interoperability)",
    .description = "Chat in MIMI rooms over MLS, through mimid's agent "
                   "embedded in this process.",
    .author = "Mark Atwood <mark@reviewcommit.com>",
    .homepage = "https://github.com/MarkAtwood/purple-mimi",
    .load = plugin_load,
    .unload = plugin_unload,
    .extra_info = &prpl_info,
};

static void init_plugin(PurplePlugin *plugin)
{
    PurpleAccountOption *option;
    GList *invites = NULL;
    PurpleKeyValuePair *pair;

    (void) plugin;

    /* §8.1: the provider URL, default https://<domain> of the handle. */
    option = purple_account_option_string_new("Provider URL",
                                              OPT_PROVIDER_URL, "");
    prpl_info.protocol_options =
        g_list_append(prpl_info.protocol_options, option);

    /* §8.1: which invites the account joins by itself. */
    pair = g_new0(PurpleKeyValuePair, 1);
    pair->key = g_strdup("From users I allowed");
    pair->value = g_strdup(INVITES_FROM_ALLOWED);
    invites = g_list_append(invites, pair);
    pair = g_new0(PurpleKeyValuePair, 1);
    pair->key = g_strdup("Always");
    pair->value = g_strdup(INVITES_ANY);
    invites = g_list_append(invites, pair);
    pair = g_new0(PurpleKeyValuePair, 1);
    pair->key = g_strdup("Never");
    pair->value = g_strdup(INVITES_NEVER);
    invites = g_list_append(invites, pair);

    option = purple_account_option_list_new("Join invites", OPT_INVITES,
                                            invites);
    /*
     * 2.14 has no list-specific setter; a list option's default is its
     * stored string value (accountopt.h).
     */
    purple_account_option_set_default_string(option, INVITES_FROM_ALLOWED);
    prpl_info.protocol_options =
        g_list_append(prpl_info.protocol_options, option);
}

PURPLE_INIT_PLUGIN(mimi, init_plugin, info)
