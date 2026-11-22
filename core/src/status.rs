// SPDX-License-Identifier: GPL-3.0-or-later
//! The status codes of the C ABI (docs/design/purple-mimi.md §5.5) and the
//! one place where mimid's errors become them.
//!
//! # Limitations
//!
//! - [`AgentError`], [`ActionError`] and [`SyncError`] are
//!   `#[non_exhaustive]`: a variant mimid adds later maps to
//!   [`Status::Internal`] until it is named here.

use mimid_agent_core::AgentError;
use mimid_agent_store::{ActionError, FileError, SyncError};

/// A status code. The values are the C ABI's `MIMI_OK` and `MIMI_ERR_*`
/// and never change once released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
#[non_exhaustive]
pub enum Status {
    /// Done, or queued.
    Ok = 0,
    /// The header and the library are of different ABI versions.
    Abi = 1,
    /// Called from another thread than the one that called `mimi_init`.
    WrongThread = 2,
    /// NULL, invalid UTF-8, or a value out of range.
    InvalidArgument = 3,
    /// The account is not open.
    NotConnected = 4,
    /// No such room.
    NoSuchRoom = 5,
    /// No such message.
    NoSuchMessage = 6,
    /// No such invite.
    NoSuchInvite = 7,
    /// No such device.
    NoSuchDevice = 8,
    /// The room's policy refuses it.
    NotPermitted = 9,
    /// The hub refused it.
    Refused = 10,
    /// Text or a file past a limit.
    TooLarge = 11,
    /// The provider could not be reached, or the stream failed.
    Network = 12,
    /// The provider refused the token, or there is no token.
    Unauthorized = 13,
    /// The provider does not serve the live stream.
    StreamOff = 14,
    /// The token belongs to another user.
    WrongUser = 15,
    /// Another instance holds the state and did not release it.
    Locked = 16,
    /// Another instance took this account over.
    TakenOver = 17,
    /// The lock file was removed or replaced under us.
    LockLost = 18,
    /// The state directory or database cannot be used.
    State = 19,
    /// A local file could not be read or written.
    Io = 20,
    /// The library panicked.
    Panic = 21,
    /// The account panicked before and was closed.
    Poisoned = 22,
    /// Anything else.
    Internal = 23,
    /// Not initialised (`mimi_init` was not called, or `mimi_shutdown` was).
    NotInitialized = 24,
}

impl Status {
    /// The code as the C ABI has it.
    #[must_use]
    pub fn code(self) -> i32 {
        self as i32
    }
}

/// Why something failed: a status, a text for the user, and whether
/// trying again later may help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The status code.
    pub status: Status,
    /// What happened, for the user.
    pub text: String,
    /// Whether trying again later may help (a network failure), as
    /// opposed to a failure that needs the user (a refused token).
    pub temporary: bool,
}

impl Failure {
    /// A failure that needs the user.
    #[must_use]
    pub fn fatal(status: Status, text: impl Into<String>) -> Self {
        Self {
            status,
            text: text.into(),
            temporary: false,
        }
    }

