<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# purple-mimi

A libpurple protocol plugin for **MIMI** — the IETF's More Instant
Messaging Interoperability protocol — over **MLS** (RFC 9420), so that
Pidgin and the other programs built on libpurple can take part in MIMI
rooms.

The plugin embeds [mimid](https://github.com/MarkAtwood/mimid)'s agent.
It is not a client of a local daemon: the plugin **is** a MIMI device of
the user, and holds the MLS keys and the plaintext in the Pidgin process.

> **Status: early.** Step 4 of the plan: the plugin loads, an account
> connects and disconnects, and failures reach the user. There are no
> rooms, no messages and no user actions yet — those are the next two
> steps. Nothing here has been run against a real provider by anyone but
> its author's test fixtures.

## What works today

- The libpurple 2.14 plugin (`libpurple-mimi.so`) and the libpurple 3
  plugin (`libpurple3-mimi.so`), from one Rust static library.
- An account connects to a MIMI provider over mimid's live stream,
  reports its progress, and disconnects.
- Every failure is mapped to a libpurple connection error with a text for
  the user: no provider, a refused token, a provider without the live
  stream, a token belonging to another user, and the state-lock cases
  below.
- Only one program at a time may use an account's state. A second
  instance asks the first to let go and takes over cleanly; the first
  finishes its work, closes the database and releases the lock.

## Building

Needs a C compiler, CMake 3.20+, pkg-config, and the latest stable Rust.
`aws-lc-sys` needs CMake and a C compiler of its own at build time.

```sh
cmake -B build          # add -DPURPLE2=OFF or -DPURPLE3=OFF to skip one
cmake --build build
cmake --install build   # into libpurple's plugin directory
```

libpurple 2 is found through `pkg-config purple` (Debian:
`libpurple-dev`; Homebrew: `pidgin`), libpurple 3 through
`pkg-config purple-3`. Each plugin is skipped when its libpurple is
absent. `-DCARGO_OFFLINE=ON` builds from vendored crates with no network.

The two container images under `ci/` are what CI and the author build in:
`ci/debian13.Containerfile` for libpurple 2, and
`ci/purple3.Containerfile`, which builds libpurple 3 from the Pidgin 3
alpha tarball since no distribution packages it yet.

## Setting up an account

1. Get a bearer token for your user from your provider. With mimid that
   is `mimid admin ... token`.
2. Add an account of protocol **MIMI**. On libpurple 2 the username is
   your handle, `alice@a.example`, and the password is the token. On
   libpurple 3 both are account settings, because libpurple 3 has no
   account username.
3. Optionally set the provider URL; it defaults to `https://<domain>` of
   your handle.

After the first successful connection the token is kept in the account's
state directory and the copy libpurple remembers is cleared, so the
password field can be left empty afterwards.

State lives in one directory per account: `~/.purple/mimi/<handle>/` on
libpurple 2, and under `purple_data_dir()` on libpurple 3. It holds the
MLS keys, the message database and the attachment blobs, mode 0700.

## Security

The plugin holds MLS keys and plaintext in the Pidgin process, which is
the point: there is no separate daemon to trust. It also means a Pidgin
crash dump may contain message plaintext, and any other Pidgin plugin
shares the address space.

The provider is **not** trusted with message contents, which MLS
protects. It is trusted for delivery and ordering. By default mimid uses
MLS BasicCredentials, so a provider can claim any identity for its own
users unless they use did:webvh credentials; purple-mimi inherits that.
Never treat this as protection against a malicious provider.

## Licence

GPL-3.0-or-later; see `COPYING`. The plugin statically links mimid
(Apache-2.0), whose `NOTICE` is carried in `NOTICE` as Apache-2.0 §4(d)
requires, along with OpenMLS, SQLite, aws-lc, rustls and the rest. See
`NOTICE` and, at each release, `THIRD-PARTY-LICENSES`.

## Where the design lives

The design note is `docs/design/purple-mimi.md` in mimid, and work is
tracked as beads in mimid (epic `mimid-s4oy`). The C ABI is
`include/purple-mimi.h`, generated from `ffi/src/lib.rs` by cbindgen and
committed; `ci/check-header.sh` keeps it honest.
