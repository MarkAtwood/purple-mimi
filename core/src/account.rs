// SPDX-License-Identifier: GPL-3.0-or-later
//! One MIMI account in the process: what it runs on the runtime from
//! `open` to its release (docs/design/purple-mimi.md §5.7, §6, §7).
//!
//! 1. Refuse at once if this process saw the account taken over and the
//!    open does not ask to take it back (§6.6; decision PQ9).
//! 2. Take the state lock, with the takeover handshake while another
//!    instance holds it ([`crate::takeover`]); `Agent::open` takes the lock
//!    and then connects over the live stream (§7), checking the user.
//! 3. Top the KeyPackages up; the first publication makes this device's
//!    Client record.
//! 4. Build the store over the agent, with the blob store, and read its
//!    records from the database.
//! 5. Keep the token in the state directory (§12.3), report
//!    [`EventKind::Connected`], and start mimid's runner (sync, expiry,
//!    KeyPackage top-up, stream wake-ups).
//! 6. Watch: every [`Timings::watch`], whether another instance asks for
//!    the account, whether the lock file is still ours (§6.5), and
//!    whether the provider has stopped answering ([`WatchConnection`]).
//! 7. Release (§6.3 items 7 to 13): stop the runner, wait up to
//!    [`Timings::grace`] and abort what is still running, save and close
//!    the store and the agent, make sure nothing else holds them, sync
//!    the directories, drop the lock.
//!
//! # Limitations
//!
//! - A lost connection is noticed a sync at a time, not at once: the
//!   account disconnects after [`NETWORK_FAILURES_BEFORE_DISCONNECT`]
//!   syncs in a row could not reach the provider, which with the default
//!   interval is tens of seconds. The stream's own loss wakes the sync
//!   loop, so the first of those failures comes quickly.
//! - No user action exists yet (step 5), so item 7's "stop taking new
//!   work" has nothing to stop.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mimid_agent_core::{Agent, AgentConfig, AgentError, Binding};
use mimid_agent_store::blobs::BlobStore;
use mimid_agent_store::runner::{Runner, RunnerHooks, RunnerOptions, top_up_key_packages};
use mimid_agent_store::{InvitePolicy, Store, SyncError, SyncReport};
use tokio::sync::watch;

use crate::events::{AccountId, Event, EventKind, EventQueue};
use crate::process::locked;
use crate::status::{self, Failure, Status};
use crate::takeover::{self, Attempt, Refusal, Timings};
use crate::token::{self, Token};

/// The blob store's directory in the state directory, as mimid-agent's.
pub const BLOBS_DIR: &str = "blobs";

/// How many steps of connecting are reported (§7.3).
const STEPS: u32 = 3;

/// What an account is opened with.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AccountConfig {
    /// libpurple's base directory (`purple_user_dir()`,
    /// `purple_data_dir()`); the state is in `<base>/mimi/<handle>`.
    pub base_dir: PathBuf,
    /// The user, as a handle or a user URI.
    pub user: String,
    /// The provider's URL, or `None` for `https://<domain>`.
    pub provider_url: Option<String>,
    /// The provider's bearer token, or `None` to use the stored one.
    pub token: Option<Token>,
    /// Which invites to join.
    pub invites: InvitePolicy,
    /// How many KeyPackages to keep published.
    pub key_packages: u32,
    /// The fallback sync interval.
    pub sync_interval: Duration,
    /// Whether to take the account back after this process saw it taken
    /// over (§6.6).
    pub take_over: bool,
}

impl AccountConfig {
    /// A configuration for `user` under `base_dir` with the daemon's
    /// defaults: the stored token, invites from allowed users, 10
    /// KeyPackages, a sync every 30 s.
    #[must_use]
    pub fn new(base_dir: impl Into<PathBuf>, user: impl Into<String>) -> Self {
        Self {
            base_dir: base_dir.into(),
            user: user.into(),
            provider_url: None,
            token: None,
            invites: InvitePolicy::FromAllowed,
            key_packages: 10,
            sync_interval: Duration::from_secs(30),
            take_over: false,
        }
    }

