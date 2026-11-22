// SPDX-License-Identifier: GPL-3.0-or-later
//! Accounts against a real mimid provider with the live stream on
//! (docs/design/purple-mimi.md §6, §7, §11): connecting, the stored token,
//! the errors the user sees, and the state lock with its takeover, within
//! one process and across processes.
//!
//! Ignored by default: they need mimid's acceptance fixture. Run them
//! with tests/live.sh.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::{Fixture, base_dir, connected, disconnected, wait_for};
use purple_mimi_core::{
    AccountConfig, EventKind, Process, ProcessOptions, Status, Timings, Token, handle,
};

const IGNORED: &str = "needs mimid's acceptance fixture: tests/live.sh";

fn timings() -> Timings {
    Timings {
        poll: Duration::from_millis(50),
        wait: Duration::from_secs(15),
        watch: Duration::from_millis(200),
        grace: Duration::from_secs(5),
        shutdown: Duration::from_secs(5),
    }
}

fn process() -> Process {
    Process::start(ProcessOptions::default().with_timings(timings())).expect("a process")
}

fn config(base: &Path, user: &str, url: &str, token: Option<&str>) -> AccountConfig {
    AccountConfig::new(base, user)
        .with_provider_url(Some(url.to_owned()))
        .with_token(token.and_then(Token::new))
        .with_key_packages(2)
}

fn state_dir(base: &Path, user: &str) -> std::path::PathBuf {
    handle::state_dir_for(base, user).expect("a user")
}

