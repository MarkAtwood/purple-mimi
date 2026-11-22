/* SPDX-License-Identifier: GPL-3.0-or-later
 *
 * A headless libpurple 2 host that loads purple-mimi and checks it
 * registers its protocol (docs/design/purple-mimi.md §11).
 *
 * This is the smallest thing that proves the plugin is real: libpurple
 * itself probes it, dlopen's it, runs its load function (which starts the
 * Rust runtime through mimi_init) and registers the protocol. A plugin
 * that compiles and links can still fail every one of those.
 *
 * It talks to no network and opens no account: connecting needs a
 * provider, which the end-to-end tests of §11 supply.
 *
 * Modelled on libpurple's own nullclient example for the required UI and
 * event-loop operations.
 */

#include <glib.h>
#include <stdio.h>
#include <stdlib.h>

#include <purple.h>

#define UI_ID "purple-mimi-smoke"

/* The protocol the plugin must register. */
#define EXPECT_PRPL "prpl-mimi"

/*
 * libpurple 2 needs event-loop operations; without them it aborts on the
 * first timeout. These are the glib ones from nullclient.
 */
typedef struct {
    PurpleInputFunction function;
    guint result;
    gpointer data;
} PurpleGLibIOClosure;

static void purple_glib_io_destroy(gpointer data)
{
    g_free(data);
}

static gboolean purple_glib_io_invoke(GIOChannel *source, GIOCondition condition,
                                      gpointer data)
{
    PurpleGLibIOClosure *closure = data;
    PurpleInputCondition purple_cond = 0;

    if (condition & (G_IO_IN | G_IO_HUP | G_IO_ERR))
        purple_cond |= PURPLE_INPUT_READ;
    if (condition & (G_IO_OUT | G_IO_HUP | G_IO_ERR | G_IO_NVAL))
        purple_cond |= PURPLE_INPUT_WRITE;

    closure->function(closure->data, g_io_channel_unix_get_fd(source),
                      purple_cond);
    return TRUE;
}

static guint glib_input_add(gint fd, PurpleInputCondition condition,
                            PurpleInputFunction function, gpointer data)
{
    PurpleGLibIOClosure *closure = g_new0(PurpleGLibIOClosure, 1);
    GIOChannel *channel;
    GIOCondition cond = 0;

    closure->function = function;
    closure->data = data;

    if (condition & PURPLE_INPUT_READ)
        cond |= G_IO_IN | G_IO_HUP | G_IO_ERR;
    if (condition & PURPLE_INPUT_WRITE)
        cond |= G_IO_OUT | G_IO_HUP | G_IO_ERR | G_IO_NVAL;

    channel = g_io_channel_unix_new(fd);
    closure->result = g_io_add_watch_full(channel, G_PRIORITY_DEFAULT, cond,
                                          purple_glib_io_invoke, closure,
                                          purple_glib_io_destroy);
    g_io_channel_unref(channel);
    return closure->result;
}

static PurpleEventLoopUiOps glib_eventloops = {
    g_timeout_add,
    g_source_remove,
    glib_input_add,
    g_source_remove,
    NULL,
    g_timeout_add_seconds,
    NULL,
    NULL,
    NULL,
};

static PurpleCoreUiOps core_uiops = {
    NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
};

int main(int argc, char **argv)
{
    const char *plugin_dir;
    const char *user_dir;
    GList *protocols;
    PurplePlugin *prpl;
    guint before, after;

    if (argc < 3) {
        fprintf(stderr, "usage: %s <plugin dir> <user dir>\n", argv[0]);
        return 2;
    }
    plugin_dir = argv[1];
    user_dir = argv[2];

    /* Keep every trace of this run inside the given directory. */
    purple_util_set_user_dir(user_dir);
    purple_debug_set_enabled(TRUE);

    /* No account or blist file is written. */
    purple_core_set_ui_ops(&core_uiops);
    purple_eventloop_set_ui_ops(&glib_eventloops);

    purple_plugins_add_search_path(plugin_dir);

    protocols = purple_plugins_get_protocols();
    before = g_list_length(protocols);

    if (!purple_core_init(UI_ID)) {
        fprintf(stderr, "FAIL: libpurple would not start\n");
        return 1;
    }

    /*
     * purple_core_init probes the search paths; a protocol plugin is
     * loaded at once because libpurple needs it to resolve accounts.
     */
    protocols = purple_plugins_get_protocols();
    after = g_list_length(protocols);
    printf("protocols before init: %u, after: %u\n", before, after);

    prpl = purple_plugins_find_with_id(EXPECT_PRPL);
    if (prpl == NULL) {
        fprintf(stderr, "FAIL: %s was not registered\n", EXPECT_PRPL);
        fprintf(stderr, "      (the plugin did not load; its load function "
                        "may have refused)\n");
        purple_core_quit();
        return 1;
    }
    if (!purple_plugin_is_loaded(prpl)) {
        fprintf(stderr, "FAIL: %s was found but is not loaded\n", EXPECT_PRPL);
        purple_core_quit();
        return 1;
    }

    printf("OK: %s loaded: %s %s\n", EXPECT_PRPL, prpl->info->name,
           prpl->info->version ? prpl->info->version : "");

    /* Unload through the core, which runs the plugin's unload. */
    purple_core_quit();
    printf("OK: libpurple shut the plugin down cleanly\n");
    return 0;
}
