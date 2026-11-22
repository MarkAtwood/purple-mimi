// SPDX-License-Identifier: GPL-3.0-or-later
//! The purple front end over the store (docs/design/purple-mimi.md
//! §5.6): one [`mimid_agent_store::Frontend`] that turns the store's
//! [`Change`]s into the events of §5.5, and renders what libpurple 2
//! shows.
//!
//! A `Change` names a record, not what moved in it, so
//! `Change::MessageChanged` could be an edit, a retraction, a reaction
//! or an attachment that arrived. The front end therefore keeps what it
//! last reported for each message in the record's `ext["purple"]` (the
//! store's per-front-end map) and reports the difference. Because that
//! lives in the record, it survives a restart and a takeover: the second
//! instance does not replay old edits.
//!
//! `ext["purple"]` also holds each message's number in its room (§8.7),
//! given in arrival order, and each room's next number.
//!
//! The text forms (§8.6 to §8.8), the HTML escaping (§12.4) and the
//! diffing are all here, in Rust, where they are tested. The C glue only
//! places the lines this produces.
//!
//! # Limitations
//!
//! - A message's number is given when this front end first reports it,
//!   so two instances that each store messages while the other is away
//!   can number them in a different order than a single instance would.
//!   The number is a local label, not an identifier.
//! - `line_html` is libpurple 2's HTML. The libpurple 3 glue wants plain
//!   text and builds its own lines (§9).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use mimid_agent_store::record::Message;
use mimid_agent_store::{Change, Frontend, Store};

use crate::events::{Event, EventKind, EventQueue, MessageChange, ReactionChange, RoomChange};

/// Our key in a record's `ext` map (the store's per-front-end map).
pub const EXT_KEY: &str = "purple";

/// The longest body rendered into a line; past this the line carries the
/// beginning and says how much was left out, so that one enormous
/// message cannot wedge a conversation window. INVENTED, NOT FROM SPEC.
pub const MAX_RENDERED_BODY: usize = 16 * 1024;

/// What this front end last reported about one message, kept in its
/// `ext["purple"]` so a restart or a takeover does not report it again.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Reported {
    /// The message's number in its room (§8.7), given in arrival order.
    pub number: u64,
    /// How many revisions had been reported: a new one is an edit.
    pub revisions: usize,
    /// Whether the message had been reported as retracted or deleted.
    pub deleted: bool,
    /// The reaction keys reported, so one gone is a removal.
    pub reactions: Vec<String>,
    /// How many attachments had arrived. A received attachment is in the
    /// record only once fetched: mimid keeps the ones still being
    /// retried outside the message (its `retry` module), so a new one
    /// appearing is one that became available.
    pub attachments: usize,
}

impl Reported {
    /// What `message` says now, as this front end would report it.
    fn of(message: &Message, number: u64) -> Self {
        Self {
            number,
            revisions: message.edit_history.len(),
            deleted: message.deleted.is_some(),
            reactions: message.reactions.iter().map(|r| r.key.clone()).collect(),
            attachments: message.attachments.len(),
        }
    }
}

/// What a room keeps in `ext["purple"]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RoomExt {
    /// The number the next message in this room is given (§8.7).
    pub next_number: u64,
}

/// Escapes `text` for libpurple 2's HTML (§12.4): the agent passes a
/// peer's text through as data and never lets it become markup.
///
/// Also replaces the characters a conversation window must not be given
/// raw: NUL and the other C0 controls (except tab, which becomes
/// spaces), and the bidi and zero-width formatting characters that could
/// make a line read as something else. They become the Unicode
/// replacement character, so the text stays visibly odd rather than
/// silently reordered.
#[must_use]
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);

    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\n' => out.push_str("<br>"),
            '\t' => out.push_str("    "),
            '\r' => {}
            // Bidi overrides and embeddings, and the zero-width and
            // invisible formatting characters: a line that reorders
            // itself is a line that lies about its sender.
            '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
            | '\u{061c}' => out.push('\u{fffd}'),
            c if c.is_control() => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

