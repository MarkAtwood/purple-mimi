/* SPDX-License-Identifier: GPL-3.0-or-later
 *
 * purple-mimi's PurpleConnection for libpurple 3
 * (docs/design/purple-mimi.md §9).
 */

#ifndef PURPLE_MIMI_CONNECTION_H
#define PURPLE_MIMI_CONNECTION_H

#include <glib.h>
#include <glib-object.h>

#include <gplugin.h>
#include <gplugin-native.h>

#include <purple.h>

#include "purple-mimi.h"

G_BEGIN_DECLS

#define PURPLE_MIMI_TYPE_CONNECTION (purple_mimi_connection_get_type())

G_DECLARE_FINAL_TYPE(PurpleMimiConnection, purple_mimi_connection,
                     PURPLE_MIMI, CONNECTION, PurpleConnection)

/** Registers the type with the plugin's GTypeModule. */
void purple_mimi_connection_register(GPluginNativePlugin *plugin);

/**
 * Finishes the connect this account was waiting on, reporting the
 * library's event. Called by the event pump, never by libpurple.
 *
 * `status` is `MIMI_OK` for a successful connect; anything else is a
 * failure, whose `text` reaches the user.
 */
void purple_mimi_connection_report(PurpleMimiConnection *connection,
                                   mimi_status status, gboolean temporary,
                                   const char *text);

/** This connection's library handle, or NULL once it is closed. */
mimi_account *purple_mimi_connection_handle(PurpleMimiConnection *connection);

/** Forgets the library handle, which the library has invalidated. */
void purple_mimi_connection_forget(PurpleMimiConnection *connection);

G_END_DECLS

#endif /* PURPLE_MIMI_CONNECTION_H */
