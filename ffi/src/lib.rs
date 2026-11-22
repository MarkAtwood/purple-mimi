// SPDX-License-Identifier: GPL-3.0-or-later
//! The C ABI of purple-mimi (docs/design/purple-mimi.md §5.5), and nothing
//! else: argument checks, conversions and `catch_unwind` around
//! `purple-mimi-core`. The header `include/purple-mimi.h` is generated
//! from this file by cbindgen.
//!
//! Rules for every function:
//!
//! - **Thread.** Call it only on the thread that called [`mimi_init`]; a
//!   call from another thread returns `MIMI_ERR_WRONG_THREAD` (or NULL, or
//!   nothing) and does nothing. Exceptions, marked "Any thread": the
//!   functions that touch no state of the library.
//! - **No panics across the boundary.** Every function runs its body
//!   inside `catch_unwind`; a panic returns `MIMI_ERR_PANIC` (or NULL),
//!   and an account it happened in is poisoned: closed, with
//!   `MIMI_EV_DISCONNECTED` and `MIMI_ERR_PANIC`, and its other calls
//!   return `MIMI_ERR_POISONED`.
//! - **No callbacks.** Everything comes back as events, through
//!   [`mimi_next_event`], when [`mimi_event_fd`] is readable.
//! - **Strings in** are UTF-8, NUL-terminated, borrowed for the call.
//!   **Strings out** are UTF-8, NUL-terminated, with any U+0000 replaced by
//!   U+FFFD, and are freed with [`mimi_string_free`] (standalone) or with
//!   the free function of the struct holding them.
//!
//! # Limitations
//!
//! - Step 4's subset of the design's 51 functions: the process, the
//!   mapping helpers, and opening, closing and describing accounts. Rooms,
//!   messages and requests come with steps 5 and 6.
//! - The thread rule is checked by thread identity only.

#![allow(non_camel_case_types)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::Duration;

use purple_mimi_core::{
    AccountConfig, AccountId, Event, EventKind, Failure, InvitePolicy, Process, ProcessOptions,
    Token,
};

/// The version of this ABI. A glue built against another version refuses
/// to load: it compares [`mimi_abi_version`] with the header's
/// `MIMI_ABI_VERSION`, and passes its own in [`mimi_init_options`].
pub const MIMI_ABI_VERSION: u32 = 1;

/// A status code: `MIMI_OK` or one of the `MIMI_ERR_*` codes.
pub type mimi_status = i32;

/// Done, or queued.
pub const MIMI_OK: mimi_status = 0;
/// The header and the library are of different ABI versions.
pub const MIMI_ERR_ABI: mimi_status = 1;
/// Called from another thread than the one that called `mimi_init`.
pub const MIMI_ERR_WRONG_THREAD: mimi_status = 2;
/// NULL, invalid UTF-8, or a value out of range.
pub const MIMI_ERR_INVALID_ARGUMENT: mimi_status = 3;
/// The account is not open.
pub const MIMI_ERR_NOT_CONNECTED: mimi_status = 4;
/// No such room.
pub const MIMI_ERR_NO_SUCH_ROOM: mimi_status = 5;
/// No such message.
pub const MIMI_ERR_NO_SUCH_MESSAGE: mimi_status = 6;
/// No such invite.
pub const MIMI_ERR_NO_SUCH_INVITE: mimi_status = 7;
/// No such device.
pub const MIMI_ERR_NO_SUCH_DEVICE: mimi_status = 8;
/// The room's policy refuses it.
pub const MIMI_ERR_NOT_PERMITTED: mimi_status = 9;
/// The hub refused it.
pub const MIMI_ERR_REFUSED: mimi_status = 10;
/// Text or a file past a limit.
pub const MIMI_ERR_TOO_LARGE: mimi_status = 11;
/// The provider could not be reached, or the stream failed.
pub const MIMI_ERR_NETWORK: mimi_status = 12;
/// The provider refused the token, or no token is stored.
pub const MIMI_ERR_UNAUTHORIZED: mimi_status = 13;
/// The provider does not serve mimid's live stream.
pub const MIMI_ERR_STREAM_OFF: mimi_status = 14;
/// The token belongs to another user.
pub const MIMI_ERR_WRONG_USER: mimi_status = 15;
/// Another instance holds the state and did not release it.
pub const MIMI_ERR_LOCKED: mimi_status = 16;
/// Another instance took this account over.
pub const MIMI_ERR_TAKEN_OVER: mimi_status = 17;
/// The state lock file was removed or replaced under us.
pub const MIMI_ERR_LOCK_LOST: mimi_status = 18;
/// The state directory or database cannot be used.
pub const MIMI_ERR_STATE: mimi_status = 19;
/// A local file could not be read or written.
pub const MIMI_ERR_IO: mimi_status = 20;
/// The library panicked; the account, if any, is closed.
pub const MIMI_ERR_PANIC: mimi_status = 21;
/// The account panicked before and was closed.
pub const MIMI_ERR_POISONED: mimi_status = 22;
/// Anything else; the text says what.
pub const MIMI_ERR_INTERNAL: mimi_status = 23;
/// `mimi_init` was not called, or `mimi_shutdown` was.
pub const MIMI_ERR_NOT_INITIALIZED: mimi_status = 24;