    /// A failure that may go away by itself.
    #[must_use]
    pub fn temporary(status: Status, text: impl Into<String>) -> Self {
        Self {
            status,
            text: text.into(),
            temporary: true,
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// `error` and its sources, joined with `: `.
fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(next) = source {
        let more = next.to_string();
        if !text.contains(&more) {
            text.push_str(": ");
            text.push_str(&more);
        }
        source = next.source();
    }
    text
}

/// The failure an [`AgentError`] is (docs/design/purple-mimi.md §5.5,
/// §7.1, §7.2).
#[must_use]
pub fn of_agent(error: &AgentError) -> Failure {
    let text = chain(error);
    match error {
        AgentError::Http { .. } | AgentError::Stream { .. } => {
            Failure::temporary(Status::Network, text)
        }
        AgentError::Status {
            status: 401 | 403, ..
        } => Failure::fatal(
            Status::Unauthorized,
            format!("The provider refused the token. ({text})"),
        ),
        AgentError::Status { .. } => Failure::temporary(Status::Network, text),
        // The stream's hello answers a bad token with `Error unauthorized`
        // (mimid-live), which agent-core gives as a method error.
        AgentError::Method { kind, .. } if kind == "unauthorized" || kind == "forbidden" => {
            Failure::fatal(
                Status::Unauthorized,
                format!("The provider refused the token. ({text})"),
            )
        }
        AgentError::Method { kind, .. } if kind == "rateLimited" || kind == "serverUnavailable" => {
            Failure::temporary(Status::Network, text)
        }
        AgentError::StreamOff { url } => Failure::fatal(
            Status::StreamOff,
            format!(
                "The provider at {url} does not serve mimid's live stream. purple-mimi needs a \
                 mimid provider with live_stream = true; ask its operator."
            ),
        ),
        AgentError::WrongUser {
            configured,
            provider,
        } => Failure::fatal(
            Status::WrongUser,
            format!("The token is {provider}'s, not {configured}'s."),
        ),
        AgentError::Locked { .. } => Failure::fatal(Status::Locked, text),
        AgentError::Refused { .. } | AgentError::NoKeyMaterial { .. } => {
            Failure::fatal(Status::Refused, text)
        }
        AgentError::Invalid(_) => Failure::fatal(Status::InvalidArgument, text),
        AgentError::NoSuchRoom(_) => Failure::fatal(Status::NoSuchRoom, text),
        AgentError::NotPermitted { .. } => Failure::fatal(Status::NotPermitted, text),
        AgentError::NoSuchInvite(_) => Failure::fatal(Status::NoSuchInvite, text),
        AgentError::Database { .. } | AgentError::State { .. } => {
            Failure::fatal(Status::State, text)
        }
        _ => Failure::fatal(Status::Internal, text),
    }
}

/// The failure a [`FileError`] of the store is.
#[must_use]
pub fn of_file(error: &FileError) -> Failure {
    let text = chain(error);
    match error {
        FileError::Io { .. } => Failure::fatal(Status::Io, text),
        FileError::Acknowledge(agent) => of_agent(agent),
        _ => Failure::fatal(Status::State, text),
    }
}

/// The failure a [`SyncError`] is.
#[must_use]
pub fn of_sync(error: &SyncError) -> Failure {
    match error {
        SyncError::Agent(agent) => of_agent(agent),
        SyncError::Save(file) => of_file(file),
        _ => Failure::fatal(Status::Internal, chain(error)),
    }
}

/// The failure an [`ActionError`] is.
#[must_use]
pub fn of_action(error: &ActionError) -> Failure {
    let text = chain(error);
    match error {
        ActionError::Agent(agent) => of_agent(agent),
        ActionError::NoSuchRoom(_) | ActionError::NotDirect => {
            Failure::fatal(Status::NoSuchRoom, text)
        }
        ActionError::NoSuchMessage(_)
        | ActionError::NoSuchReaction(_)
        | ActionError::NotEditable(_)
        | ActionError::NotReferable(_)
        | ActionError::NotReportable(_)
        | ActionError::NoSuchBlob(_) => Failure::fatal(Status::NoSuchMessage, text),
        ActionError::NoAdminRole | ActionError::OwnRole => {
            Failure::fatal(Status::NotPermitted, text)
        }
        ActionError::TooManyVersions => Failure::fatal(Status::TooLarge, text),
        ActionError::NotOneRoom | ActionError::InvalidCall(_) => {
            Failure::fatal(Status::InvalidArgument, text)
        }
        ActionError::NoBlobStore | ActionError::Blob(_) => Failure::fatal(Status::Io, text),
        _ => Failure::fatal(Status::Internal, text),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn status(error: &AgentError) -> (Status, bool) {
        let failure = of_agent(error);
        (failure.status, failure.temporary)
    }

    #[test]
    fn agent_errors_map_variant_by_variant() {
        let cases: Vec<(AgentError, Status, bool)> = vec![
            (
                AgentError::Stream {
                    what: "live stream hello".into(),
                    detail: "connection refused".into(),
                },
                Status::Network,
                true,
            ),
            (
                AgentError::Status {
                    what: "session".into(),
                    status: 401,
                    body: String::new(),
                },
                Status::Unauthorized,
                false,
            ),
            (
                AgentError::Status {
                    what: "session".into(),
                    status: 502,
                    body: String::new(),
                },
                Status::Network,
                true,
            ),
            (
                AgentError::Method {
                    method: "live stream hello".into(),
                    kind: "unauthorized".into(),
                    description: String::new(),
                },
                Status::Unauthorized,
                false,
            ),
            (
                AgentError::Method {
                    method: "Room/send".into(),
                    kind: "rateLimited".into(),
                    description: String::new(),
                },
                Status::Network,
                true,
            ),
            (
                AgentError::Method {
                    method: "Room/send".into(),
                    kind: "invalidArguments".into(),
                    description: String::new(),
                },
                Status::Internal,
                false,
            ),
            (
                AgentError::StreamOff {
                    url: "ws://x/mimid/live".into(),
                },
                Status::StreamOff,
                false,
            ),
            (
                AgentError::WrongUser {
                    configured: "mimi://a/u/a".into(),
                    provider: "mimi://a/u/b".into(),
                },
                Status::WrongUser,
                false,
            ),
            (
                AgentError::Locked {
                    path: PathBuf::from("/s/lock"),
                },
                Status::Locked,
                false,
            ),
            (
                AgentError::Malformed {
                    what: "x".into(),
                    detail: "y".into(),
                },
                Status::Internal,
                false,
            ),
            (
                AgentError::Refused {
                    what: "x".into(),
                    reason: "y".into(),
                },
                Status::Refused,
                false,
            ),
            (
                AgentError::NoKeyMaterial {
                    user: "u".into(),
                    code: "noClients".into(),
                },
                Status::Refused,
                false,
            ),
            (
                AgentError::Invalid("x".into()),
                Status::InvalidArgument,
                false,
            ),
            (
                AgentError::NoSuchRoom("r".into()),
                Status::NoSuchRoom,
                false,
            ),
            (
                AgentError::NotPermitted {
                    room: "r".into(),
                    capability: "canChangeRoomSubject",
                },
                Status::NotPermitted,
                false,
            ),
            (AgentError::NoSuchInvite(3), Status::NoSuchInvite, false),
            (
                AgentError::Mls {
                    what: "x".into(),
                    source: "y".into(),
                },
                Status::Internal,
                false,
            ),
            (
                AgentError::Encoding {
                    what: "x".into(),
                    source: "y".into(),
                },
                Status::Internal,
                false,
            ),
            (
                AgentError::Database {
                    path: PathBuf::from("/s/mimid.sqlite"),
                    what: "x".into(),
                    source: "y".into(),
                },
                Status::State,
                false,
            ),
            (
                AgentError::State {
                    path: PathBuf::from("/s"),
                    detail: "x".into(),
                    source: None,
                },
                Status::State,
                false,
            ),
        ];
        for (error, expected, temporary) in cases {
            assert_eq!(status(&error), (expected, temporary), "{error:?}");
        }
    }

    #[test]
    fn the_user_texts_say_what_to_do() {
        let off = of_agent(&AgentError::StreamOff {
            url: "wss://a.example/mimid/live".into(),
        });
        assert!(off.text.contains("live_stream = true"), "{}", off.text);
        let wrong = of_agent(&AgentError::WrongUser {
            configured: "mimi://a.example/u/alice".into(),
            provider: "mimi://a.example/u/bob".into(),
        });
        assert_eq!(
            wrong.text,
            "The token is mimi://a.example/u/bob's, not mimi://a.example/u/alice's."
        );
    }

    #[test]
    fn sync_and_store_errors_map_through_the_agent() {
        let sync = SyncError::Agent(AgentError::StreamOff { url: "u".into() });
        assert_eq!(of_sync(&sync).status, Status::StreamOff);
        let save = SyncError::Save(FileError::Malformed("x".into()));
        assert_eq!(of_sync(&save).status, Status::State);
        let io = FileError::Io {
            path: PathBuf::from("/x"),
            source: std::io::Error::other("disk"),
        };
        assert_eq!(of_file(&io).status, Status::Io);
        assert!(of_file(&io).text.contains("disk"), "the source is kept");
    }

    #[test]
    fn action_errors_map_variant_by_variant() {
        use mimid_agent_store::record::Key;
        let key = || Key::from("k1".to_owned());
        let cases = [
            (
                ActionError::Agent(AgentError::NoSuchInvite(1)),
                Status::NoSuchInvite,
            ),
            (ActionError::NoSuchRoom(key()), Status::NoSuchRoom),
            (ActionError::NotDirect, Status::NoSuchRoom),
            (ActionError::NoSuchMessage(key()), Status::NoSuchMessage),
            (ActionError::NoSuchReaction(key()), Status::NoSuchMessage),
            (ActionError::NotEditable(key()), Status::NoSuchMessage),
            (ActionError::NotReferable(key()), Status::NoSuchMessage),
            (ActionError::NotReportable(key()), Status::NoSuchMessage),
            (ActionError::NoSuchBlob("b".into()), Status::NoSuchMessage),
            (ActionError::NoAdminRole, Status::NotPermitted),
            (ActionError::OwnRole, Status::NotPermitted),
            (ActionError::TooManyVersions, Status::TooLarge),
            (ActionError::NotOneRoom, Status::InvalidArgument),
            (ActionError::InvalidCall("x"), Status::InvalidArgument),
            (ActionError::NoBlobStore, Status::Io),
        ];
        for (error, expected) in cases {
            assert_eq!(of_action(&error).status, expected, "{error:?}");
        }
    }

    #[test]
    fn codes_are_stable() {
        assert_eq!(Status::Ok.code(), 0);
        assert_eq!(Status::Network.code(), 12);
        assert_eq!(Status::Internal.code(), 23);
        assert_eq!(Status::NotInitialized.code(), 24);
    }
}
