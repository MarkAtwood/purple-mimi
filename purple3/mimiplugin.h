/* SPDX-License-Identifier: GPL-3.0-or-later
 *
 * The libpurple 3 plugin of purple-mimi: its GPlugin entry points, the
 * process-wide library, the event pump, and what the protocol and the
 * connection share (docs/design/purple-mimi.md §9).
 */

#ifndef PURPLE_MIMI_PLUGIN_H
#define PURPLE_MIMI_PLUGIN_H

#include <glib.h>

#include <purple.h>

#include "purple-mimi.h"

G_BEGIN_DECLS

/** The protocol id libpurple knows us by. INVENTED. */
#define PURPLE_MIMI_PROTOCOL_ID "prpl-mimi"

/** Account setting keys (§8.1). INVENTED names, stable once released. */
/*
 * libpurple 3 (2.98.0) has no account username: purple_account_new takes
 * a user-supplied name and everything else is an account setting, so the
 * MIMI user is a setting here, as the demo protocol's "id" is. §9 assumed
 * a username; found while building step 4.
 */
#define PURPLE_MIMI_SETTING_USER "user"
#define PURPLE_MIMI_SETTING_PROVIDER_URL "provider-url"
#define PURPLE_MIMI_SETTING_INVITES "invites"
#define PURPLE_MIMI_SETTING_TOKEN "token"

/** `PURPLE_MIMI_SETTING_INVITES` values, as stored. */
#define PURPLE_MIMI_INVITES_FROM_ALLOWED "from-allowed"
#define PURPLE_MIMI_INVITES_ANY "any"
#define PURPLE_MIMI_INVITES_NEVER "never"

/** The stored invite setting as the library's value. */
mimi_invite_policy purple_mimi_invite_policy_of(const char *value);

/**
 * The MIMI user of `account`: its "user" setting, or the account's name
 * when that is empty, or NULL when neither is set.
 */
const char *purple_mimi_account_user(PurpleAccount *account);

/**
 * Remembers that `handle` belongs to `connection`, so the event pump can
 * route the library's events. `connection` is not referenced: it owns the
 * handle and forgets it before closing.
 */
void purple_mimi_plugin_track(mimi_account *handle, gpointer connection);

/** Forgets `handle`; called before the handle is closed. */
void purple_mimi_plugin_forget(mimi_account *handle);

G_END_DECLS

#endif /* PURPLE_MIMI_PLUGIN_H */