/// Log levels, for [`mimi_set_log_level`] and `MIMI_EV_LOG` events.
pub type mimi_log_level = i32;
/// No log events.
pub const MIMI_LOG_NONE: mimi_log_level = 0;
/// Errors.
pub const MIMI_LOG_ERROR: mimi_log_level = 1;
/// Warnings and errors.
pub const MIMI_LOG_WARN: mimi_log_level = 2;
/// Information too.
pub const MIMI_LOG_INFO: mimi_log_level = 3;
/// Debugging too.
pub const MIMI_LOG_DEBUG: mimi_log_level = 4;
/// Everything.
pub const MIMI_LOG_TRACE: mimi_log_level = 5;

/// Which invites an account joins by itself (decision PQ6).
pub type mimi_invite_policy = i32;
/// Invites from users this user allowed; the rest wait. The default.
pub const MIMI_INVITES_FROM_ALLOWED: mimi_invite_policy = 0;
/// Every invite.
pub const MIMI_INVITES_ANY: mimi_invite_policy = 1;
/// None; every invite waits.
pub const MIMI_INVITES_NEVER: mimi_invite_policy = 2;

/// Event kinds.
pub type mimi_event_kind = i32;
/// A step of connecting: `step`, `steps`, `text`.
pub const MIMI_EV_PROGRESS: mimi_event_kind = 1;
/// The account is open; its token is now kept in the state directory.
pub const MIMI_EV_CONNECTED: mimi_event_kind = 2;
/// The account is closed by a failure or a takeover: `status`,
/// `temporary`, `text`. Never sent after `mimi_account_close`.
pub const MIMI_EV_DISCONNECTED: mimi_event_kind = 3;
/// A log line: `level`, `text`; `account` is NULL.
pub const MIMI_EV_LOG: mimi_event_kind = 11;

/// What [`mimi_init`] starts with.
#[repr(C)]
pub struct mimi_init_options {
    /// `MIMI_ABI_VERSION` of the header the caller was built with.
    pub abi_version: u32,
    /// The log level, `MIMI_LOG_*`.
    pub log_level: mimi_log_level,
}

/// An account, from [`mimi_account_open`] until [`mimi_account_close`].
pub struct mimi_account {
    id: AccountId,
}

/// What [`mimi_account_open`] takes. Every string is borrowed for the call.
#[repr(C)]
pub struct mimi_account_config {
    /// libpurple's base directory, absolute (`purple_user_dir()` on
    /// libpurple 2, `purple_data_dir()` on libpurple 3). Required.
    pub base_dir: *const c_char,
    /// The user: a handle (`alice@a.example`) or a user URI. Required.
    pub user: *const c_char,
    /// The provider's URL, or NULL or "" for `https://<domain>`.
    pub provider_url: *const c_char,
    /// The provider's bearer token, or NULL or "" to use the one stored in
    /// the state directory. A token given replaces the stored one once the
    /// account is open.
    pub token: *const c_char,
    /// `MIMI_INVITES_*`.
    pub invite_policy: mimi_invite_policy,
    /// How many KeyPackages to keep published, 1 to 64; 0 means 10.
    pub key_packages: u32,
    /// The fallback sync interval in milliseconds, at least 50; 0 means
    /// 30000.
    pub sync_interval_ms: u32,
    /// Take the account back even if this process saw another program
    /// take it over (§6.6). The libpurple 2 glue always sets it; the
    /// libpurple 3 glue only from its account action.
    pub take_over: bool,
}

