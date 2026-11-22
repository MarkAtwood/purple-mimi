// SPDX-License-Identifier: GPL-3.0-or-later
//! The one process-wide queue of events for the libpurple glue, and the
//! descriptor that wakes it (docs/design/purple-mimi.md §5.4).
//!
//! The runtime pushes events from any thread; the glue takes them on its
//! main thread. When the queue goes from empty to not empty, one octet is
//! written to a socket pair whose other end the glue watches for reading
//! (`purple_input_add`, `g_unix_fd_add`). Taking the last event drains it.
//! The design note names a `pipe(2)`; a socket pair from std does the same
//! without `unsafe`, and is what Windows (§13 step 9) needs anyway.
//!
//! # Limitations
//!
//! - Unbounded: a glue that never drains the queue grows it without
//!   limit. The glue drains it on every wake-up.

use std::collections::{HashSet, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, PoisonError};

use crate::status::Status;

/// An account of the process, as the core names it.
pub type AccountId = u64;

/// Log levels, as the C ABI numbers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(i32)]
pub enum LogLevel {
    /// Errors.
    Error = 1,
    /// Warnings.
    Warn = 2,
    /// Information.
    Info = 3,
    /// Debugging.
    Debug = 4,
    /// Everything.
    Trace = 5,
}

/// What happened.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventKind {
    /// A step of connecting (docs/design/purple-mimi.md §7.3).
    Progress {
        /// This step, from 1.
        step: u32,
        /// How many steps there are.
        steps: u32,
        /// What the step does.
        text: String,
    },
    /// The account is open.
    Connected,
    /// The account is closed, by a failure or a takeover. Not sent after
    /// the glue closed the account itself.
    Disconnected {
        /// Why.
        status: Status,
        /// Whether connecting again later may help.
        temporary: bool,
        /// Why, for the user.
        text: String,
    },
    /// A log line of the library (§5.4).
    Log {
        /// Its level.
        level: LogLevel,
        /// The line.
        text: String,
    },
    /// A room was added, changed, left or forgotten (§5.5, §5.6).
    Room {
        /// What happened to it.
        change: RoomChange,
        /// Its store key.
        room: String,
    },
    /// A message arrived or moved (§5.6). One store change can mean
    /// several of these.
    Message {
        /// What happened to it.
        change: MessageChange,
        /// Its store key.
        message: String,
        /// Its room's store key; empty once the record is gone, when the
        /// glue finds the line by the message key alone.
        room: String,
    },
    /// A reaction was added or taken back (§5.6, §8.8).
    Reaction {
        /// Which way.
        change: ReactionChange,
        /// The message reacted to, by store key.
        message: String,
        /// The reaction's key among that message's reactions.
        reaction_key: String,
        /// Who reacted, as a user URI; empty for a removal.
        sender: String,
        /// The reaction itself, typically an emoji; empty for a removal.
        reaction: String,
    },
    /// A person was first seen or changed (§5.5).
    Person {
        /// Whether this is the first time.
        added: bool,
        /// The user URI.
        user: String,
    },
}

/// What happened to a room ([`EventKind::Room`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomChange {
    /// Joined, hosted or found.
    Added,
    /// Its participants, roles, name or subject changed, or may have.
    Changed,
    /// This user is out of it.
    Left,
    /// It and everything the store kept of it are gone.
    Forgotten,
}

/// What happened to a message ([`EventKind::Message`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageChange {
    /// Stored for the first time.
    New,
    /// A new revision: the body changed.
    Edited,
    /// Retracted by its sender; the record stays, saying so.
    Retracted,
    /// Deleted here, in every front end; the record is gone.
    Deleted,
    /// Deleted at its expiry; the record is gone.
    Expired,
    /// An attachment's content arrived and is now in the record.
    AttachmentReady,
}

/// Which way a reaction went ([`EventKind::Reaction`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionChange {
    /// Someone reacted.
    Added,
    /// Someone took their reaction back.
    Removed,
}

/// One event: what happened, and to which account (`None` for the
/// process).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// The account, or `None`.
    pub account: Option<AccountId>,
    /// What happened.
    pub kind: EventKind,
}

#[derive(Default)]
struct Inner {
    queue: VecDeque<Event>,
    /// Accounts the glue closed: their events are dropped.
    silenced: HashSet<AccountId>,
}

/// The queue and its wake-up descriptor.
pub struct EventQueue {
    inner: Mutex<Inner>,
    reader: UnixStream,
    writer: UnixStream,
}

