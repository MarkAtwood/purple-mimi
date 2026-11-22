/* SPDX-License-Identifier: GPL-3.0-or-later
 *
 * purple-mimi's PurpleConnection for libpurple 3
 * (docs/design/purple-mimi.md §9).
 *
 * Step 4 of §13: connect, disconnect, report errors. Rooms, messages and
 * the user's actions are steps 5 and 6.
 *
 * libpurple 3 connects asynchronously, so connect_async starts the
 * library's open and keeps the GTask; the event pump finishes it when
 * MIMI_EV_CONNECTED or MIMI_EV_DISCONNECTED arrives
 * (purple_mimi_connection_report). A loss after that has no GTask to
 * fail, so it becomes a connection-error notification and a disconnect.
 */

#include <glib/gi18n-lib.h>
#include <string.h>

#include "mimiconnection.h"

#include "mimiplugin.h"

struct _PurpleMimiConnection {
    PurpleConnection parent;

    /** The library's account, or NULL before open and after close. */
    mimi_account *handle;
    /** The connect in flight, or NULL once it has been finished. */
    GTask *connecting;
};

G_DEFINE_DYNAMIC_TYPE_EXTENDED(PurpleMimiConnection, purple_mimi_connection,
                               PURPLE_TYPE_CONNECTION, G_TYPE_FLAG_FINAL, {})

/*
 * §8.2 and §9: libpurple 3's PurpleConnectionError is narrower than
 * 2.14's — there is no AUTHENTICATION_FAILED, NAME_IN_USE or
 * INVALID_USERNAME — so several statuses land on CUSTOM_FATAL, whose
 * text carries what happened. The library's own `temporary` decides
 * between the temporary and fatal custom errors, so libpurple does not
 * retry something hopeless.
 */
static PurpleConnectionError connection_error_for(mimi_status status,
                                                  gboolean temporary)
{
    switch (status) {
    case MIMI_ERR_NETWORK:
        return PURPLE_CONNECTION_ERROR_NETWORK_ERROR;
    case MIMI_ERR_STREAM_OFF:
    case MIMI_ERR_WRONG_USER:
        return PURPLE_CONNECTION_ERROR_INVALID_SETTINGS;
    case MIMI_ERR_UNAUTHORIZED:
    case MIMI_ERR_TAKEN_OVER:
    case MIMI_ERR_LOCKED:
    case MIMI_ERR_LOCK_LOST:
        return PURPLE_CONNECTION_ERROR_CUSTOM_FATAL;
    default:
        return temporary ? PURPLE_CONNECTION_ERROR_CUSTOM_TEMPORARY
                         : PURPLE_CONNECTION_ERROR_OTHER_ERROR;
    }
}

/* Tells the UI, for a failure with no GTask left to carry it. */
static void notify_error(PurpleConnection *connection, const char *text)
{
    PurpleNotificationManager *manager = NULL;
    PurpleNotification *notification = NULL;
    PurpleAccount *account = purple_connection_get_account(connection);

    notification = purple_notification_connection_error_new(NULL, account);
    if (notification == NULL)
        return;
    purple_notification_set_title(notification, text);
    manager = purple_core_get_notification_manager(purple_core_get_default());
    if (manager != NULL)
        purple_notification_manager_add(manager, notification);
    g_object_unref(notification);
}

void purple_mimi_connection_report(PurpleMimiConnection *connection,
                                   mimi_status status, gboolean temporary,
                                   const char *text)
{
    const char *message = (text != NULL && *text != '\0')
                              ? text
                              : _("purple-mimi failed");

    g_return_if_fail(PURPLE_MIMI_IS_CONNECTION(connection));

    if (connection->connecting != NULL) {
        GTask *task = g_steal_pointer(&connection->connecting);

        if (status == MIMI_OK) {
            purple_account_ready(purple_connection_get_account(
                PURPLE_CONNECTION(connection)));
            g_task_return_boolean(task, TRUE);
        } else {
            g_task_return_new_error(
                task, PURPLE_CONNECTION_ERROR,
                connection_error_for(status, temporary), "%s", message);
        }
        g_clear_object(&task);
        return;
    }

    if (status == MIMI_OK) {
        /* Already connected and told so; nothing to do. */
        return;
    }

    /*
     * Lost after connecting: no task is waiting, so the UI is told
     * directly and the connection is taken down (§7.4).
     */
    notify_error(PURPLE_CONNECTION(connection), message);
    purple_connection_disconnect_async(PURPLE_CONNECTION(connection), message,
                                       NULL, NULL, NULL);
}

mimi_account *purple_mimi_connection_handle(PurpleMimiConnection *connection)
{
    g_return_val_if_fail(PURPLE_MIMI_IS_CONNECTION(connection), NULL);

    return connection->handle;
}

void purple_mimi_connection_forget(PurpleMimiConnection *connection)
{
    g_return_if_fail(PURPLE_MIMI_IS_CONNECTION(connection));

    connection->handle = NULL;
}