/// What [`mimi_account_info`] gives. Freed with [`mimi_account_info_free`].
#[repr(C)]
pub struct mimi_account_info {
    /// The user URI.
    pub user_uri: *mut c_char,
    /// The handle.
    pub handle: *mut c_char,
    /// The state directory.
    pub state_dir: *mut c_char,
    /// This device's key fingerprint (16 hex digits), or NULL before the
    /// account was first open.
    pub key_fingerprint: *mut c_char,
    /// Whether the account is open now.
    pub connected: bool,
}

/// One event. Freed with [`mimi_event_free`].
#[repr(C)]
pub struct mimi_event {
    /// `MIMI_EV_*`.
    pub kind: mimi_event_kind,
    /// The account, or NULL for the process.
    pub account: *mut mimi_account,
    /// `MIMI_EV_DISCONNECTED`: why.
    pub status: mimi_status,
    /// `MIMI_EV_DISCONNECTED`: whether connecting again later may help.
    pub temporary: bool,
    /// `MIMI_EV_PROGRESS`: this step, from 1.
    pub step: u32,
    /// `MIMI_EV_PROGRESS`: how many steps.
    pub steps: u32,
    /// `MIMI_EV_LOG`: the level.
    pub level: mimi_log_level,
    /// The event's text, or NULL.
    pub text: *mut c_char,
}

/// The library's state, made by `mimi_init`.
struct State {
    thread: ThreadId,
    process: Process,
    /// The open handles, by account id, as addresses (raw pointers are
    /// not `Send`).
    accounts: HashMap<AccountId, usize>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn state() -> MutexGuard<'static, Option<State>> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs `body`, giving `on_panic` if it panics.
fn guard<R>(on_panic: R, body: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(on_panic)
}

/// A copy of `text` as a C string: U+0000 becomes U+FFFD.
fn c_string(text: &str) -> *mut c_char {
    let text = if text.contains('\0') {
        text.replace('\0', "\u{FFFD}")
    } else {
        text.to_owned()
    };
    CString::new(text).expect("U+0000 was replaced").into_raw()
}

/// Sets `*error` to `text` if `error` is not NULL.
///
/// # Safety
///
/// `error` is NULL or valid for one write.
unsafe fn set_error(error: *mut *mut c_char, text: &str) {
    if !error.is_null() {
        // SAFETY: the caller's contract.
        unsafe { error.write(c_string(text)) };
    }
}

/// Sets `*error` to `failure`'s text and gives its status.
///
/// # Safety
///
/// `error` is NULL or valid for one write.
unsafe fn fail(error: *mut *mut c_char, failure: &Failure) -> mimi_status {
    // SAFETY: the caller's contract.
    unsafe { set_error(error, &failure.text) };
    failure.status.code()
}

/// The string at `text`, or `None` if it is NULL or not UTF-8.
///
/// # Safety
///
/// `text` is NULL or a NUL-terminated string valid for the call.
unsafe fn borrowed<'a>(text: *const c_char) -> Option<&'a str> {
    if text.is_null() {
        return None;
    }
    // SAFETY: the caller's contract.
    unsafe { CStr::from_ptr(text) }.to_str().ok()
}

/// The state, if the call is on the `mimi_init` thread; else the status
/// to give.
fn on_thread(state: &Option<State>) -> Result<&State, mimi_status> {
    match state {
        None => Err(MIMI_ERR_NOT_INITIALIZED),
        Some(s) if s.thread != std::thread::current().id() => Err(MIMI_ERR_WRONG_THREAD),
        Some(s) => Ok(s),
    }
}

/// The ABI version of this library, `MIMI_ABI_VERSION` as it was built.
/// Any thread.
#[unsafe(no_mangle)]
pub extern "C" fn mimi_abi_version() -> u32 {
    MIMI_ABI_VERSION
}

/// The library's version and the mimid revision it is built on; static,
/// not freed. Any thread.
#[unsafe(no_mangle)]
pub extern "C" fn mimi_version() -> *const c_char {
    static VERSION: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| {
            CString::new(format!(
                "purple-mimi {} (mimid {})",
                purple_mimi_core::VERSION,
                purple_mimi_core::MIMID_REVISION
            ))
            .expect("no NUL in a version")
        })
        .as_ptr()
}