    /// With provider URL `url`.
    #[must_use]
    pub fn with_provider_url(mut self, url: Option<String>) -> Self {
        self.provider_url = url;
        self
    }

    /// With token `token`.
    #[must_use]
    pub fn with_token(mut self, token: Option<Token>) -> Self {
        self.token = token;
        self
    }

    /// With invite policy `invites`.
    #[must_use]
    pub fn with_invites(mut self, invites: InvitePolicy) -> Self {
        self.invites = invites;
        self
    }

    /// Keeping `count` KeyPackages published.
    #[must_use]
    pub fn with_key_packages(mut self, count: u32) -> Self {
        self.key_packages = count;
        self
    }

    /// Syncing at least every `interval`.
    #[must_use]
    pub fn with_sync_interval(mut self, interval: Duration) -> Self {
        self.sync_interval = interval;
        self
    }

    /// Taking the account back if this process saw it taken over.
    #[must_use]
    pub fn with_take_over(mut self, take_over: bool) -> Self {
        self.take_over = take_over;
        self
    }
}

/// A checked configuration.
#[derive(Debug, Clone)]
pub(crate) struct Checked {
    pub(crate) user_uri: String,
    pub(crate) handle: String,
    pub(crate) state_dir: PathBuf,
    pub(crate) provider_url: String,
    pub(crate) config: AccountConfig,
}

/// Checks `config` without touching the disk or the network.
///
/// # Errors
///
/// [`Status::InvalidArgument`] with a text for the user.
pub(crate) fn check(config: AccountConfig) -> Result<Checked, Failure> {
    let invalid = |text: String| Failure::fatal(Status::InvalidArgument, text);
    let user_uri = crate::handle::user_uri_of(&config.user).ok_or_else(|| {
        invalid(format!(
            "{} is not a MIMI user: give a handle such as alice@a.example, or a \
             mimi://<domain>/u/<id> URI",
            config.user
        ))
    })?;
    let handle = crate::handle::handle_of(&user_uri)
        .ok_or_else(|| invalid(format!("{user_uri} has no handle")))?;
    if !config.base_dir.is_absolute() {
        return Err(invalid(format!(
            "{} is not an absolute directory",
            config.base_dir.display()
        )));
    }
    let state_dir = crate::handle::state_dir_for(&config.base_dir, &user_uri)
        .ok_or_else(|| invalid(format!("{user_uri} has no state directory")))?;
    let provider_url = match config.provider_url.as_deref().map(str::trim) {
        Some(url) if !url.is_empty() => url.trim_end_matches('/').to_owned(),
        _ => crate::handle::default_provider_url(&user_uri)
            .ok_or_else(|| invalid(format!("{user_uri} has no domain")))?,
    };
    crate::handle::check_provider_url(&provider_url).map_err(invalid)?;
    if config.key_packages > 64 {
        return Err(invalid("at most 64 KeyPackages".to_owned()));
    }
    if config.sync_interval < Duration::from_millis(50) {
        return Err(invalid("a sync interval of at least 50 ms".to_owned()));
    }
    Ok(Checked {
        user_uri,
        handle,
        state_dir,
        provider_url,
        config,
    })
}

/// What an account shares with the main thread.
#[derive(Debug, Default)]
pub struct Shared {
    /// Set once the account is open; cleared on release.
    pub connected: AtomicBool,
    /// Set after a panic (§5.5).
    pub poisoned: AtomicBool,
    /// This device's key fingerprint once open: the first 8 octets of its
    /// signature key in hex (§8.12, INVENTED form).
    pub fingerprint: Mutex<Option<String>>,
    /// The account's store while it is open, so the reads of §5.5 can be
    /// synchronous.
    ///
    /// The release path clears this BEFORE it checks that nothing else
    /// holds the store (§6.3 item 10): an Arc left here would fail that
    /// check and keep the state lock for the life of the process.
    pub store: Mutex<Option<Arc<Store>>>,
}

