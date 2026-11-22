/* SPDX-License-Identifier: GPL-3.0-or-later
 *
 * purple-mimi's PurpleProtocol for libpurple 3
 * (docs/design/purple-mimi.md §9).
 *
 * Step 4 of §13: the account's settings and its connection. The
 * conversation, contacts and directory interfaces are steps 5 and 6.
 */

#include <glib/gi18n-lib.h>

#include "mimiprotocol.h"

#include "mimiconnection.h"
#include "mimiplugin.h"

struct _PurpleMimiProtocol {
    PurpleProtocol parent;
};

G_DEFINE_DYNAMIC_TYPE_EXTENDED(PurpleMimiProtocol, purple_mimi_protocol,
                               PURPLE_TYPE_PROTOCOL, G_TYPE_FLAG_FINAL, {})

static PurpleConnection *purple_mimi_protocol_create_connection(
    G_GNUC_UNUSED PurpleProtocol *protocol, PurpleAccount *account,
    G_GNUC_UNUSED GError **error)
{
    g_return_val_if_fail(PURPLE_IS_ACCOUNT(account), NULL);

    return g_object_new(PURPLE_MIMI_TYPE_CONNECTION, "account", account, NULL);
}

/* §8.1: the provider URL and the invite policy. */
static PurpleAccountSettings *purple_mimi_protocol_get_default_account_settings(
    G_GNUC_UNUSED PurpleProtocol *protocol)
{
    PurpleAccountSettings *settings = purple_account_settings_new();
    PurpleAccountSetting *setting = NULL;

    setting = purple_account_setting_string_new(
        PURPLE_MIMI_SETTING_USER, _("MIMI user (alice@a.example)"), NULL);
    purple_account_setting_set_weight(setting, 0);
    purple_account_settings_add_setting(settings, setting);

    setting = purple_account_setting_string_new(
        PURPLE_MIMI_SETTING_PROVIDER_URL, _("Provider URL"), NULL);
    purple_account_setting_set_weight(setting, 10);
    purple_account_settings_add_setting(settings, setting);

    setting = purple_account_setting_string_new(
        PURPLE_MIMI_SETTING_INVITES, _("Join invites"),
        PURPLE_MIMI_INVITES_FROM_ALLOWED);
    purple_account_setting_set_weight(setting, 20);
    purple_account_setting_set_advanced(setting, TRUE);
    purple_account_settings_add_setting(settings, setting);

    /*
     * §12.3: the token is kept in the state directory after the first
     * connection. Until step 5 asks the credential manager for it, an
     * account can carry it here.
     */
    setting = purple_account_setting_string_new(PURPLE_MIMI_SETTING_TOKEN,
                                                _("Provider token"), NULL);
    purple_account_setting_set_weight(setting, 30);
    purple_account_setting_set_advanced(setting, TRUE);
    purple_account_settings_add_setting(settings, setting);

    return settings;
}

/* §8.1: the username must be a handle or a MIMI user URI. */
static gboolean purple_mimi_protocol_validate_account(
    G_GNUC_UNUSED PurpleProtocol *protocol, PurpleAccount *account,
    GError **error)
{
    const char *username = NULL;
    char *mapped = NULL;

    g_return_val_if_fail(PURPLE_IS_ACCOUNT(account), FALSE);

    username = purple_mimi_account_user(account);
    if (username == NULL) {
        g_set_error_literal(error, PURPLE_PROTOCOL_ERROR, 0,
                            _("A MIMI user is needed, such as "
                              "alice@a.example"));
        return FALSE;
    }
    /* The library owns the forms; NULL means it is neither (§8.1). */
    mapped = mimi_handle_to_user(username);
    if (mapped == NULL) {
        g_set_error(error, PURPLE_PROTOCOL_ERROR, 0,
                    _("%s is not a MIMI user: give a handle such as "
                      "alice@a.example, or a mimi://<domain>/u/<id> URI"),
                    username);
        return FALSE;
    }
    mimi_string_free(mapped);
    return TRUE;
}

static char *purple_mimi_protocol_generate_account_name(
    G_GNUC_UNUSED PurpleProtocol *protocol, PurpleAccount *account,
    guint n_accounts)
{
    const char *username = purple_mimi_account_user(account);

    if (username != NULL)
        return g_strdup(username);
    return g_strdup_printf("MIMI %u", n_accounts + 1);
}

static void purple_mimi_protocol_init(
    G_GNUC_UNUSED PurpleMimiProtocol *protocol)
{
}

static void purple_mimi_protocol_class_finalize(
    G_GNUC_UNUSED PurpleMimiProtocolClass *klass)
{
}

static void purple_mimi_protocol_class_init(PurpleMimiProtocolClass *klass)
{
    PurpleProtocolClass *protocol_class = PURPLE_PROTOCOL_CLASS(klass);

    protocol_class->create_connection =
        purple_mimi_protocol_create_connection;
    protocol_class->get_default_account_settings =
        purple_mimi_protocol_get_default_account_settings;
    protocol_class->validate_account = purple_mimi_protocol_validate_account;
    protocol_class->generate_account_name =
        purple_mimi_protocol_generate_account_name;
}

void purple_mimi_protocol_register(GPluginNativePlugin *plugin)
{
    purple_mimi_protocol_register_type(G_TYPE_MODULE(plugin));
}

PurpleProtocol *purple_mimi_protocol_new(void)
{
    return g_object_new(PURPLE_MIMI_TYPE_PROTOCOL, "id",
                        PURPLE_MIMI_PROTOCOL_ID, "name", "MIMI",
                        "description",
                        "Chat in MIMI rooms over MLS, through mimid's agent "
                        "embedded in this process.",
                        NULL);
}