/// Whether `status` is a code this ABI defines. Any thread.
#[unsafe(no_mangle)]
pub extern "C" fn mimi_status_known(status: mimi_status) -> bool {
    (MIMI_OK..=MIMI_ERR_NOT_INITIALIZED).contains(&status)
}

/// Starts the library: the runtime on its own threads and the event
/// queue. Records the calling thread, on which every later call must be
/// made. `MIMI_ERR_ABI` if `options->abi_version` is not this library's;
/// `MIMI_ERR_INVALID_ARGUMENT` if already started.
///
/// # Safety
///
/// `options` is NULL or points to a valid `mimi_init_options`; `error` is
/// NULL or valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mimi_init(
    options: *const mimi_init_options,
    error: *mut *mut c_char,
) -> mimi_status {
    guard(MIMI_ERR_PANIC, || {
        if options.is_null() {
            // SAFETY: the caller's contract.
            unsafe { set_error(error, "no options") };
            return MIMI_ERR_INVALID_ARGUMENT;
        }
        // SAFETY: the caller's contract.
        let options = unsafe { &*options };
        if options.abi_version != MIMI_ABI_VERSION {
            let text = format!(
                "purple-mimi's library has ABI {MIMI_ABI_VERSION}, the plugin was built for {}",
                options.abi_version
            );
            // SAFETY: the caller's contract.
            unsafe { set_error(error, &text) };
            return MIMI_ERR_ABI;
        }
        let mut state = state();
        if state.is_some() {
            // SAFETY: the caller's contract.
            unsafe { set_error(error, "already started") };
            return MIMI_ERR_INVALID_ARGUMENT;
        }
        let process_options = ProcessOptions::default().with_log_level(options.log_level);
        match Process::start(process_options) {
            Ok(process) => {
                *state = Some(State {
                    thread: std::thread::current().id(),
                    process,
                    accounts: HashMap::new(),
                });
                MIMI_OK
            }
            // SAFETY: the caller's contract.
            Err(failure) => unsafe { fail(error, &failure) },
        }
    })
}

/// Closes every account by the release path, waits up to 5 seconds for
/// them, and stops the runtime. Every handle is invalid afterwards. Does
/// nothing if not started, or on another thread.
#[unsafe(no_mangle)]
pub extern "C" fn mimi_shutdown() {
    guard((), || {
        let taken = {
            let mut state = state();
            if on_thread(&state).is_err() {
                return;
            }
            state.take()
        };
        if let Some(state) = taken {
            for address in state.accounts.into_values() {
                // SAFETY: each address is a Box made by mimi_account_open
                // and not freed while in the map.
                drop(unsafe { Box::from_raw(address as *mut mimi_account) });
            }
            state.process.shutdown();
        }
    });
}

/// The descriptor to watch for reading: readable while events wait. -1
/// if not started or on another thread.
#[unsafe(no_mangle)]
pub extern "C" fn mimi_event_fd() -> c_int {
    guard(-1, || match on_thread(&state()) {
        Ok(s) => s.process.event_fd(),
        Err(_) => -1,
    })
}

/// The next event, or NULL if none waits (or not started, or on another
/// thread). Taking the last one drains the descriptor. Free it with
/// [`mimi_event_free`].
#[unsafe(no_mangle)]
pub extern "C" fn mimi_next_event() -> *mut mimi_event {
    guard(std::ptr::null_mut(), || {
        let state = state();
        let Ok(s) = on_thread(&state) else {
            return std::ptr::null_mut();
        };
        loop {
            let Some(event) = s.process.next_event() else {
                return std::ptr::null_mut();
            };
            let account = match event.account {
                None => std::ptr::null_mut(),
                Some(id) => match s.accounts.get(&id) {
                    Some(&address) => address as *mut mimi_account,
                    // Closed meanwhile: no event names it.
                    None => continue,
                },
            };
            return Box::into_raw(Box::new(c_event(&event, account)));
        }
    })
}

