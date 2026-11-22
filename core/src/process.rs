// SPDX-License-Identifier: GPL-3.0-or-later
//! The process-wide part of the library (docs/design/purple-mimi.md §5.4):
//! one Tokio runtime on its own threads, the event queue, and the
//! accounts.
//!
//! The glue's thread never waits on the runtime, except in
//! [`Process::shutdown`], which waits up to [`Timings::shutdown`] for the
//! accounts to release their state.
//!
//! # Limitations
//!
//! - One [`Process`] at a time owns the log forwarding
//!   ([`crate::logging`]).

use std::collections::{HashMap, HashSet};
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use mimid_agent_store::Store;
use tokio::runtime::Runtime;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::account::{self, AccountConfig, Context, Shared};
use crate::events::{AccountId, Event, EventKind, EventQueue};
use crate::status::{Failure, Status};
use crate::takeover::Timings;

/// The runtime's worker threads. INVENTED VALUE (§5.4).
pub const WORKER_THREADS: usize = 2;

/// What a [`Process`] starts with.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ProcessOptions {
    /// The handshake's and the release's timings.
    pub timings: Timings,
    /// The level from which log events are forwarded ([`crate::logging`]).
    pub log_level: i32,
}

impl Default for ProcessOptions {
    fn default() -> Self {
        Self {
            timings: Timings::default(),
            log_level: crate::events::LogLevel::Warn as i32,
        }
    }
}

impl ProcessOptions {
    /// With `timings`.
    #[must_use]
    pub fn with_timings(mut self, timings: Timings) -> Self {
        self.timings = timings;
        self
    }

    /// With log level `level`.
    #[must_use]
    pub fn with_log_level(mut self, level: i32) -> Self {
        self.log_level = level;
        self
    }
}

/// What the main thread can learn about an account without waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AccountInfo {
    /// The user URI.
    pub user_uri: String,
    /// The handle.
    pub handle: String,
    /// The state directory.
    pub state_dir: PathBuf,
    /// This device's key fingerprint, once it was open.
    pub fingerprint: Option<String>,
    /// Whether it is open now.
    pub connected: bool,
}

/// One room, as a list needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RoomSummary {
    /// Its store key.
    pub key: String,
    /// Its name, empty for an unnamed room.
    pub name: String,
    /// Its status, as the store names it.
    pub status: String,
    /// Whether it is a one-to-one room flagged `direct` locally.
    pub direct: bool,
}

/// One message as the glue shows it: the record's fields the glue needs,
/// plus the lines rendered in Rust (§5.6).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MessageView {
    /// Its store key.
    pub key: String,
    /// Its room's store key.
    pub room: String,
    /// Who sent it, as a user URI; empty for a notice from the agent.
    pub sender: String,
    /// The hub's acceptance time, in milliseconds.
    pub accepted_at: u64,
    /// The body as plain text, as stored.
    pub body: String,
    /// Its number in its room (§8.7), 0 when this front end has not
    /// numbered it yet.
    pub number: u64,
    /// The whole line libpurple 2 shows, as HTML (§5.6).
    pub line_html: String,
    /// Whether it was retracted or deleted by its sender.
    pub deleted: bool,
    /// Whether it has been edited.
    pub edited: bool,
    /// Whether it is one of the agent's own notices.
    pub notice: bool,
}

impl MessageView {
    /// `message` as the glue shows it, with its line rendered.
    fn of(message: &mimid_agent_store::record::Message, state: &mimid_agent_store::State) -> Self {
        let reported: crate::purple::Reported = message
            .ext
            .get(crate::purple::EXT_KEY)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default();
        // §8.7: a reply names its target's number, which only the target's
        // own record knows.
        let reply_to = match &message.in_reply_to {
            Some(mimid_agent_store::record::Reply::Message(target)) => state
                .messages
                .get(target)
                .and_then(|m| m.ext.get(crate::purple::EXT_KEY))
                .and_then(|value| {
                    serde_json::from_value::<crate::purple::Reported>(value.clone()).ok()
                })
                .map(|r| r.number)
                .filter(|number| *number != 0),
            _ => None,
        };
        Self {
            key: message.key.clone(),
            room: message.room.clone(),
            sender: match &message.sender {
                mimid_agent_store::record::Sender::User(uri) => uri.clone(),
                _ => String::new(),
            },
            accepted_at: message.accepted_at,
            body: message.body.clone(),
            number: reported.number,
            line_html: crate::purple::render_line(message, true, reported.number, reply_to),
            deleted: message.deleted.is_some(),
            edited: message.edited_at.is_some(),
            notice: message.notice.is_some(),
        }
    }
}