/// `body` as a libpurple 2 HTML fragment, cut at [`MAX_RENDERED_BODY`]
/// bytes on a character boundary with a note of what was left out.
#[must_use]
pub fn render_body(body: &str) -> String {
    if body.len() <= MAX_RENDERED_BODY {
        return escape_html(body);
    }
    let mut end = MAX_RENDERED_BODY;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let left = body.len() - end;
    format!(
        "{}<br><i>[{left} more bytes not shown]</i>",
        escape_html(&body[..end])
    )
}

/// The whole line libpurple 2 shows for `message` (§8.6 to §8.8): the
/// body, with the text forms for an edit, a retraction, a reply and the
/// message's number.
#[must_use]
pub fn render_line(
    message: &Message,
    show_numbers: bool,
    number: u64,
    reply_to: Option<u64>,
) -> String {
    let mut line = String::new();

    if show_numbers {
        line.push_str(&format!("<font color=\"#808080\">[{number}]</font> "));
    }
    // §8.7: a reply says what it answers, since libpurple 2 cannot show
    // a thread. `reply_to` is the target's number when it is stored here
    // and this front end has numbered it.
    if message.in_reply_to.is_some() {
        match reply_to {
            Some(target) => line.push_str(&format!(
                "<font color=\"#808080\">&gt;&gt; [{target}]</font> "
            )),
            None => line.push_str("<font color=\"#808080\">&gt;&gt; an earlier message</font> "),
        }
    }

    if message.deleted.is_some() {
        // §8.6: a retraction leaves the line in place, saying so.
        line.push_str("<i>(deleted)</i>");
        return line;
    }

    line.push_str(&render_body(&message.body));

    if message.edited_at.is_some() {
        line.push_str(" <i>(edited)</i>");
    }

    // §8.9: an attachment is a line of its own below the text.
    for (index, attachment) in message.attachments.iter().enumerate() {
        line.push_str(&format!(
            "<br><i>[attachment {}: {} ({}, {} bytes)]</i>",
            index + 1,
            escape_html(&attachment.file_name),
            escape_html(&attachment.media_type),
            attachment.size
        ));
    }

    // §8.8: reactions, counted, since libpurple 2 has none of its own.
    let counts = message.reaction_counts();
    if !counts.is_empty() {
        let shown: Vec<String> = counts
            .iter()
            .map(|(reaction, senders)| {
                if senders.len() > 1 {
                    format!("{} {}", escape_html(reaction), senders.len())
                } else {
                    escape_html(reaction)
                }
            })
            .collect();
        line.push_str(&format!(
            "<br><font color=\"#808080\">{}</font>",
            shown.join("  ")
        ));
    }

    line
}

/// The front end registered on the store: it reports what changed to the
/// process's event queue, and keeps its bookkeeping in the records.
#[derive(Debug)]
pub struct PurpleFrontend {
    queue: Arc<EventQueue>,
    account: crate::events::AccountId,
}

impl PurpleFrontend {
    /// A front end reporting `account`'s changes to `queue`.
    #[must_use]
    pub fn new(queue: Arc<EventQueue>, account: crate::events::AccountId) -> Self {
        Self { queue, account }
    }

    fn push(&self, kind: EventKind) {
        self.queue.push(Event {
            account: Some(self.account),
            kind,
        });
    }
}

/// Reads our part of a record's `ext`, or the default when it is absent
/// or not what we wrote. A front end must never fail on another's data.
fn ext_of<T: serde::de::DeserializeOwned + Default>(
    ext: &BTreeMap<String, serde_json::Value>,
) -> T {
    ext.get(EXT_KEY)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default()
}

/// Writes our part of a record's `ext`, leaving every other front end's
/// alone.
fn set_ext<T: serde::Serialize>(ext: &mut BTreeMap<String, serde_json::Value>, value: &T) {
    if let Ok(value) = serde_json::to_value(value) {
        ext.insert(EXT_KEY.to_owned(), value);
    }
}