fn c_event(event: &Event, account: *mut mimi_account) -> mimi_event {
    let mut out = mimi_event {
        kind: 0,
        account,
        status: MIMI_OK,
        temporary: false,
        step: 0,
        steps: 0,
        level: MIMI_LOG_NONE,
        text: std::ptr::null_mut(),
    };
    match &event.kind {
        EventKind::Progress { step, steps, text } => {
            out.kind = MIMI_EV_PROGRESS;
            out.step = *step;
            out.steps = *steps;
            out.text = c_string(text);
        }
        EventKind::Connected => out.kind = MIMI_EV_CONNECTED,
        EventKind::Disconnected {
            status,
            temporary,
            text,
        } => {
            out.kind = MIMI_EV_DISCONNECTED;
            out.status = status.code();
            out.temporary = *temporary;
            out.text = c_string(text);
        }
        EventKind::Log { level, text } => {
            out.kind = MIMI_EV_LOG;
            out.level = *level as i32;
            out.text = c_string(text);
        }
        other => {
            out.kind = MIMI_EV_LOG;
            out.level = MIMI_LOG_DEBUG;
            out.text = c_string(&format!("an event this ABI does not carry: {other:?}"));
        }
    }
    out
}

/// Frees an event. NULL is ignored. Any thread.
///
/// # Safety
///
/// `event` is NULL or came from [`mimi_next_event`] and was not freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mimi_event_free(event: *mut mimi_event) {
    guard((), || {
        if event.is_null() {
            return;
        }
        // SAFETY: the caller's contract: a Box from mimi_next_event.
        let event = unsafe { Box::from_raw(event) };
        // SAFETY: text is NULL or from c_string.
        unsafe { mimi_string_free(event.text) };
    });
}

/// Frees a string from the library. NULL is ignored. Any thread.
///
/// # Safety
///
/// `text` is NULL or a standalone string from this library, not freed
/// before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mimi_string_free(text: *mut c_char) {
    guard((), || {
        if !text.is_null() {
            // SAFETY: the caller's contract: CString::into_raw of c_string.
            drop(unsafe { CString::from_raw(text) });
        }
    });
}

/// Sets the level from which log lines become `MIMI_EV_LOG` events,
/// `MIMI_LOG_NONE` to `MIMI_LOG_TRACE`; values outside are clamped. Any
/// thread.
#[unsafe(no_mangle)]
pub extern "C" fn mimi_set_log_level(level: mimi_log_level) {
    guard((), || purple_mimi_core::logging::set_level(level));
}

/// The state directory of `user` (handle or URI) under base directory
/// `base`: `<base>/mimi/<handle>`. NULL if `user` names no MIMI user.
/// Free with [`mimi_string_free`]. Any thread.
///
/// # Safety
///
/// Both are NULL or NUL-terminated strings valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mimi_state_dir_for(
    base: *const c_char,
    user: *const c_char,
) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        // SAFETY: the caller's contract.
        let (Some(base), Some(user)) = (unsafe { borrowed(base) }, unsafe { borrowed(user) })
        else {
            return std::ptr::null_mut();
        };
        purple_mimi_core::handle::state_dir_for(std::path::Path::new(base), user)
            .and_then(|path| path.to_str().map(c_string))
            .unwrap_or(std::ptr::null_mut())
    })
}

/// The handle (`alice@a.example`) of user URI or handle `user`, or NULL.
/// Free with [`mimi_string_free`]. Any thread.
///
/// # Safety
///
/// `user` is NULL or a NUL-terminated string valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mimi_user_to_handle(user: *const c_char) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        // SAFETY: the caller's contract.
        unsafe { borrowed(user) }
            .and_then(purple_mimi_core::handle::handle_of)
            .map_or(std::ptr::null_mut(), |h| c_string(&h))
    })
}

