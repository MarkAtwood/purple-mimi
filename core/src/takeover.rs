// SPDX-License-Identifier: GPL-3.0-or-later
//! The takeover handshake (docs/design/purple-mimi.md §6.3; decisions P6,
//! PQ2): a second instance asks the holder of an account's state lock to
//! let go, and the holder releases cleanly before the newcomer opens the
//! database.
//!
//! The lock itself is mimid-agent-core's (`StateLock`, taken by
//! `Agent::open`). This module has the request file and the newcomer's
//! loop:
//!
//! - The request is `<state>/takeover`: three lines, the requester's
//!   process id, the time in seconds since the Unix epoch, and a random
//!   128-bit nonce in hex. It is written aside and renamed into place, so
//!   it is read whole or not at all. A newer request replaces an older
//!   one.
//! - The newcomer tries the lock; while it is held, it writes its request
//!   once and tries again every [`Timings::poll`], for at most
//!   [`Timings::wait`]. Then it gives up and deletes its request (if it is
//!   still its own); it never breaks the lock.
//! - The holder looks at the request every [`Timings::watch`]; a fresh
//!   request with another nonce than its own starts its release.
//!
//! # Limitations
//!
//! - A request older than [`Timings::wait`] plus [`STALE_MARGIN`] is
//!   stale: its writer has given up or died. The holder ignores it, and
//!   whoever holds the lock deletes it. This rule is not in the design
//!   note, which would have a stale request make every later holder
//!   release at once.
//! - Deleting one's own request reads it first and removes it after; a
//!   request written in between by a third instance is removed with it,
//!   and that instance then times out.
//! - The holder's process id is not recorded, so a newcomer that times
//!   out cannot name it.

use std::fmt::Write as _;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The request file in the state directory.
pub const TAKEOVER_FILE: &str = "takeover";

/// How much longer than [`Timings::wait`] a request counts as fresh.
/// INVENTED VALUE.
pub const STALE_MARGIN: Duration = Duration::from_secs(5);

/// The timings of the handshake and the release (docs/design/
/// purple-mimi.md §6.3, all INVENTED and untested in the design note).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timings {
    /// How often a newcomer tries the lock again (100 ms).
    pub poll: Duration,
    /// How long a newcomer waits for the holder (30 s).
    pub wait: Duration,
    /// How often the holder looks for a request (1 s).
    pub watch: Duration,
    /// How long the holder waits for work in flight when it releases
    /// (10 s).
    pub grace: Duration,
    /// How long `shutdown` waits for every account to close (5 s).
    pub shutdown: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(100),
            wait: Duration::from_secs(30),
            watch: Duration::from_secs(1),
            grace: Duration::from_secs(10),
            shutdown: Duration::from_secs(5),
        }
    }
}

/// A takeover request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The requester's process id.
    pub pid: u32,
    /// When it was made, in seconds since the Unix epoch.
    pub at: u64,
    /// The requester's nonce, 32 hex digits.
    pub nonce: String,
}

/// Seconds since the Unix epoch.
#[must_use]
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A random 128-bit nonce in hex.
///
/// # Errors
///
/// If the system's random source fails.
pub fn new_nonce() -> Result<String, getrandom::Error> {
    let mut octets = [0u8; 16];
    getrandom::fill(&mut octets)?;
    let mut hex = String::with_capacity(32);
    for octet in octets {
        let _ = write!(hex, "{octet:02x}");
    }
    Ok(hex)
}

impl Request {
    /// A request by this process, now, with `nonce`.
    #[must_use]
    pub fn mine(nonce: &str) -> Self {
        Self {
            pid: std::process::id(),
            at: now_secs(),
            nonce: nonce.to_owned(),
        }
    }

    fn encode(&self) -> String {
        format!("{}\n{}\n{}\n", self.pid, self.at, self.nonce)
    }

    fn decode(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let pid = lines.next()?.parse().ok()?;
        let at = lines.next()?.parse().ok()?;
        let nonce = lines.next()?;
        let ok = nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_hexdigit());
        (ok && lines.next().is_none()).then(|| Self {
            pid,
            at,
            nonce: nonce.to_owned(),
        })
    }

    /// Whether the request is fresh at `now` under `timings`: not older
    /// than the wait plus [`STALE_MARGIN`]. A request from the future is
    /// fresh.
    #[must_use]
    pub fn fresh(&self, now: u64, timings: &Timings) -> bool {
        now.saturating_sub(self.at) <= (timings.wait + STALE_MARGIN).as_secs().max(1)
    }
}

/// Writes `request` as state directory `dir`'s request, replacing any.
///
/// # Errors
///
/// If it cannot be written.
pub fn write_request(dir: &Path, request: &Request) -> std::io::Result<()> {
    let aside = dir.join(format!("{TAKEOVER_FILE}.{}", request.nonce));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&aside)?;
    file.write_all(request.encode().as_bytes())?;
    drop(file);
    std::fs::rename(&aside, dir.join(TAKEOVER_FILE))
}