static void purple_mimi_connection_connect_async(PurpleConnection *connection,
                                                 GCancellable *cancellable,
                                                 GAsyncReadyCallback callback,
                                                 gpointer data)
{
    PurpleMimiConnection *self = PURPLE_MIMI_CONNECTION(connection);
    PurpleAccount *account = purple_connection_get_account(connection);
    PurpleAccountSettings *settings = purple_account_get_settings(account);
    mimi_account_config config;
    mimi_account *handle = NULL;
    char *error = NULL;
    mimi_status status;
    GTask *task = g_task_new(connection, cancellable, callback, data);
    const char *base_dir = purple_data_dir();
    const char *url;

    memset(&config, 0, sizeof config);
    config.base_dir = base_dir;
    config.user = purple_mimi_account_user(account);
    url = purple_account_settings_get_string(settings,
                                             PURPLE_MIMI_SETTING_PROVIDER_URL,
                                             NULL);
    config.provider_url = url;
    /*
     * §12.3: libpurple 3 keeps the token in its credential manager until
     * the first connection succeeds; after that the state directory has
     * it and NULL means "use the stored one". Asking the credential
     * manager is step 5's: for now an account connects with the token
     * already in its state directory, or with the one in the settings.
     */
    config.token = purple_account_settings_get_string(
        settings, PURPLE_MIMI_SETTING_TOKEN, NULL);
    config.invite_policy = purple_mimi_invite_policy_of(
        purple_account_settings_get_string(settings,
                                           PURPLE_MIMI_SETTING_INVITES, NULL));
    config.key_packages = 0;    /* the library's defaults */
    config.sync_interval_ms = 0;
    /* §6.6: taking an account back is an explicit action, not a connect. */
    config.take_over = FALSE;

    status = mimi_account_open(&config, &handle, &error);
    if (status != MIMI_OK) {
        g_task_return_new_error(task, PURPLE_CONNECTION_ERROR,
                                connection_error_for(status, FALSE), "%s",
                                error ? error : "the account could not be opened");
        mimi_string_free(error);
        g_clear_object(&task);
        return;
    }
    mimi_string_free(error);

    self->handle = handle;
    self->connecting = task;    /* finished by the event pump */
    purple_mimi_plugin_track(handle, self);
}

static gboolean purple_mimi_connection_connect_finish(
    PurpleConnection *connection, GAsyncResult *result, GError **error)
{
    g_return_val_if_fail(PURPLE_MIMI_IS_CONNECTION(connection), FALSE);
    g_return_val_if_fail(g_task_is_valid(result, connection), FALSE);

    return g_task_propagate_boolean(G_TASK(result), error);
}

static void purple_mimi_connection_disconnect_async(
    PurpleConnection *connection, G_GNUC_UNUSED const char *message,
    GCancellable *cancellable, GAsyncReadyCallback callback, gpointer data)
{
    PurpleMimiConnection *self = PURPLE_MIMI_CONNECTION(connection);
    GTask *task = g_task_new(connection, cancellable, callback, data);

    if (self->handle != NULL) {
        mimi_account *handle = g_steal_pointer(&self->handle);

        /* Untracked first: the handle dies inside mimi_account_close. */
        purple_mimi_plugin_forget(handle);
        mimi_account_close(handle);
    }
    /*
     * A connect still in flight cannot be left unfinished, or libpurple
     * waits for a callback that will never come.
     */
    if (self->connecting != NULL) {
        GTask *pending = g_steal_pointer(&self->connecting);

        g_task_return_new_error(pending, PURPLE_CONNECTION_ERROR,
                                PURPLE_CONNECTION_ERROR_CUSTOM_TEMPORARY,
                                "%s", _("the account was disconnected while "
                                        "it was connecting"));
        g_clear_object(&pending);
    }

    g_task_return_boolean(task, TRUE);
    g_clear_object(&task);
}

static gboolean purple_mimi_connection_disconnect_finish(
    PurpleConnection *connection, GAsyncResult *result, GError **error)
{
    g_return_val_if_fail(PURPLE_MIMI_IS_CONNECTION(connection), FALSE);
    g_return_val_if_fail(g_task_is_valid(result, connection), FALSE);

    return g_task_propagate_boolean(G_TASK(result), error);
}

static void purple_mimi_connection_finalize(GObject *obj)
{
    PurpleMimiConnection *self = PURPLE_MIMI_CONNECTION(obj);

    if (self->handle != NULL) {
        mimi_account *handle = g_steal_pointer(&self->handle);

        purple_mimi_plugin_forget(handle);
        mimi_account_close(handle);
    }
    g_clear_object(&self->connecting);

    G_OBJECT_CLASS(purple_mimi_connection_parent_class)->finalize(obj);
}

static void purple_mimi_connection_init(
    G_GNUC_UNUSED PurpleMimiConnection *connection)
{
}

static void purple_mimi_connection_class_finalize(
    G_GNUC_UNUSED PurpleMimiConnectionClass *klass)
{
}

static void purple_mimi_connection_class_init(PurpleMimiConnectionClass *klass)
{
    GObjectClass *obj_class = G_OBJECT_CLASS(klass);
    PurpleConnectionClass *connection_class = PURPLE_CONNECTION_CLASS(klass);

    obj_class->finalize = purple_mimi_connection_finalize;

    connection_class->connect_async = purple_mimi_connection_connect_async;
    connection_class->connect_finish = purple_mimi_connection_connect_finish;
    connection_class->disconnect_async =
        purple_mimi_connection_disconnect_async;
    connection_class->disconnect_finish =
        purple_mimi_connection_disconnect_finish;
}

void purple_mimi_connection_register(GPluginNativePlugin *plugin)
{
    purple_mimi_connection_register_type(G_TYPE_MODULE(plugin));
}