/// The user URI (`mimi://a.example/u/alice`) of handle or URI `handle`,
/// or NULL. Free with [`mimi_string_free`]. Any thread.
///
/// # Safety
///
/// `handle` is NULL or a NUL-terminated string valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mimi_handle_to_user(handle: *const c_char) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        // SAFETY: the caller's contract.
        unsafe { borrowed(handle) }
            .and_then(purple_mimi_core::handle::user_uri_of)
            .map_or(std::ptr::null_mut(), |u| c_string(&u))
    })
}

/// The provider URL `user` (handle or URI) gets when none is set,
/// `https://<domain>`, or NULL. Free with [`mimi_string_free`]. Any
/// thread.
///
/// # Safety
///
/// `user` is NULL or a NUL-terminated string valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mimi_default_provider_url(user: *const c_char) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        // SAFETY: the caller's contract.
        unsafe { borrowed(user) }
            .and_then(purple_mimi_core::handle::default_provider_url)
            .map_or(std::ptr::null_mut(), |u| c_string(&u))
    })
}

/// Whether a provider token is stored for `user` under `base` (§12.3), so
/// that the account can open without one. Any thread.
///
/// # Safety
///
/// Both are NULL or NUL-terminated strings valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mimi_token_stored(base: *const c_char, user: *const c_char) -> bool {
    guard(false, || {
        // SAFETY: the caller's contract.
        let (Some(base), Some(user)) = (unsafe { borrowed(base) }, unsafe { borrowed(user) })
        else {
            return false;
        };
        purple_mimi_core::handle::state_dir_for(std::path::Path::new(base), user)
            .and_then(|dir| purple_mimi_core::token::read_stored(&dir).ok().flatten())
            .is_some()
    })
}

/// Checks `config` and starts opening the account in the background: the
/// state lock, with the takeover handshake if another instance holds it
/// (§6), then the live stream (§7). The result comes as
/// `MIMI_EV_CONNECTED` or `MIMI_EV_DISCONNECTED`, with `MIMI_EV_PROGRESS`
/// before. On `MIMI_OK`, `*out` is the account's handle.
///
/// # Safety
///
/// `config` is NULL or points to a valid `mimi_account_config` whose
/// strings are NULL or NUL-terminated; `out` and `error` are each NULL or
/// valid for one write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mimi_account_open(
    config: *const mimi_account_config,
    out: *mut *mut mimi_account,
    error: *mut *mut c_char,
) -> mimi_status {
    guard(MIMI_ERR_PANIC, || {
        let mut state = state();
        let s = match on_thread(&state) {
            Ok(s) => s,
            Err(status) => return status,
        };
        if config.is_null() || out.is_null() {
            // SAFETY: the caller's contract.
            unsafe { set_error(error, "config and out are required") };
            return MIMI_ERR_INVALID_ARGUMENT;
        }
        // SAFETY: the caller's contract, for the struct and its strings.
        let checked = unsafe { core_config(&*config) };
        let core = match checked {
            Ok(core) => core,
            Err(text) => {
                // SAFETY: the caller's contract.
                unsafe { set_error(error, &text) };
                return MIMI_ERR_INVALID_ARGUMENT;
            }
        };
        let opened = s.process.open_account(core);
        match opened {
            Ok(id) => {
                let handle = Box::into_raw(Box::new(mimi_account { id }));
                if let Some(s) = state.as_mut() {
                    s.accounts.insert(id, handle as usize);
                }
                // SAFETY: the caller's contract.
                unsafe { out.write(handle) };
                MIMI_OK
            }
            // SAFETY: the caller's contract.
            Err(failure) => unsafe { fail(error, &failure) },
        }
    })
}