/// The request in state directory `dir`, or `None` if there is none or it
/// is malformed.
#[must_use]
pub fn read_request(dir: &Path) -> Option<Request> {
    std::fs::read_to_string(dir.join(TAKEOVER_FILE))
        .ok()
        .as_deref()
        .and_then(Request::decode)
}

/// Removes the request in `dir` if its nonce is `nonce`, or if it is
/// stale or malformed. Whether something was removed.
pub fn remove_own_or_stale(dir: &Path, nonce: &str, timings: &Timings) -> bool {
    let path = dir.join(TAKEOVER_FILE);
    if !path.exists() {
        return false;
    }
    let remove = match read_request(dir) {
        Some(request) => request.nonce == nonce || !request.fresh(now_secs(), timings),
        None => true,
    };
    remove && std::fs::remove_file(&path).is_ok()
}

/// What the holder with nonce `own` should do about `dir`'s request:
/// `true` if another instance asks for the account.
#[must_use]
pub fn asked_to_release(dir: &Path, own: &str, timings: &Timings) -> bool {
    read_request(dir).is_some_and(|r| r.nonce != own && r.fresh(now_secs(), timings))
}

/// One try at the lock.
#[derive(Debug)]
pub enum Attempt<T, E> {
    /// Got it, and whatever came with it.
    Got(T),
    /// Another holds it.
    Locked,
    /// Failed otherwise; the handshake ends with this.
    Failed(E),
}

/// How the handshake ended without the lock.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal<E> {
    /// The holder did not release within [`Timings::wait`].
    TimedOut,
    /// A try failed otherwise.
    Failed(E),
    /// The request could not be written.
    Io(String),
}

/// Removes the newcomer's request when the handshake ends, however it
/// ends (the future may be dropped).
struct RequestGuard {
    dir: PathBuf,
    nonce: String,
    timings: Timings,
    written: bool,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if self.written {
            remove_own_or_stale(&self.dir, &self.nonce, &self.timings);
        }
    }
}