#[test]
#[ignore = "needs mimid's acceptance fixture: tests/live.sh"]
fn an_account_connects_keeps_its_token_and_reconnects_with_it() {
    let _ = IGNORED;
    let fixture = Fixture::start();
    let base = base_dir();
    let process = process();
    let handle_form = handle::handle_of(&fixture.alice).unwrap();
    let id = process
        .open_account(config(
            base.path(),
            &handle_form,
            &fixture.url,
            Some(&fixture.alice_tokens[0]),
        ))
        .unwrap();
    let before = connected(&process, id);
    let steps: Vec<u32> = before
        .iter()
        .filter_map(|k| match k {
            EventKind::Progress { step, steps, .. } => {
                assert_eq!(*steps, 3);
                Some(*step)
            }
            _ => None,
        })
        .collect();
    assert_eq!(steps, [1, 2, 3], "{before:?}");
    let info = process.account_info(id).unwrap();
    assert!(info.connected);
    assert_eq!(info.user_uri, fixture.alice);
    assert_eq!(info.fingerprint.as_ref().map(String::len), Some(16));
    let dir = state_dir(base.path(), &fixture.alice);
    let token_file = dir.join("provider-token");
    assert_eq!(
        std::fs::read_to_string(&token_file).unwrap().trim(),
        fixture.alice_tokens[0]
    );
    assert_eq!(
        std::fs::metadata(&token_file).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    assert_eq!(
        std::fs::metadata(dir.parent().unwrap()).unwrap().mode() & 0o777,
        0o700
    );
    process.close_account(id);
    // Closed by the glue: no event names it, and the lock is let go, so
    // the account opens again, with the stored token.
    let again = process
        .open_account(config(base.path(), &fixture.alice, &fixture.url, None))
        .unwrap();
    connected(&process, again);
    process.shutdown();
}

#[test]
#[ignore = "needs mimid's acceptance fixture: tests/live.sh"]
fn another_users_token_is_wrong_user_and_writes_no_state() {
    let fixture = Fixture::start();
    let base = base_dir();
    let process = process();
    let id = process
        .open_account(config(
            base.path(),
            &fixture.alice,
            &fixture.url,
            Some(&fixture.bob_tokens[0]),
        ))
        .unwrap();
    let (status, text) = disconnected(&process, id, Duration::from_secs(30));
    assert_eq!(status, Status::WrongUser, "{text}");
    assert!(text.contains(&fixture.bob), "{text}");
    let dir = state_dir(base.path(), &fixture.alice);
    assert!(!dir.join("mimid.sqlite").exists());
    assert!(!dir.join("provider-token").exists());
    process.shutdown();
}

#[test]
#[ignore = "needs mimid's acceptance fixture: tests/live.sh"]
fn a_refused_token_is_unauthorized() {
    let fixture = Fixture::start();
    let base = base_dir();
    let process = process();
    let id = process
        .open_account(config(
            base.path(),
            &fixture.alice,
            &fixture.url,
            Some("not-a-token"),
        ))
        .unwrap();
    let (status, text) = disconnected(&process, id, Duration::from_secs(30));
    assert_eq!(status, Status::Unauthorized, "{text}");
    process.shutdown();
}

#[test]
#[ignore = "needs mimid's acceptance fixture: tests/live.sh"]
fn no_token_given_or_stored_is_unauthorized() {
    let fixture = Fixture::start();
    let base = base_dir();
    let process = process();
    let id = process
        .open_account(config(base.path(), &fixture.alice, &fixture.url, None))
        .unwrap();
    let (status, text) = disconnected(&process, id, Duration::from_secs(10));
    assert_eq!(status, Status::Unauthorized);
    assert!(text.contains("as the password"), "{text}");
    process.shutdown();
}

#[test]
fn a_provider_without_the_stream_is_stream_off() {
    let base = base_dir();
    let process = process();
    let url = common::not_found_server();
    let id = process
        .open_account(config(
            base.path(),
            "alice@a.example",
            &url,
            Some("some-token"),
        ))
        .unwrap();
    let (status, text) = disconnected(&process, id, Duration::from_secs(30));
    assert_eq!(status, Status::StreamOff, "{text}");
    assert!(text.contains("live_stream = true"), "{text}");
    process.shutdown();
}

#[test]
fn an_unreachable_provider_is_a_temporary_network_error() {
    let base = base_dir();
    let process = process();
    let id = process
        .open_account(config(
            base.path(),
            "alice@a.example",
            &common::closed_port_url(),
            Some("some-token"),
        ))
        .unwrap();
    let (kind, _) = wait_for(&process, id, Duration::from_secs(30), |k| {
        matches!(k, EventKind::Disconnected { .. })
    });
    let EventKind::Disconnected {
        status, temporary, ..
    } = kind
    else {
        unreachable!()
    };
    assert_eq!(status, Status::Network);
    assert!(temporary);
    process.shutdown();
}

#[test]
fn a_bad_configuration_is_refused_before_anything_starts() {
    let process = process();
    for (base, user, url) in [
        ("/tmp/x", "alice", "https://a.example"),
        ("relative", "alice@a.example", "https://a.example"),
        ("/tmp/x", "alice@a.example", "http://a.example"),
        ("/tmp/x", "alice@a.example", "ftp://a.example"),
    ] {
        let failure = process
            .open_account(config(Path::new(base), user, url, Some("t")))
            .expect_err(user);
        assert_eq!(
            failure.status,
            Status::InvalidArgument,
            "{base} {user} {url}"
        );
    }
    process.shutdown();
}

#[test]
#[ignore = "needs mimid's acceptance fixture: tests/live.sh"]
fn a_second_instance_takes_over_and_the_first_does_not_take_it_back() {
    let fixture = Fixture::start();
    let base = base_dir();
    let first = process();
    let second = process();
    let token = &fixture.alice_tokens[0];
    let a = first
        .open_account(config(
            base.path(),
            &fixture.alice,
            &fixture.url,
            Some(token),
        ))
        .unwrap();
    connected(&first, a);
    let b = second
        .open_account(config(base.path(), &fixture.alice, &fixture.url, None))
        .unwrap();
    let before = connected(&second, b);
    assert!(
        before.iter().any(|k| matches!(k,
            EventKind::Progress { text, .. } if text.contains("Waiting for another instance"))),
        "{before:?}"
    );
    let (status, text) = disconnected(&first, a, Duration::from_secs(5));
    assert_eq!(status, Status::TakenOver, "{text}");
    assert!(!first.account_info(a).unwrap().connected);
    assert!(
        !state_dir(base.path(), &fixture.alice)
            .join("takeover")
            .exists(),
        "the request is gone once handed over"
    );
    first.close_account(a);
    // §6.6: the first process does not take it back by itself...
    let again = first
        .open_account(config(base.path(), &fixture.alice, &fixture.url, None))
        .unwrap();
    let (status, _) = disconnected(&first, again, Duration::from_secs(5));
    assert_eq!(status, Status::TakenOver);
    first.close_account(again);
    assert!(second.account_info(b).unwrap().connected, "untouched");
    // ...only when asked to.
    let back = first
        .open_account(config(base.path(), &fixture.alice, &fixture.url, None).with_take_over(true))
        .unwrap();
    connected(&first, back);
    let (status, _) = disconnected(&second, b, Duration::from_secs(5));
    assert_eq!(status, Status::TakenOver);
    second.shutdown();
    first.shutdown();
}

#[test]
#[ignore = "needs mimid's acceptance fixture: tests/live.sh"]
fn a_lock_file_removed_under_the_holder_stops_it() {
    let fixture = Fixture::start();
    let base = base_dir();
    let process = process();
    let id = process
        .open_account(config(
            base.path(),
            &fixture.alice,
            &fixture.url,
            Some(&fixture.alice_tokens[0]),
        ))
        .unwrap();
    connected(&process, id);
    std::fs::remove_file(state_dir(base.path(), &fixture.alice).join("lock")).unwrap();
    let (status, text) = disconnected(&process, id, Duration::from_secs(5));
    assert_eq!(status, Status::LockLost, "{text}");
    process.shutdown();
}

/// The other process of the cross-process tests: opens the account named
/// by `PURPLE_MIMI_HELPER_*`, prints `HELPER connected` and
/// `HELPER disconnected <code>`, and closes it when its stdin closes.
#[test]
#[ignore = "run by the cross-process tests"]
fn helper_process() {
    let Ok(base) = std::env::var("PURPLE_MIMI_HELPER_BASE") else {
        return;
    };
    let var = |name: &str| std::env::var(name).expect(name);
    let process = process();
    let id = process
        .open_account(config(
            Path::new(&base),
            &var("PURPLE_MIMI_HELPER_USER"),
            &var("PURPLE_MIMI_HELPER_URL"),
            std::env::var("PURPLE_MIMI_HELPER_TOKEN").ok().as_deref(),
        ))
        .unwrap();
    let (done, wait) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut sink = String::new();
        let _ = std::io::stdin().read_line(&mut sink);
        let _ = done.send(());
    });
    loop {
        if wait.try_recv().is_ok() {
            process.close_account(id);
            println!("HELPER closed");
            break;
        }
        match process.next_event() {
            Some(event) if event.account == Some(id) => match event.kind {
                EventKind::Connected => println!("HELPER connected"),
                EventKind::Disconnected { status, .. } => {
                    println!("HELPER disconnected {}", status.code());
                    break;
                }
                _ => {}
            },
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(10)),
        }
        std::io::stdout().flush().unwrap();
    }
    std::io::stdout().flush().unwrap();
    process.shutdown();
}