/// The core's configuration from `config`, or why it is refused.
///
/// # Safety
///
/// Each string of `config` is NULL or NUL-terminated and valid for the
/// call.
unsafe fn core_config(config: &mimi_account_config) -> Result<AccountConfig, String> {
    // SAFETY: the caller's contract, for each.
    let (base, user, url, token) = unsafe {
        (
            borrowed(config.base_dir),
            borrowed(config.user),
            borrowed(config.provider_url),
            borrowed(config.token),
        )
    };
    let base = base.ok_or("base_dir is required, in UTF-8")?;
    let user = user.ok_or("user is required, in UTF-8")?;
    if !config.provider_url.is_null() && url.is_none() {
        return Err("provider_url is not UTF-8".to_owned());
    }
    if !config.token.is_null() && token.is_none() {
        return Err("the token is not UTF-8".to_owned());
    }
    let invites = match config.invite_policy {
        MIMI_INVITES_FROM_ALLOWED => InvitePolicy::FromAllowed,
        MIMI_INVITES_ANY => InvitePolicy::Any,
        MIMI_INVITES_NEVER => InvitePolicy::Never,
        other => return Err(format!("invite_policy {other} is not MIMI_INVITES_*")),
    };
    let key_packages = match config.key_packages {
        0 => 10,
        n => n,
    };
    let interval = match config.sync_interval_ms {
        0 => 30_000,
        n => n,
    };
    Ok(AccountConfig::new(PathBuf::from(base), user)
        .with_provider_url(url.map(str::to_owned))
        .with_token(token.and_then(Token::new))
        .with_invites(invites)
        .with_key_packages(key_packages)
        .with_sync_interval(Duration::from_millis(u64::from(interval)))
        .with_take_over(config.take_over))
}

/// Starts the account's release (§6.3) and frees the handle: it is invalid
/// on return, and no event names it from now on. NULL is ignored, and so
/// is a call from another thread.
///
/// # Safety
///
/// `account` is NULL or a handle from [`mimi_account_open`] not closed
/// before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mimi_account_close(account: *mut mimi_account) {
    guard((), || {
        if account.is_null() {
            return;
        }
        let mut state = state();
        let Some(s) = state.as_mut() else {
            return;
        };
        if s.thread != std::thread::current().id() {
            return;
        }
        // SAFETY: the caller's contract: a live handle.
        let id = unsafe { (*account).id };
        if s.accounts.remove(&id).is_some() {
            s.process.close_account(id);
            // SAFETY: the Box made by mimi_account_open, now out of the map.
            drop(unsafe { Box::from_raw(account) });
        }
    });
}

/// What is known about `account` now. On `MIMI_OK`, `*out` is to be freed
/// with [`mimi_account_info_free`]. `MIMI_ERR_POISONED` after a panic.
///
/// # Safety
///
/// `account` is NULL or a live handle; `out` is NULL or valid for one
/// write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mimi_account_get_info(
    account: *mut mimi_account,
    out: *mut *mut mimi_account_info,
) -> mimi_status {
    // SAFETY: the caller's contract.
    let id = (!account.is_null()).then(|| unsafe { (*account).id });
    guard_account(id, || {
        let state = state();
        let s = match on_thread(&state) {
            Ok(s) => s,
            Err(status) => return status,
        };
        let (Some(id), false) = (id, out.is_null()) else {
            return MIMI_ERR_INVALID_ARGUMENT;
        };
        match s.process.account_info(id) {
            Ok(info) => {
                let c = mimi_account_info {
                    user_uri: c_string(&info.user_uri),
                    handle: c_string(&info.handle),
                    state_dir: c_string(&info.state_dir.to_string_lossy()),
                    key_fingerprint: info
                        .fingerprint
                        .as_deref()
                        .map_or(std::ptr::null_mut(), c_string),
                    connected: info.connected,
                };
                // SAFETY: the caller's contract.
                unsafe { out.write(Box::into_raw(Box::new(c))) };
                MIMI_OK
            }
            Err(failure) => failure.status.code(),
        }
    })
}

/// Frees what [`mimi_account_get_info`] gave. NULL is ignored. Any thread.
///
/// # Safety
///
/// `info` is NULL or came from [`mimi_account_get_info`] and was not freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mimi_account_info_free(info: *mut mimi_account_info) {
    guard((), || {
        if info.is_null() {
            return;
        }
        // SAFETY: the caller's contract: a Box from mimi_account_get_info,
        // whose strings are each NULL or from c_string.
        unsafe {
            let info = Box::from_raw(info);
            mimi_string_free(info.user_uri);
            mimi_string_free(info.handle);
            mimi_string_free(info.state_dir);
            mimi_string_free(info.key_fingerprint);
        }
    });
}

