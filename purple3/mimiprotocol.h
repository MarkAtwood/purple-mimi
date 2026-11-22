/* SPDX-License-Identifier: GPL-3.0-or-later
 *
 * purple-mimi's PurpleProtocol for libpurple 3
 * (docs/design/purple-mimi.md §9).
 */

#ifndef PURPLE_MIMI_PROTOCOL_H
#define PURPLE_MIMI_PROTOCOL_H

#include <glib.h>
#include <glib-object.h>

#include <gplugin.h>
#include <gplugin-native.h>

#include <purple.h>

G_BEGIN_DECLS

#define PURPLE_MIMI_TYPE_PROTOCOL (purple_mimi_protocol_get_type())

G_DECLARE_FINAL_TYPE(PurpleMimiProtocol, purple_mimi_protocol, PURPLE_MIMI,
                     PROTOCOL, PurpleProtocol)

/** Registers the type with the plugin's GTypeModule. */
void purple_mimi_protocol_register(GPluginNativePlugin *plugin);

/** A new protocol object, for the protocol manager. */
PurpleProtocol *purple_mimi_protocol_new(void);

G_END_DECLS

#endif /* PURPLE_MIMI_PROTOCOL_H */