struct Helper {
    child: Child,
    lines: std::sync::mpsc::Receiver<String>,
}

impl Helper {
    fn start(base: &Path, user: &str, url: &str, token: Option<&str>) -> Self {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--ignored", "--exact", "helper_process", "--nocapture"])
            .env("PURPLE_MIMI_HELPER_BASE", base)
            .env("PURPLE_MIMI_HELPER_USER", user)
            .env("PURPLE_MIMI_HELPER_URL", url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(token) = token {
            command.env("PURPLE_MIMI_HELPER_TOKEN", token);
        }
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if let Some(rest) = line.strip_prefix("HELPER ") {
                    let _ = send.send(rest.to_owned());
                }
            }
        });
        Self { child, lines }
    }

    fn expect(&self, line: &str, limit: Duration) {
        let got = self.lines.recv_timeout(limit).expect(line);
        assert_eq!(got, line);
    }

    fn signal(&self, signal: &str) {
        let status = Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        assert!(status.success());
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .arg("-CONT")
            .arg(self.child.id().to_string())
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "needs mimid's acceptance fixture: tests/live.sh"]
fn another_process_hands_the_account_over_cleanly() {
    let fixture = Fixture::start();
    let base = base_dir();
    let helper = Helper::start(
        base.path(),
        &fixture.alice,
        &fixture.url,
        Some(&fixture.alice_tokens[0]),
    );
    helper.expect("connected", Duration::from_secs(60));
    let process = process();
    let id = process
        .open_account(config(base.path(), &fixture.alice, &fixture.url, None))
        .unwrap();
    connected(&process, id);
    helper.expect(
        &format!("disconnected {}", Status::TakenOver.code()),
        Duration::from_secs(5),
    );
    process.shutdown();
}

#[test]
#[ignore = "needs mimid's acceptance fixture: tests/live.sh"]
fn a_stopped_holder_times_the_newcomer_out_without_touching_the_database() {
    let fixture = Fixture::start();
    let base = base_dir();
    let helper = Helper::start(
        base.path(),
        &fixture.alice,
        &fixture.url,
        Some(&fixture.alice_tokens[0]),
    );
    helper.expect("connected", Duration::from_secs(60));
    let database = state_dir(base.path(), &fixture.alice).join("mimid.sqlite");
    let before = std::fs::metadata(&database).unwrap().modified().unwrap();
    helper.signal("STOP");
    let process = Process::start(ProcessOptions::default().with_timings(Timings {
        wait: Duration::from_secs(2),
        ..timings()
    }))
    .unwrap();
    let id = process
        .open_account(config(base.path(), &fixture.alice, &fixture.url, None))
        .unwrap();
    let (status, text) = disconnected(&process, id, Duration::from_secs(10));
    assert_eq!(status, Status::Locked, "{text}");
    assert_eq!(
        std::fs::metadata(&database).unwrap().modified().unwrap(),
        before,
        "the newcomer never wrote the database"
    );
    assert!(
        !state_dir(base.path(), &fixture.alice)
            .join("takeover")
            .exists(),
        "the newcomer's request is gone"
    );
    process.shutdown();
}

#[test]
#[ignore = "needs mimid's acceptance fixture: tests/live.sh"]
fn a_killed_holder_leaves_the_lock_free_at_once() {
    let fixture = Fixture::start();
    let base = base_dir();
    let helper = Helper::start(
        base.path(),
        &fixture.alice,
        &fixture.url,
        Some(&fixture.alice_tokens[0]),
    );
    helper.expect("connected", Duration::from_secs(60));
    helper.signal("KILL");
    std::thread::sleep(Duration::from_millis(200));
    let process = process();
    let id = process
        .open_account(config(base.path(), &fixture.alice, &fixture.url, None))
        .unwrap();
    let before = connected(&process, id);
    assert!(
        !before.iter().any(|k| matches!(k,
            EventKind::Progress { text, .. } if text.contains("Waiting"))),
        "no waiting: {before:?}"
    );
    process.shutdown();
}