impl std::fmt::Debug for EventQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventQueue")
            .field("fd", &self.reader.as_raw_fd())
            .finish_non_exhaustive()
    }
}

impl EventQueue {
    /// An empty queue.
    ///
    /// # Errors
    ///
    /// If the socket pair cannot be made.
    pub fn new() -> std::io::Result<Self> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        Ok(Self {
            inner: Mutex::default(),
            reader,
            writer,
        })
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The descriptor that becomes readable when events are waiting.
    #[must_use]
    pub fn fd(&self) -> RawFd {
        self.reader.as_raw_fd()
    }

    /// Queues `event`, unless its account was silenced, and wakes the
    /// glue if the queue was empty. Safe from any thread.
    pub fn push(&self, event: Event) {
        let mut inner = self.inner();
        if event
            .account
            .is_some_and(|account| inner.silenced.contains(&account))
        {
            return;
        }
        let was_empty = inner.queue.is_empty();
        inner.queue.push_back(event);
        if was_empty {
            // Under the lock, so a pop that empties the queue and drains
            // the socket cannot come between the push and the octet. A
            // full socket already wakes the glue. A failure is not logged:
            // the log goes through this queue.
            let _ = (&self.writer).write(&[1]);
        }
    }

    /// The next event, or `None`. Taking the last one drains the
    /// descriptor.
    pub fn pop(&self) -> Option<Event> {
        let mut inner = self.inner();
        let event = inner.queue.pop_front();
        if inner.queue.is_empty() {
            self.drain();
        }
        event
    }

    fn drain(&self) {
        let mut buffer = [0u8; 64];
        loop {
            match (&self.reader).read(&mut buffer) {
                Ok(0) => return,
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }

    /// Drops `account`'s queued events and every later one.
    pub fn silence(&self, account: AccountId) {
        let mut inner = self.inner();
        inner.silenced.insert(account);
        inner.queue.retain(|e| e.account != Some(account));
        if inner.queue.is_empty() {
            self.drain();
        }
    }

    /// How many events are waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner().queue.len()
    }

    /// Whether no event is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readable(queue: &EventQueue) -> bool {
        // Reads an octet and puts one back: what is readable is all that
        // matters, not how many octets.
        let mut octet = [0u8; 1];
        match (&queue.reader).read(&mut octet) {
            Ok(n) => {
                (&queue.writer).write_all(&[1]).unwrap();
                n > 0
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => false,
            Err(e) => panic!("{e}"),
        }
    }

    fn log(account: Option<AccountId>, text: &str) -> Event {
        Event {
            account,
            kind: EventKind::Log {
                level: LogLevel::Info,
                text: text.to_owned(),
            },
        }
    }

    #[test]
    fn the_descriptor_is_readable_exactly_while_events_wait() {
        let queue = EventQueue::new().unwrap();
        assert!(!readable(&queue));
        queue.push(log(None, "a"));
        queue.push(log(None, "b"));
        assert!(readable(&queue));
        assert_eq!(queue.pop(), Some(log(None, "a")));
        assert!(readable(&queue), "one is still waiting");
        assert_eq!(queue.pop(), Some(log(None, "b")));
        assert!(!readable(&queue), "drained with the last event");
        assert_eq!(queue.pop(), None);
        queue.push(log(None, "c"));
        assert!(readable(&queue), "woken again");
    }

    #[test]
    fn a_silenced_account_gets_no_more_events() {
        let queue = EventQueue::new().unwrap();
        queue.push(log(Some(1), "one"));
        queue.push(log(Some(2), "two"));
        queue.silence(1);
        queue.push(log(Some(1), "later"));
        assert_eq!(queue.pop(), Some(log(Some(2), "two")));
        assert_eq!(queue.pop(), None);
        assert!(!readable(&queue));
    }

    #[test]
    fn pushes_from_other_threads_all_arrive() {
        let queue = std::sync::Arc::new(EventQueue::new().unwrap());
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let queue = std::sync::Arc::clone(&queue);
                std::thread::spawn(move || {
                    for i in 0..1000 {
                        queue.push(log(None, &format!("{t}/{i}")));
                    }
                })
            })
            .collect();
        let mut taken = 0;
        while taken < 4000 {
            if queue.pop().is_some() {
                taken += 1;
            } else {
                std::thread::yield_now();
            }
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(queue.pop(), None);
        assert!(!readable(&queue));
    }
}