/// What the account task has from the process.
pub(crate) struct Context {
    pub(crate) id: AccountId,
    pub(crate) checked: Checked,
    pub(crate) queue: Arc<EventQueue>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) timings: Timings,
    /// State directories this process saw taken over (§6.6).
    pub(crate) taken_over: Arc<Mutex<HashSet<PathBuf>>>,
    /// Stores that could not be released, kept with their lock until the
    /// process ends.
    pub(crate) kept: Arc<Mutex<Vec<Arc<Store>>>>,
    #[cfg(feature = "test-hooks")]
    pub(crate) panic: bool,
}

/// Why the account stopped serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    /// The glue closed it, or the process shuts down.
    Closed,
    /// Another instance asked for it.
    TakenOver,
    /// The lock file is not ours any more.
    LockLost,
    /// The agent could not reach its provider for
    /// [`NETWORK_FAILURES_BEFORE_DISCONNECT`] syncs in a row (§7.4).
    NetworkLost,
}

/// How many syncs in a row must fail to reach the provider before the
/// account is reported disconnected with [`Status::Network`] (§7.4).
/// One failure is not a lost connection: agent-core reopens the stream on
/// the next call and the sync loop makes that call at once, so a blip
/// costs one pass. INVENTED VALUE, NOT FROM SPEC.
pub const NETWORK_FAILURES_BEFORE_DISCONNECT: u32 = 3;

/// The [`mimid_agent_store::runner::AfterSync`] hook of §7.4: it counts
/// syncs that could not reach the provider and, at
/// [`NETWORK_FAILURES_BEFORE_DISCONNECT`] in a row, tells the account to
/// disconnect. A sync that failed to save is a local problem and is not
/// counted; any success resets the count.
struct WatchConnection {
    lost: Arc<AtomicBool>,
    failures: u32,
}

impl mimid_agent_store::runner::AfterSync for WatchConnection {
    fn after_sync<'a>(
        &'a mut self,
        _store: &'a Store,
        outcome: Result<&'a SyncReport, &'a SyncError>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        match outcome {
            Ok(_) => self.failures = 0,
            Err(SyncError::Agent(_)) => {
                self.failures = self.failures.saturating_add(1);
                if self.failures >= NETWORK_FAILURES_BEFORE_DISCONNECT {
                    self.lost.store(true, Ordering::SeqCst);
                }
            }
            // A save that failed is this device's problem, not the
            // provider's, and says nothing about the connection.
            Err(_) => {}
        }
        Box::pin(std::future::ready(()))
    }
}

impl Context {
    fn push(&self, kind: EventKind) {
        self.queue.push(Event {
            account: Some(self.id),
            kind,
        });
    }

    fn progress(&self, step: u32, text: &str) {
        self.push(EventKind::Progress {
            step,
            steps: STEPS,
            text: text.to_owned(),
        });
    }

    fn state_dir(&self) -> &Path {
        &self.checked.state_dir
    }

