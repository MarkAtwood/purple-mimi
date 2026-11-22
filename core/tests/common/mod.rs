// SPDX-License-Identifier: GPL-3.0-or-later
//! What the live tests share: a real mimid provider from mimid's
//! `acceptance_fixture --providers-only` (live stream on), a provider
//! that refuses the stream, and waiting for an account's events.
//!
//! `PURPLE_MIMI_FIXTURE` names the fixture binary, built from mimid at the
//! pinned revision (tests/live.sh does this). Each test starts its own
//! provider, so its tokens are fresh.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use purple_mimi_core::{AccountId, Event, EventKind, Process, Status};

/// A running fixture: alice's and bob's provider URL and tokens. The
/// providers stop when it is dropped.
pub struct Fixture {
    child: Child,
    _stdin: ChildStdin,
    pub url: String,
    pub alice: String,
    pub alice_tokens: Vec<String>,
    pub bob: String,
    pub bob_tokens: Vec<String>,
}

impl Fixture {
    /// Starts the fixture named by `PURPLE_MIMI_FIXTURE`.
    ///
    /// # Panics
    ///
    /// If the variable is not set or the fixture does not start.
    pub fn start() -> Self {
        let binary = std::env::var("PURPLE_MIMI_FIXTURE").expect(
            "PURPLE_MIMI_FIXTURE names mimid's acceptance_fixture binary; run tests/live.sh",
        );
        let mut child = Command::new(binary)
            .arg("--providers-only")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the fixture starts");
        let stdin = child.stdin.take().expect("stdin");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("the fixture's line");
        let json: serde_json::Value = serde_json::from_str(&line).expect("JSON");
        let tokens = |who: &str| -> Vec<String> {
            json[who]["tokens"]
                .as_array()
                .expect("tokens")
                .iter()
                .map(|t| t.as_str().expect("a token").to_owned())
                .collect()
        };
        Self {
            url: json["alice"]["url"].as_str().expect("url").to_owned(),
            alice: json["alice"]["user"].as_str().expect("user").to_owned(),
            alice_tokens: tokens("alice"),
            bob: json["bob"]["user"].as_str().expect("user").to_owned(),
            bob_tokens: tokens("bob"),
            child,
            _stdin: stdin,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A local HTTP server that answers every request `404 Not Found`, as a
/// mimid provider without `live_stream = true` answers the stream's
/// upgrade (docs/design/binary-live-stream.md §5.2). Its URL.
pub fn not_found_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    url
}

/// A URL where nothing listens.
pub fn closed_port_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    drop(listener);
    url
}

/// A private temporary directory, as libpurple's base directory.
pub fn base_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("0700");
    dir
}

/// Waits up to `limit` for an event of `account` that `want` accepts,
/// returning it and every event of that account before it. Panics on a
/// `Disconnected` that `want` does not accept.
pub fn wait_for(
    process: &Process,
    account: AccountId,
    limit: Duration,
    want: impl Fn(&EventKind) -> bool,
) -> (EventKind, Vec<EventKind>) {
    let start = Instant::now();
    let mut seen = Vec::new();
    while start.elapsed() < limit {
        match process.next_event() {
            Some(Event {
                account: Some(id),
                kind,
            }) if id == account => {
                if want(&kind) {
                    return (kind, seen);
                }
                if let EventKind::Disconnected { .. } = kind {
                    panic!("unexpected {kind:?} after {seen:?}");
                }
                seen.push(kind);
            }
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    panic!("no wanted event in {limit:?}; saw {seen:?}");
}

/// Waits for `account` to connect.
pub fn connected(process: &Process, account: AccountId) -> Vec<EventKind> {
    wait_for(process, account, Duration::from_secs(60), |k| {
        matches!(k, EventKind::Connected)
    })
    .1
}

/// Waits for `account` to disconnect, giving the status and the text.
pub fn disconnected(process: &Process, account: AccountId, limit: Duration) -> (Status, String) {
    match wait_for(process, account, limit, |k| {
        matches!(k, EventKind::Disconnected { .. })
    })
    .0
    {
        EventKind::Disconnected { status, text, .. } => (status, text),
        _ => unreachable!("only Disconnected is wanted"),
    }
}