/// The newcomer's side (§6.3 items 1 to 5): tries the lock with
/// `attempt`; while another holds it, writes a request with `nonce` once
/// (calling `waiting` then) and tries again every `timings.poll` for at
/// most `timings.wait`. On success the request is removed if it is this
/// one's or stale, and kept if a third instance replaced it, so that this
/// holder's watch hands over to that one.
///
/// # Errors
///
/// [`Refusal::TimedOut`], or the first failure that is not
/// [`Attempt::Locked`].
pub async fn acquire<T, E, F, Fut>(
    dir: &Path,
    nonce: &str,
    timings: &Timings,
    mut attempt: F,
    mut waiting: impl FnMut(),
) -> Result<T, Refusal<E>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Attempt<T, E>>,
{
    let start = tokio::time::Instant::now();
    let mut guard = RequestGuard {
        dir: dir.to_owned(),
        nonce: nonce.to_owned(),
        timings: *timings,
        written: false,
    };
    loop {
        match attempt().await {
            Attempt::Got(value) => {
                remove_own_or_stale(dir, nonce, timings);
                guard.written = false;
                return Ok(value);
            }
            Attempt::Failed(e) => return Err(Refusal::Failed(e)),
            Attempt::Locked => {}
        }
        if !guard.written {
            write_request(dir, &Request::mine(nonce))
                .map_err(|e| Refusal::Io(format!("{}: {e}", dir.join(TAKEOVER_FILE).display())))?;
            guard.written = true;
            waiting();
        }
        if start.elapsed() >= timings.wait {
            return Err(Refusal::TimedOut);
        }
        tokio::time::sleep(timings.poll).await;
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn quick() -> Timings {
        Timings {
            poll: Duration::from_millis(10),
            wait: Duration::from_millis(500),
            watch: Duration::from_millis(10),
            grace: Duration::from_millis(100),
            shutdown: Duration::from_millis(100),
        }
    }

    /// The lock as std's file lock gives it: what mimid-agent-core's
    /// `StateLock` uses.
    fn try_lock(dir: &Path) -> Attempt<File, String> {
        let file = File::create(dir.join("lock")).unwrap();
        match file.try_lock() {
            Ok(()) => Attempt::Got(file),
            Err(std::fs::TryLockError::WouldBlock) => Attempt::Locked,
            Err(std::fs::TryLockError::Error(e)) => Attempt::Failed(e.to_string()),
        }
    }

    #[test]
    fn requests_round_trip_and_reject_junk() {
        let dir = tempfile::tempdir().unwrap();
        let nonce = new_nonce().unwrap();
        assert_eq!(nonce.len(), 32);
        let request = Request::mine(&nonce);
        write_request(dir.path(), &request).unwrap();
        assert_eq!(read_request(dir.path()), Some(request));
        for junk in [
            "",
            "1\n2\n",
            "x\n2\n00000000000000000000000000000000\n",
            "1\n2\n0000\n",
            "1\n2\n0000000000000000000000000000000g\n",
            "1\n2\n00000000000000000000000000000000\nmore\n",
        ] {
            std::fs::write(dir.path().join(TAKEOVER_FILE), junk).unwrap();
            assert_eq!(read_request(dir.path()), None, "{junk:?}");
        }
    }

    #[test]
    fn the_holder_releases_only_for_a_fresh_foreign_request() {
        let dir = tempfile::tempdir().unwrap();
        let t = Timings::default();
        assert!(!asked_to_release(dir.path(), "a", &t));
        let own = new_nonce().unwrap();
        write_request(dir.path(), &Request::mine(&own)).unwrap();
        assert!(!asked_to_release(dir.path(), &own, &t), "its own");
        let other = new_nonce().unwrap();
        write_request(dir.path(), &Request::mine(&other)).unwrap();
        assert!(asked_to_release(dir.path(), &own, &t));
        let stale = Request {
            at: now_secs() - 60,
            ..Request::mine(&other)
        };
        write_request(dir.path(), &stale).unwrap();
        assert!(!asked_to_release(dir.path(), &own, &t), "stale");
        assert!(remove_own_or_stale(dir.path(), &own, &t), "stale goes");
        assert!(!dir.path().join(TAKEOVER_FILE).exists());
    }

    #[tokio::test]
    async fn a_free_lock_is_taken_at_once_without_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let waited = AtomicUsize::new(0);
        let got = acquire(
            dir.path(),
            &new_nonce().unwrap(),
            &quick(),
            || async { try_lock(dir.path()) },
            || {
                waited.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;
        assert!(got.is_ok());
        assert_eq!(waited.load(Ordering::SeqCst), 0);
        assert!(!dir.path().join(TAKEOVER_FILE).exists());
    }

    #[tokio::test]
    async fn the_newcomer_waits_for_the_holder_then_takes_over() {
        let dir = tempfile::tempdir().unwrap();
        let t = quick();
        let holder_nonce = new_nonce().unwrap();
        let Attempt::Got(held) = try_lock(dir.path()) else {
            panic!("the first lock")
        };
        // The holder's watch: releases when asked.
        let watch_dir = dir.path().to_owned();
        let holder = tokio::spawn(async move {
            loop {
                if asked_to_release(&watch_dir, &holder_nonce, &t) {
                    drop(held);
                    return;
                }
                tokio::time::sleep(t.watch).await;
            }
        });
        let waited = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&waited);
        let got = acquire(
            dir.path(),
            &new_nonce().unwrap(),
            &t,
            || async { try_lock(dir.path()) },
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;
        assert!(got.is_ok());
        assert_eq!(waited.load(Ordering::SeqCst), 1, "one request, once");
        assert!(!dir.path().join(TAKEOVER_FILE).exists(), "removed");
        holder.await.unwrap();
    }

    #[tokio::test]
    async fn a_holder_that_never_releases_times_the_newcomer_out() {
        let dir = tempfile::tempdir().unwrap();
        let Attempt::Got(_held) = try_lock(dir.path()) else {
            panic!("the first lock")
        };
        let started = std::time::Instant::now();
        let got = acquire(
            dir.path(),
            &new_nonce().unwrap(),
            &quick(),
            || async { try_lock(dir.path()) },
            || {},
        )
        .await;
        assert_eq!(got.err(), Some(Refusal::TimedOut));
        assert!(started.elapsed() >= quick().wait);
        assert!(
            !dir.path().join(TAKEOVER_FILE).exists(),
            "the newcomer's request is gone"
        );
    }

    #[tokio::test]
    async fn a_third_instances_request_survives_the_second_getting_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let t = quick();
        let second = new_nonce().unwrap();
        let third = new_nonce().unwrap();
        let mut tries = 0;
        let got = acquire(
            dir.path(),
            &second,
            &t,
            || {
                tries += 1;
                let path = dir.path().to_owned();
                let third = third.clone();
                async move {
                    if tries < 3 {
                        Attempt::<(), String>::Locked
                    } else {
                        // A third instance asked while the second waited.
                        write_request(&path, &Request::mine(&third)).unwrap();
                        Attempt::Got(())
                    }
                }
            },
            || {},
        )
        .await;
        assert!(got.is_ok());
        assert_eq!(
            read_request(dir.path()).map(|r| r.nonce),
            Some(third.clone())
        );
        assert!(asked_to_release(dir.path(), &second, &t));
    }

    #[tokio::test]
    async fn other_failures_end_the_handshake_and_remove_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut tries = 0;
        let got = acquire(
            dir.path(),
            &new_nonce().unwrap(),
            &quick(),
            || {
                tries += 1;
                let attempt = if tries == 1 {
                    Attempt::<(), &str>::Locked
                } else {
                    Attempt::Failed("network")
                };
                async move { attempt }
            },
            || {},
        )
        .await;
        assert_eq!(got.err(), Some(Refusal::Failed("network")));
        assert!(!dir.path().join(TAKEOVER_FILE).exists());
    }
}