/// [`guard`] for a call on account `id`: a panic poisons the account.
fn guard_account(id: Option<AccountId>, body: impl FnOnce() -> mimi_status) -> mimi_status {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(status) => status,
        Err(_) => {
            if let Some(id) = id
                && let Some(s) = state().as_ref()
            {
                s.process.poison(id);
            }
            MIMI_ERR_PANIC
        }
    }
}

/// Tests only, never in a shipped build: panics inside the C ABI in a call
/// on `account` (NULL for none), which poisons it, and returns what the
/// caller then gets, `MIMI_ERR_PANIC`. With `in_task`, instead makes the
/// next opened account's task panic as it starts, and returns
/// `MIMI_OK`.
///
/// # Safety
///
/// `account` is NULL or a live handle.
#[cfg(feature = "test-hooks")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mimi_test_panic(account: *mut mimi_account, in_task: bool) -> mimi_status {
    if in_task {
        return guard(MIMI_ERR_PANIC, || match on_thread(&state()) {
            Ok(s) => {
                s.process.test_panic_next_open();
                MIMI_OK
            }
            Err(status) => status,
        });
    }
    // SAFETY: the caller's contract.
    let id = (!account.is_null()).then(|| unsafe { (*account).id });
    guard_account(id, || panic!("test hook: a panic inside the C ABI"))
}

#[cfg(test)]
mod tests {
    use purple_mimi_core::Status;

    use super::*;

    #[test]
    fn the_status_constants_are_the_cores_codes() {
        for (constant, status) in [
            (MIMI_OK, Status::Ok),
            (MIMI_ERR_ABI, Status::Abi),
            (MIMI_ERR_WRONG_THREAD, Status::WrongThread),
            (MIMI_ERR_INVALID_ARGUMENT, Status::InvalidArgument),
            (MIMI_ERR_NOT_CONNECTED, Status::NotConnected),
            (MIMI_ERR_NO_SUCH_ROOM, Status::NoSuchRoom),
            (MIMI_ERR_NO_SUCH_MESSAGE, Status::NoSuchMessage),
            (MIMI_ERR_NO_SUCH_INVITE, Status::NoSuchInvite),
            (MIMI_ERR_NO_SUCH_DEVICE, Status::NoSuchDevice),
            (MIMI_ERR_NOT_PERMITTED, Status::NotPermitted),
            (MIMI_ERR_REFUSED, Status::Refused),
            (MIMI_ERR_TOO_LARGE, Status::TooLarge),
            (MIMI_ERR_NETWORK, Status::Network),
            (MIMI_ERR_UNAUTHORIZED, Status::Unauthorized),
            (MIMI_ERR_STREAM_OFF, Status::StreamOff),
            (MIMI_ERR_WRONG_USER, Status::WrongUser),
            (MIMI_ERR_LOCKED, Status::Locked),
            (MIMI_ERR_TAKEN_OVER, Status::TakenOver),
            (MIMI_ERR_LOCK_LOST, Status::LockLost),
            (MIMI_ERR_STATE, Status::State),
            (MIMI_ERR_IO, Status::Io),
            (MIMI_ERR_PANIC, Status::Panic),
            (MIMI_ERR_POISONED, Status::Poisoned),
            (MIMI_ERR_INTERNAL, Status::Internal),
            (MIMI_ERR_NOT_INITIALIZED, Status::NotInitialized),
        ] {
            assert_eq!(constant, status.code(), "{status:?}");
        }
        assert!(!mimi_status_known(-1));
        assert!(!mimi_status_known(MIMI_ERR_NOT_INITIALIZED + 1));
    }

    #[test]
    fn strings_out_never_hold_nul() {
        let text = c_string("a\0b");
        // SAFETY: just made by c_string.
        let back = unsafe { CStr::from_ptr(text) }.to_str().unwrap().to_owned();
        // SAFETY: made by c_string, freed once.
        unsafe { mimi_string_free(text) };
        assert_eq!(back, "a\u{FFFD}b");
    }

    #[test]
    fn a_panic_in_a_guarded_body_is_a_status() {
        assert_eq!(guard(MIMI_ERR_PANIC, || panic!("inside")), MIMI_ERR_PANIC);
        assert_eq!(guard_account(None, || panic!("inside")), MIMI_ERR_PANIC);
    }
}