    fn taken_over(&self) -> std::sync::MutexGuard<'_, HashSet<PathBuf>> {
        self.taken_over
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Runs the account until `close` turns `true` or it fails; reports the
/// end as [`EventKind::Disconnected`] unless the glue closed it.
pub(crate) async fn run(ctx: Context, close: watch::Receiver<bool>) {
    let outcome = serve(&ctx, close).await;
    ctx.shared.connected.store(false, Ordering::SeqCst);
    if let Err(failure) = outcome {
        tracing::info!(
            name: "purple_account_disconnected",
            status = ?failure.status,
            "the account is disconnected"
        );
        ctx.push(EventKind::Disconnected {
            status: failure.status,
            temporary: failure.temporary,
            text: failure.text,
        });
    }
}

/// Resolves when `close` turns `true` or its sender is gone.
async fn closed(close: &mut watch::Receiver<bool>) {
    let _ = close.wait_for(|c| *c).await;
}

fn private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// `Ok(())` after a close, `Err` after anything else.
async fn serve(ctx: &Context, mut close: watch::Receiver<bool>) -> Result<(), Failure> {
    #[cfg(feature = "test-hooks")]
    if ctx.panic {
        panic!("test hook: a panic in the account task");
    }
    let state_dir = ctx.state_dir().to_owned();
    // §6.6: not taking it back by itself.
    if ctx.checked.config.take_over {
        ctx.taken_over().remove(&state_dir);
    } else if ctx.taken_over().contains(&state_dir) {
        return Err(Failure::fatal(
            Status::TakenOver,
            "This account was taken over by another program using the same profile; it is \
             not taken back automatically. Use \"Use this account here\" to take it back.",
        ));
    }
    let parent = state_dir.parent().unwrap_or(&state_dir).to_owned();
    private_dir(&parent)
        .map_err(|e| Failure::fatal(Status::State, format!("{}: {e}", parent.display())))?;
    let token = match &ctx.checked.config.token {
        Some(token) => token.clone(),
        None => token::read_stored(&state_dir)
            .map_err(|e| {
                Failure::fatal(
                    Status::Io,
                    format!("{}: {e}", state_dir.join(token::TOKEN_FILE).display()),
                )
            })?
            .ok_or_else(|| {
                Failure::fatal(
                    Status::Unauthorized,
                    "No provider token is stored for this account. Enter the token from your \
                     provider (mimid admin ... token) as the password.",
                )
            })?,
    };
    let nonce = takeover::new_nonce()
        .map_err(|e| Failure::fatal(Status::Internal, format!("no random numbers: {e}")))?;

    ctx.progress(1, "Locking the state and connecting to the provider");
    let agent_config = AgentConfig {
        server_url: ctx.checked.provider_url.clone(),
        token: token.expose().to_owned(),
        user_uri: ctx.checked.user_uri.clone(),
        state_dir: state_dir.clone(),
        full_tree_hubs: Vec::new(),
        // BasicCredential, mimid's default (decision PQ10).
        did_webvh: mimid_agent_core::DidWebvhConfig::default(),
        binding: Binding::Stream,
    };
    let acquiring = takeover::acquire(
        &state_dir,
        &nonce,
        &ctx.timings,
        || {
            let config = agent_config.clone();
            async move {
                match Agent::open(config).await {
                    Ok(agent) => Attempt::Got(agent),
                    Err(AgentError::Locked { .. }) => Attempt::Locked,
                    Err(e) => Attempt::Failed(e),
                }
            }
        },
        || ctx.progress(1, "Waiting for another instance to release this account"),
    );
    let mut agent = tokio::select! {
        got = acquiring => match got {
            Ok(agent) => agent,
            Err(Refusal::Failed(e)) => return Err(status::of_agent(&e)),
            Err(Refusal::TimedOut) => return Err(Failure::fatal(
                Status::Locked,
                "This account is open in another program that did not release it. Close it \
                 there, or stop that program.",
            )),
            Err(Refusal::Io(text)) => return Err(Failure::fatal(Status::Io, text)),
        },
        () = closed(&mut close) => return Ok(()),
    };
    drop(agent_config);
    {
        let key = agent.signature_public_key();
        let fingerprint: String = key
            .iter()
            .take(8)
            .map(|octet| format!("{octet:02x}"))
            .collect();
        *ctx.shared
            .fingerprint
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(fingerprint);
    }

    ctx.progress(2, "Publishing keys");
    tokio::select! {
        () = top_up_key_packages(&mut agent, ctx.checked.config.key_packages) => {}
        () = closed(&mut close) => {
            agent.close().await;
            return Ok(());
        }
    }

    ctx.progress(3, "Opening the store");
    let stream_wake = agent.stream_wake();
    let store = Arc::new(open_store(agent, &state_dir, ctx.checked.config.invites.clone()).await?);

    // §12.3: keep the token beside the keys, so the glue can clear
    // libpurple's copy once connected.
    let stored = token::read_stored(&state_dir).ok().flatten();
    if stored.as_ref() != Some(&token)
        && let Err(e) = token::store(&state_dir, &token)
    {
        let failure = Failure::fatal(
            Status::Io,
            format!(
                "The token could not be kept in {}: {e}",
                state_dir.join(token::TOKEN_FILE).display()
            ),
        );
        let _ = release(ctx, store, None, Reason::Closed).await;
        return Err(failure);
    }

    let (stop, stop_receiver) = watch::channel(false);
    let network_lost = Arc::new(AtomicBool::new(false));
    let runner = Runner::start(
        &store,
        RunnerOptions::new(
            ctx.checked.config.sync_interval,
            ctx.checked.config.key_packages,
        )
        .with_stream_wake(stream_wake),
        &stop_receiver,
        RunnerHooks::new().after(Box::new(WatchConnection {
            lost: Arc::clone(&network_lost),
            failures: 0,
        })),
    );
    *locked(&ctx.shared.store) = Some(Arc::clone(&store));
    ctx.shared.connected.store(true, Ordering::SeqCst);
    tracing::info!(name: "purple_account_connected", "the account is connected");
    ctx.push(EventKind::Connected);

    let reason = watch_lock(ctx, &store, &nonce, &mut close, &network_lost).await;
    ctx.shared.connected.store(false, Ordering::SeqCst);
    let _ = stop.send(true);
    let released = release(ctx, store, Some(runner), reason).await;
    match (reason, released) {
        (_, Err(failure)) => Err(failure),
        (Reason::Closed, Ok(())) => Ok(()),
        (Reason::TakenOver, Ok(())) => {
            ctx.taken_over().insert(state_dir);
            Err(Failure::fatal(
                Status::TakenOver,
                "This account was opened by another program using the same profile.",
            ))
        }
        // Temporary, and the account is not remembered as taken over:
        // libpurple reconnects by its own rules (§7.4).
        (Reason::NetworkLost, Ok(())) => Err(Failure::temporary(
            Status::Network,
            "The connection to the provider was lost and could not be reopened.",
        )),
        (Reason::LockLost, Ok(())) => Err(Failure::fatal(
            Status::LockLost,
            format!(
                "The state lock of this account was removed while it was in use; the account \
                 was stopped. Check that no other program uses {}.",
                ctx.state_dir().display()
            ),
        )),
    }
}

/// The store over `agent` with the blob store and the records from the
/// database. On failure the agent's connection is closed first.
async fn open_store(
    agent: Agent,
    state_dir: &Path,
    invites: InvitePolicy,
) -> Result<Store, Failure> {
    let (store, failure) = match BlobStore::open(state_dir.join(BLOBS_DIR)) {
        Ok(blobs) => {
            let store = Store::new(agent, invites).with_blobs(blobs);
            match store.open_database() {
                Ok(()) => return Ok(store),
                Err(e) => (store, status::of_file(&e)),
            }
        }
        Err(e) => (
            Store::new(agent, invites),
            Failure::fatal(
                Status::State,
                format!("{}: {e}", state_dir.join(BLOBS_DIR).display()),
            ),
        ),
    };
    store.agent().lock().await.close().await;
    Err(failure)
}

/// Item 6 and §6.5: until closed, asked to let go, or the lock file is
/// not ours.
async fn watch_lock(
    ctx: &Context,
    store: &Store,
    nonce: &str,
    close: &mut watch::Receiver<bool>,
    network_lost: &AtomicBool,
) -> Reason {
    loop {
        if *close.borrow() {
            return Reason::Closed;
        }
        if network_lost.load(Ordering::SeqCst) {
            tracing::warn!(
                name: "purple_network_lost",
                failures = NETWORK_FAILURES_BEFORE_DISCONNECT,
                "the provider could not be reached by that many syncs in a row"
            );
            return Reason::NetworkLost;
        }
        if !store.state_lock().intact() {
            tracing::error!(
                name: "purple_lock_lost",
                "the state lock file was removed or replaced while in use"
            );
            return Reason::LockLost;
        }
        if takeover::asked_to_release(ctx.state_dir(), nonce, &ctx.timings) {
            tracing::info!(
                name: "purple_takeover_requested",
                "another instance asked for this account"
            );
            return Reason::TakenOver;
        }
        tokio::select! {
            () = closed(close) => return Reason::Closed,
            () = tokio::time::sleep(ctx.timings.watch) => {}
        }
    }
}

/// The release path (§6.3 items 8 to 12).
async fn release(
    ctx: &Context,
    store: Arc<Store>,
    runner: Option<Runner>,
    reason: Reason,
) -> Result<(), Failure> {
    // Before anything else: the reads of §5.5 hold a clone of the store
    // here, and item 10 below needs the last reference. Clearing it
    // first means a read that arrives during the release fails with
    // "not connected" rather than keeping the lock for ever.
    *locked(&ctx.shared.store) = None;
    // Items 8 and 9: wait the grace period out, then abort what is still
    // running. §6.5 gives a lost lock no grace of its own, but a loop in
    // the middle of a pass still gets one watch period to end cleanly.
    let grace = if reason == Reason::LockLost {
        ctx.timings.watch
    } else {
        ctx.timings.grace
    };
    if let Some(runner) = runner {
        let stopped = runner.join_within(grace).await;
        if !stopped.clean() {
            // Not a failure of the release: an aborted loop is cut at a
            // transaction boundary (§6.4) and has dropped its reference
            // to the store, which is what item 10 needs.
            tracing::warn!(
                name: "purple_loops_aborted",
                loops = ?stopped.aborted,
                "loops of the account did not end within the grace period and were aborted"
            );
        }
    }
    // Item 10: one last save, and the stream closed.
    if let Err(e) = tokio::time::timeout(grace, store.close())
        .await
        .map(|saved| {
            if let Err(e) = saved {
                tracing::warn!(
                    name: "purple_last_save_failed",
                    error = &e as &dyn std::error::Error,
                    "the last save before release failed; the next open reads the last good one"
                );
            }
        })
    {
        tracing::warn!(name: "purple_close_timed_out", error = %e, "closing the store timed out");
        return Err(keep(ctx, store, "closing the store timed out"));
    }
    let lock = Arc::downgrade(store.state_lock());
    // Item 10: nothing else may hold the store (and with it the agent).
    let mut store = store;
    let mut tries = 0;
    let owned = loop {
        match Arc::try_unwrap(store) {
            Ok(alone) => break alone,
            Err(still) if tries == 20 => {
                return Err(keep(ctx, still, "part of it is still in use"));
            }
            Err(still) => {
                store = still;
                tries += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    // Item 11.
    for dir in [ctx.state_dir().to_owned(), ctx.state_dir().join(BLOBS_DIR)] {
        if let Ok(handle) = std::fs::File::open(&dir) {
            let _ = handle.sync_all();
        }
    }
    // Item 12: the store, the agent, both database connections, and with
    // the last of them the lock.
    drop(owned);
    if lock.upgrade().is_some() {
        return Err(Failure::fatal(
            Status::Internal,
            "The account's state lock is still held after its release.",
        ));
    }
    tracing::debug!(name: "purple_account_released", "the account's state is released");
    Ok(())
}

/// A store that cannot be released is kept, with its lock, until the
/// process ends (§6.3 item 10: a late release would be worse).
fn keep(ctx: &Context, store: Arc<Store>, why: &str) -> Failure {
    tracing::error!(name: "purple_release_failed", why, "the account could not be released");
    ctx.kept
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(store);
    Failure::fatal(
        Status::Internal,
        format!(
            "The account could not be released ({why}). It stays locked until this program \
             quits."
        ),
    )
}
