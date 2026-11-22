// SPDX-License-Identifier: GPL-3.0-or-later
//! purple-mimi's engine: mimid's agent (mimid-agent-core and
//! mimid-agent-store) inside a libpurple plugin, so that Pidgin and the
//! other libpurple programs are a MIMI device of the user
//! (docs/design/purple-mimi.md in mimid, decisions P1 to P7 and PQ1 to
//! PQ12).
//!
//! The C ABI is the crate `purple-mimi-ffi`; this crate has no `unsafe`.
//!
//! - [`Process`]: one Tokio runtime on its own threads, the event queue
//!   ([`events`]) with the descriptor that wakes the glue, the log as
//!   events ([`logging`]), and the accounts;
//! - [`account`]: what an account runs, from taking the state lock to its
//!   release;
//! - [`takeover`]: the handshake by which a second instance takes the
//!   account over and the first lets go (§6);
//! - [`handle`]: handles, user URIs, provider URLs, state directories;
//! - [`token`]: the provider token kept in the state directory (§12.3);
//! - [`status`]: the C ABI's status codes and the mapping of mimid's
//!   errors to them.
//!
//! # Limitations
//!
//! - Step 4 of the design's plan: accounts connect, disconnect, report
//!   their state and hand over. Rooms, messages and every user action are
//!   steps 5 and 6.
//! - A connection lost while connected is not reported (see [`account`]).
//! - Unix only: the event descriptor is a Unix socket pair and the state
//!   lock is mimid-agent-core's `flock`.

#![forbid(unsafe_code)]

pub mod account;
pub mod events;
pub mod handle;
pub mod logging;
pub mod process;
pub mod status;
pub mod takeover;
pub mod token;

pub use account::AccountConfig;
pub use events::{AccountId, Event, EventKind, LogLevel};
pub use mimid_agent_store::InvitePolicy;
pub use process::{AccountInfo, Process, ProcessOptions};
pub use status::{Failure, Status};
pub use takeover::Timings;
pub use token::Token;

/// This library's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The mimid revision this library is built against (decision PQ12): tag
/// purple-groundwork-1.
pub const MIMID_REVISION: &str = "279826c9581e2f7bc8aaa1bb92cb3a980fdffe14";