impl Frontend for PurpleFrontend {
    fn before_save<'a>(
        &'a self,
        store: &'a Store,
        changes: &'a [Change],
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = store.state();

            for change in changes {
                match change {
                    Change::RoomAdded(key) => {
                        self.push(EventKind::Room {
                            change: RoomChange::Added,
                            room: key.clone(),
                        });
                    }
                    Change::RoomChanged(key) => {
                        self.push(EventKind::Room {
                            change: RoomChange::Changed,
                            room: key.clone(),
                        });
                    }
                    Change::RoomLeft(key) => {
                        self.push(EventKind::Room {
                            change: RoomChange::Left,
                            room: key.clone(),
                        });
                    }
                    Change::RoomForgotten { key, .. } => {
                        self.push(EventKind::Room {
                            change: RoomChange::Forgotten,
                            room: key.clone(),
                        });
                    }
                    Change::MessageAdded(key) => {
                        // Number it now, in arrival order (§8.7), and
                        // remember what we reported.
                        let Some(message) = state.messages.get(key) else {
                            continue;
                        };
                        let room_key = message.room.clone();
                        let number = {
                            let mut room_ext: RoomExt = state
                                .rooms
                                .get(&room_key)
                                .map(|room| ext_of(&room.ext))
                                .unwrap_or_default();
                            room_ext.next_number += 1;
                            let number = room_ext.next_number;
                            if let Some(room) = state.rooms.get_mut(&room_key) {
                                set_ext(&mut room.ext, &room_ext);
                            }
                            number
                        };
                        if let Some(message) = state.messages.get_mut(key) {
                            let reported = Reported::of(message, number);
                            set_ext(&mut message.ext, &reported);
                        }
                        self.push(EventKind::Message {
                            change: MessageChange::New,
                            message: key.clone(),
                            room: room_key,
                        });
                    }
                    Change::MessageChanged(key) => {
                        let Some(message) = state.messages.get(key) else {
                            continue;
                        };
                        let before: Reported = ext_of(&message.ext);
                        let now = Reported::of(message, before.number);
                        let room_key = message.room.clone();

                        // One Change can mean several things at once; each
                        // difference is reported on its own.
                        if now.deleted && !before.deleted {
                            self.push(EventKind::Message {
                                change: MessageChange::Retracted,
                                message: key.clone(),
                                room: room_key.clone(),
                            });
                        } else if now.revisions > before.revisions {
                            self.push(EventKind::Message {
                                change: MessageChange::Edited,
                                message: key.clone(),
                                room: room_key.clone(),
                            });
                        }
                        if now.attachments > before.attachments {
                            self.push(EventKind::Message {
                                change: MessageChange::AttachmentReady,
                                message: key.clone(),
                                room: room_key.clone(),
                            });
                        }
                        for reaction in message.reactions.iter() {
                            if !before.reactions.contains(&reaction.key) {
                                self.push(EventKind::Reaction {
                                    change: ReactionChange::Added,
                                    message: key.clone(),
                                    reaction_key: reaction.key.clone(),
                                    sender: reaction.sender.clone(),
                                    reaction: reaction.reaction.clone(),
                                });
                            }
                        }
                        for gone in before
                            .reactions
                            .iter()
                            .filter(|key| !now.reactions.contains(key))
                        {
                            self.push(EventKind::Reaction {
                                change: ReactionChange::Removed,
                                message: key.clone(),
                                reaction_key: gone.clone(),
                                sender: String::new(),
                                reaction: String::new(),
                            });
                        }
                        if let Some(message) = state.messages.get_mut(key) {
                            set_ext(&mut message.ext, &now);
                        }
                    }
                    Change::MessageDeleted(key) | Change::MessageExpired(key) => {
                        let change = if matches!(change, Change::MessageExpired(_)) {
                            MessageChange::Expired
                        } else {
                            MessageChange::Deleted
                        };
                        // The record is gone, so its room is not known
                        // from it; the glue finds the line by its key.
                        self.push(EventKind::Message {
                            change,
                            message: key.clone(),
                            room: String::new(),
                        });
                    }
                    Change::PersonAdded(key) => self.push(EventKind::Person {
                        added: true,
                        user: key.clone(),
                    }),
                    Change::PersonChanged(key) => self.push(EventKind::Person {
                        added: false,
                        user: key.clone(),
                    }),
                    _ => {}
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §12.4: a peer's text is data, never markup.
    #[test]
    fn markup_in_a_body_is_escaped() {
        assert_eq!(
            escape_html("<script>alert('x')</script>"),
            "&lt;script&gt;alert(&apos;x&apos;)&lt;/script&gt;"
        );
        assert_eq!(escape_html("a & b"), "a &amp; b");
        assert_eq!(escape_html("\"quoted\""), "&quot;quoted&quot;");
        // An escape must not be escaped twice.
        assert_eq!(escape_html("&amp;"), "&amp;amp;");
    }

    /// AGENTS.md: hostile input. None of these may reach a conversation
    /// window as itself.
    #[test]
    fn control_bidi_and_zero_width_characters_are_replaced() {
        assert_eq!(escape_html("a\0b"), "a\u{fffd}b");
        assert_eq!(escape_html("a\u{7}b"), "a\u{fffd}b");
        // Right-to-left override: the classic filename spoof.
        assert_eq!(escape_html("a\u{202e}b"), "a\u{fffd}b");
        assert_eq!(escape_html("a\u{200b}b"), "a\u{fffd}b");
        assert_eq!(escape_html("a\u{2066}b"), "a\u{fffd}b");
        assert_eq!(escape_html("a\u{feff}b"), "a\u{fffd}b");
        // Newlines and tabs are shown, not dropped.
        assert_eq!(escape_html("a\nb"), "a<br>b");
        assert_eq!(escape_html("a\tb"), "a    b");
        assert_eq!(escape_html("a\r\nb"), "a<br>b");
    }

    #[test]
    fn valid_text_is_left_alone() {
        for text in ["", "hello", "héllo wörld", "日本語", "🎉 emoji", "a/b-c_d"] {
            let escaped = escape_html(text);
            assert!(!escaped.contains('\u{fffd}'), "{text:?} became {escaped:?}");
        }
        assert_eq!(escape_html("日本語"), "日本語");
    }

    #[test]
    fn an_enormous_body_is_cut_on_a_character_boundary() {
        let body = "é".repeat(MAX_RENDERED_BODY);
        let rendered = render_body(&body);
        assert!(rendered.contains("more bytes not shown"), "{rendered:.80}");
        // Cutting mid-character would make invalid UTF-8 or a mojibake
        // tail; the result must still be valid text.
        assert!(!rendered.contains('\u{fffd}'));
        assert!(render_body("short").contains("short"));
        assert!(!render_body("short").contains("not shown"));
    }

    #[test]
    fn our_ext_entry_survives_other_front_ends() {
        let mut ext: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        ext.insert("irc".to_owned(), serde_json::json!({"nick": "alice-a"}));
        let reported = Reported {
            number: 7,
            revisions: 2,
            deleted: false,
            reactions: vec!["r1".to_owned()],
            attachments: 1,
        };
        set_ext(&mut ext, &reported);

        let read: Reported = ext_of(&ext);
        assert_eq!(read, reported);
        assert!(ext.contains_key("irc"), "another front end's entry is kept");
    }

    /// A message record built from JSON, so these tests need no live
    /// account: the store's records are `Deserialize`.
    fn message(extra: serde_json::Value) -> Message {
        let mut base = serde_json::json!({
            "key": "m1",
            "room": "r1",
            "seq": 1,
            "sender": {"user": "mimi://a.example/u/alice"},
            "acceptedAt": 1_700_000_000_000u64,
            "receivedAt": 1_700_000_000_000u64,
            "senderMsgId": "s1",
            "body": "hello",
            "bodyType": "text/plain",
        });
        let (serde_json::Value::Object(base_map), serde_json::Value::Object(extra_map)) =
            (&mut base, &extra)
        else {
            panic!("both must be objects");
        };
        for (key, value) in extra_map {
            base_map.insert(key.clone(), value.clone());
        }
        serde_json::from_value(base).expect("a valid message record")
    }

    #[test]
    fn a_line_carries_the_number_and_the_body() {
        let line = render_line(&message(serde_json::json!({})), true, 4, None);
        assert!(line.contains("[4]"), "{line}");
        assert!(line.contains("hello"), "{line}");
        assert!(!line.contains("(edited)"), "{line}");

        // Without numbers the label is gone but the body stays.
        let plain = render_line(&message(serde_json::json!({})), false, 4, None);
        assert!(!plain.contains("[4]"), "{plain}");
        assert!(plain.contains("hello"), "{plain}");
    }

    /// §8.6: an edit says so, and a retraction replaces the line rather
    /// than leaving the old text visible.
    #[test]
    fn an_edit_and_a_retraction_say_so() {
        let edited = render_line(
            &message(serde_json::json!({"editedAt": 1_700_000_001_000u64})),
            false,
            1,
            None,
        );
        assert!(edited.contains("(edited)"), "{edited}");
        assert!(edited.contains("hello"), "{edited}");

        let retracted = render_line(
            &message(serde_json::json!({
                "body": "",
                "deleted": {"at": 1_700_000_002_000u64, "forAll": true},
            })),
            false,
            1,
            None,
        );
        assert!(retracted.contains("(deleted)"), "{retracted}");
    }

    /// §8.7: a reply names the number of what it answers, and says so
    /// even when that message is not stored here.
    #[test]
    fn a_reply_names_its_target() {
        let with_number = render_line(
            &message(serde_json::json!({"inReplyTo": {"message": "m0"}})),
            false,
            2,
            Some(1),
        );
        assert!(with_number.contains("[1]"), "{with_number}");

        let unknown = render_line(
            &message(serde_json::json!({"inReplyTo": {"message": "m0"}})),
            false,
            2,
            None,
        );
        assert!(unknown.contains("an earlier message"), "{unknown}");
    }

    /// A body is never markup, even inside a whole line (§12.4).
    #[test]
    fn a_hostile_body_cannot_escape_its_line() {
        let line = render_line(
            &message(serde_json::json!({
                "body": "</font><script>alert(1)</script>\u{202e}evil",
            })),
            true,
            1,
            None,
        );
        assert!(!line.contains("<script"), "{line}");
        assert!(line.contains("&lt;script&gt;"), "{line}");
        assert!(!line.contains('\u{202e}'), "{line}");
        // Our own markup is still there, so the line is well formed.
        assert!(
            line.contains("<font color=\"#808080\">[1]</font>"),
            "{line}"
        );
    }

    /// A front end must never fail on what it did not write.
    #[test]
    fn a_junk_ext_entry_reads_as_the_default() {
        for junk in [
            serde_json::json!("not an object"),
            serde_json::json!(42),
            serde_json::json!({"number": "not a number"}),
            serde_json::json!(null),
        ] {
            let mut ext: BTreeMap<String, serde_json::Value> = BTreeMap::new();
            ext.insert(EXT_KEY.to_owned(), junk.clone());
            let read: Reported = ext_of(&ext);
            assert_eq!(read, Reported::default(), "{junk}");
        }
        // An entry with only some fields keeps the ones it has.
        let mut ext: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        ext.insert(EXT_KEY.to_owned(), serde_json::json!({"number": 3}));
        let read: Reported = ext_of(&ext);
        assert_eq!(read.number, 3);
        assert_eq!(read.revisions, 0);
    }
}