struct Entry {
    shared: Arc<Shared>,
    close: watch::Sender<bool>,
    task: JoinHandle<()>,
    user_uri: String,
    handle: String,
    state_dir: PathBuf,
}

/// The library's state in one process.
pub struct Process {
    runtime: Option<Runtime>,
    queue: Arc<EventQueue>,
    accounts: Mutex<HashMap<AccountId, Entry>>,
    /// Accounts closed by the glue, still releasing.
    closing: Mutex<Vec<JoinHandle<()>>>,
    taken_over: Arc<Mutex<HashSet<PathBuf>>>,
    kept: Arc<Mutex<Vec<Arc<Store>>>>,
    next_id: AtomicU64,
    timings: Timings,
    #[cfg(feature = "test-hooks")]
    panic_next: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for Process {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Process")
            .field("queue", &self.queue)
            .finish_non_exhaustive()
    }
}

pub(crate) fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Process {
    /// Starts the runtime and the event queue.
    ///
    /// # Errors
    ///
    /// [`Status::Internal`] if the runtime or the socket pair cannot be
    /// made.
    pub fn start(options: ProcessOptions) -> Result<Self, Failure> {
        let queue = Arc::new(EventQueue::new().map_err(|e| {
            Failure::fatal(Status::Internal, format!("the event socket pair: {e}"))
        })?);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(WORKER_THREADS)
            .thread_name("mimi-rt")
            .enable_all()
            .build()
            .map_err(|e| Failure::fatal(Status::Internal, format!("the runtime: {e}")))?;
        crate::logging::set_level(options.log_level);
        crate::logging::attach(&queue);
        Ok(Self {
            runtime: Some(runtime),
            queue,
            accounts: Mutex::default(),
            closing: Mutex::default(),
            taken_over: Arc::default(),
            kept: Arc::default(),
            next_id: AtomicU64::new(1),
            timings: options.timings,
            #[cfg(feature = "test-hooks")]
            panic_next: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn runtime(&self) -> &Runtime {
        self.runtime
            .as_ref()
            .expect("the runtime lives until shutdown, which consumes the process")
    }

    /// The descriptor the glue watches for reading.
    #[must_use]
    pub fn event_fd(&self) -> RawFd {
        self.queue.fd()
    }

    /// The next event, or `None`.
    #[must_use]
    pub fn next_event(&self) -> Option<Event> {
        self.queue.pop()
    }

    /// Makes the next opened account's task panic as it starts.
    #[cfg(feature = "test-hooks")]
    pub fn test_panic_next_open(&self) {
        self.panic_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Checks `config` and starts opening the account in the background;
    /// the result is [`EventKind::Connected`] or
    /// [`EventKind::Disconnected`].
    ///
    /// # Errors
    ///
    /// [`Status::InvalidArgument`] if the configuration is refused; nothing
    /// was started.
    pub fn open_account(&self, config: AccountConfig) -> Result<AccountId, Failure> {
        let checked = account::check(config)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(Shared::default());
        let (close, close_receiver) = watch::channel(false);
        let ctx = Context {
            id,
            checked: checked.clone(),
            queue: Arc::clone(&self.queue),
            shared: Arc::clone(&shared),
            timings: self.timings,
            taken_over: Arc::clone(&self.taken_over),
            kept: Arc::clone(&self.kept),
            #[cfg(feature = "test-hooks")]
            panic: self
                .panic_next
                .swap(false, std::sync::atomic::Ordering::SeqCst),
        };
        let queue = Arc::clone(&self.queue);
        let watched = Arc::clone(&shared);
        let runtime = self.runtime();
        // §5.5: a panic in the account's task is caught at its JoinHandle
        // and poisons the account.
        let task = runtime.spawn(async move {
            let inner = tokio::spawn(account::run(ctx, close_receiver));
            if let Err(error) = inner.await
                && error.is_panic()
            {
                watched.poisoned.store(true, Ordering::SeqCst);
                watched.connected.store(false, Ordering::SeqCst);
                tracing::error!(name: "purple_account_panicked", "the account's task panicked");
                queue.push(Event {
                    account: Some(id),
                    kind: EventKind::Disconnected {
                        status: Status::Panic,
                        temporary: false,
                        text: "purple-mimi failed inside (a panic); the account was closed."
                            .to_owned(),
                    },
                });
            }
        });
        locked(&self.accounts).insert(
            id,
            Entry {
                shared,
                close,
                task,
                user_uri: checked.user_uri,
                handle: checked.handle,
                state_dir: checked.state_dir,
            },
        );
        Ok(id)
    }

    /// Starts the release of account `id`; no event names it from now
    /// on. Does nothing for an unknown id.
    pub fn close_account(&self, id: AccountId) {
        self.queue.silence(id);
        if let Some(entry) = locked(&self.accounts).remove(&id) {
            let _ = entry.close.send(true);
            locked(&self.closing).push(entry.task);
        }
    }

    /// Marks account `id` poisoned after a panic at the C ABI (§5.5): it is
    /// closed, and the glue gets [`EventKind::Disconnected`] with
    /// [`Status::Panic`].
    pub fn poison(&self, id: AccountId) {
        let accounts = locked(&self.accounts);
        let Some(entry) = accounts.get(&id) else {
            return;
        };
        if entry.shared.poisoned.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = entry.close.send(true);
        drop(accounts);
        self.queue.push(Event {
            account: Some(id),
            kind: EventKind::Disconnected {
                status: Status::Panic,
                temporary: false,
                text: "purple-mimi failed inside (a panic); the account was closed.".to_owned(),
            },
        });
    }

    /// Whether account `id` is known and not poisoned.
    ///
    /// # Errors
    ///
    /// [`Status::InvalidArgument`] for an unknown id, [`Status::Poisoned`]
    /// for a poisoned account.
    pub fn check_account(&self, id: AccountId) -> Result<(), Failure> {
        let accounts = locked(&self.accounts);
        let entry = accounts
            .get(&id)
            .ok_or_else(|| Failure::fatal(Status::InvalidArgument, "no such account"))?;
        if entry.shared.poisoned.load(Ordering::SeqCst) {
            return Err(Failure::fatal(
                Status::Poisoned,
                "the account failed inside (a panic) and was closed",
            ));
        }
        Ok(())
    }

    /// What is known about account `id`.
    ///
    /// # Errors
    ///
    /// As [`Process::check_account`].
    pub fn account_info(&self, id: AccountId) -> Result<AccountInfo, Failure> {
        self.check_account(id)?;
        let accounts = locked(&self.accounts);
        let entry = accounts
            .get(&id)
            .ok_or_else(|| Failure::fatal(Status::InvalidArgument, "no such account"))?;
        Ok(AccountInfo {
            user_uri: entry.user_uri.clone(),
            handle: entry.handle.clone(),
            state_dir: entry.state_dir.clone(),
            fingerprint: locked(&entry.shared.fingerprint).clone(),
            connected: entry.shared.connected.load(Ordering::SeqCst),
        })
    }

    /// Runs `read` against `id`'s store, which is open only while the
    /// account is (§5.5: the reads are synchronous, straight from the
    /// store's records).
    ///
    /// # Errors
    ///
    /// [`Status::NotConnected`] when the account is not open, including
    /// while it is releasing, and [`Status::Poisoned`] after a panic.
    pub fn with_store<T>(
        &self,
        id: AccountId,
        read: impl FnOnce(&Store) -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        self.check_account(id)?;
        // The clone is held only for this call, so it cannot delay the
        // release's ownership check (§6.3 item 10) beyond one read.
        let store = {
            let accounts = locked(&self.accounts);
            let entry = accounts
                .get(&id)
                .ok_or_else(|| Failure::fatal(Status::InvalidArgument, "no such account"))?;
            locked(&entry.shared.store).clone()
        };
        let store = store
            .ok_or_else(|| Failure::temporary(Status::NotConnected, "The account is not open."))?;
        read(&store)
    }

    /// Every room this account has not forgotten, oldest first by key.
    ///
    /// # Errors
    ///
    /// As [`Process::with_store`].
    pub fn rooms(&self, id: AccountId) -> Result<Vec<RoomSummary>, Failure> {
        self.with_store(id, |store| {
            let state = store.state();
            Ok(state
                .rooms
                .iter()
                .map(|(key, room)| RoomSummary {
                    key: key.clone(),
                    name: room.name.clone().unwrap_or_default(),
                    status: format!("{:?}", room.status),
                    direct: room.direct,
                })
                .collect())
        })
    }

    /// One message as the store has it, with the lines libpurple 2 shows
    /// (§5.6).
    ///
    /// # Errors
    ///
    /// As [`Process::with_store`], and [`Status::NoSuchMessage`].
    pub fn message(&self, id: AccountId, key: &str) -> Result<MessageView, Failure> {
        self.with_store(id, |store| {
            let state = store.state();
            let message = state
                .messages
                .get(key)
                .ok_or_else(|| Failure::fatal(Status::NoSuchMessage, "no such message"))?;
            Ok(MessageView::of(message, &state))
        })
    }

    /// A page of `room`'s history, oldest first, ending before
    /// `before_key` when it is given.
    ///
    /// # Errors
    ///
    /// As [`Process::with_store`].
    pub fn history(
        &self,
        id: AccountId,
        room: &str,
        before_key: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MessageView>, Failure> {
        self.with_store(id, |store| {
            let state = store.state();
            // Ordered as the store orders history: (accepted_at, seq).
            let mut all: Vec<&mimid_agent_store::record::Message> =
                state.messages.values().filter(|m| m.room == room).collect();
            all.sort_by_key(|m| (m.accepted_at, m.seq));
            if let Some(before) = before_key
                && let Some(at) = all.iter().position(|m| m.key == before)
            {
                all.truncate(at);
            }
            let start = all.len().saturating_sub(limit);
            Ok(all[start..]
                .iter()
                .map(|m| MessageView::of(m, &state))
                .collect())
        })
    }

    /// Closes every account by the release path, waits up to
    /// [`Timings::shutdown`] for them, then stops the runtime (§5.4).
    pub fn shutdown(mut self) {
        let ids: Vec<AccountId> = locked(&self.accounts).keys().copied().collect();
        for id in ids {
            self.close_account(id);
        }
        let tasks: Vec<JoinHandle<()>> = std::mem::take(&mut *locked(&self.closing));
        let wait = self.timings.shutdown;
        if let Some(runtime) = self.runtime.take() {
            let all = async {
                for task in tasks {
                    let _ = task.await;
                }
            };
            // The timeout must be built inside `block_on`: a `Timeout`
            // needs the runtime's timer, and as an argument it would be
            // constructed before the runtime is entered, which panics.
            if runtime
                .block_on(async { tokio::time::timeout(wait, all).await })
                .is_err()
            {
                tracing::warn!(
                    name: "purple_shutdown_timed_out",
                    "accounts were still releasing at shutdown"
                );
            }
            locked(&self.kept).clear();
            runtime.shutdown_timeout(std::time::Duration::from_secs(1));
        }
        crate::logging::detach();
    }

    /// Runs `future` on the runtime and waits for it, for tests.
    #[doc(hidden)]
    pub fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.runtime().block_on(future)
    }
}
