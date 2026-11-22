// SPDX-License-Identifier: GPL-3.0-or-later
//! Account names (docs/design/purple-mimi.md §5.3, §8.1; decision PQ4):
//! the handle `alice@a.example` that libpurple shows as the username, the
//! user URI `mimi://a.example/u/alice` that mimid uses, the provider URL
//! a user gets by default, and the state directory of an account.
//!
//! The handle form is INVENTED: `<id>@<domain>`, the domain being the
//! part after the last `@`. A domain never holds `@`, so an id that does
//! still maps back.
//!
//! # Limitations
//!
//! - Domains are ASCII host names with an optional port; internationalised
//!   domain names must be given in their ASCII (punycode) form.
//! - The state directory's name is the handle with every octet outside
//!   `A-Z a-z 0-9 . _ ~ + - @` percent-encoded, so two handles that differ
//!   only in ASCII case get two directories.

use std::path::{Path, PathBuf};

use mimid_agent_core::uri::{normalized_uri, user_uri_domain};

/// The directory under the libpurple base directory that holds one state
/// directory per account.
pub const STATE_PARENT: &str = "mimi";

/// Whether `domain` is an ASCII host name or address with an optional
/// port, as a provider domain must be to name a URL.
fn valid_domain(domain: &str) -> bool {
    let host = match domain.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            host
        }
        _ => domain,
    };
    !host.is_empty()
        && !host.starts_with(['.', '-'])
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
}

/// Whether `id` can be the id of a user URI: not empty, and no `/`, `?`,
/// `#`, whitespace or control character.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && !id.contains(['/', '?', '#'])
        && !id.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The user URI that `input` names: a handle (`alice@a.example`) or a user
/// URI (`mimi://a.example/u/alice`), normalised (RFC 3986 §6.2.2, as mimid
/// compares them). `None` if it is neither.
#[must_use]
pub fn user_uri_of(input: &str) -> Option<String> {
    let input = input.trim();
    if input.len() > 7 && input[..7].eq_ignore_ascii_case("mimi://") {
        let uri = normalized_uri(input).into_owned();
        let domain = user_uri_domain(&uri)?;
        let id = uri.rsplit_once("/u/")?.1;
        return (valid_domain(domain) && valid_id(id)).then_some(uri);
    }
    let (id, domain) = input.rsplit_once('@')?;
    if !valid_id(id) || !valid_domain(domain) {
        return None;
    }
    let uri = format!("mimi://{}/u/{id}", domain.to_ascii_lowercase());
    Some(normalized_uri(&uri).into_owned())
}

/// The handle of user URI `uri`, `<id>@<domain>`; `None` if `uri` is not a
/// user URI with a host-name domain.
#[must_use]
pub fn handle_of(uri: &str) -> Option<String> {
    let uri = user_uri_of(uri)?;
    let rest = uri.strip_prefix("mimi://")?;
    let (domain, path) = rest.split_once('/')?;
    let id = path.strip_prefix("u/")?;
    Some(format!("{id}@{domain}"))
}

/// The provider URL a user gets when the account sets none: `https://` and
/// the domain of the user URI (decision PQ4).
#[must_use]
pub fn default_provider_url(user: &str) -> Option<String> {
    let uri = user_uri_of(user)?;
    let domain = user_uri_domain(&uri)?;
    Some(format!("https://{domain}"))
}

/// Checks a provider URL: `https://` with a host, or `http://` to a
/// loopback host only, since the bearer token travels in the live stream's
/// hello (docs/design/purple-mimi.md §12.2; INVENTED rule).
///
/// # Errors
///
/// Why it is refused, as text for the user.
pub fn check_provider_url(url: &str) -> Result<(), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("{url} is not an https:// URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return Err(format!("{url} has no host, or has a user name in it"));
    }
    if scheme.eq_ignore_ascii_case("https") {
        return Ok(());
    }
    if scheme.eq_ignore_ascii_case("http") {
        let host = match authority.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or_default(),
            None => authority.split(':').next().unwrap_or_default(),
        };
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if loopback {
            return Ok(());
        }
        return Err(format!(
            "{url} is plain HTTP to another host; the token would be sent unencrypted. Use https://"
        ));
    }
    Err(format!("{url} is not an https:// URL"))
}

/// The file name a handle gets as a state directory: octets outside
/// `A-Z a-z 0-9 . _ ~ + - @` percent-encoded.
fn directory_name(handle: &str) -> String {
    let mut out = String::with_capacity(handle.len());
    for byte in handle.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'~' | b'+' | b'-' | b'@') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The state directory of user `user` (handle or URI) under libpurple's
/// base directory `base`: `<base>/mimi/<handle>`
/// (docs/design/purple-mimi.md §5.3). `None` if `user` names no user.
#[must_use]
pub fn state_dir_for(base: &Path, user: &str) -> Option<PathBuf> {
    let handle = handle_of(user)?;
    Some(base.join(STATE_PARENT).join(directory_name(&handle)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_and_uris_map_both_ways() {
        for (handle, uri) in [
            ("alice@a.example", "mimi://a.example/u/alice"),
            ("Bob@B.Example", "mimi://b.example/u/Bob"),
            ("x@localhost:8443", "mimi://localhost:8443/u/x"),
            ("a@b@c.example", "mimi://c.example/u/a@b"),
        ] {
            assert_eq!(user_uri_of(handle).as_deref(), Some(uri), "{handle}");
            assert_eq!(user_uri_of(uri).as_deref(), Some(uri), "{uri}");
        }
        assert_eq!(
            handle_of("MIMI://A.Example/u/alice").as_deref(),
            Some("alice@a.example")
        );
        assert_eq!(
            handle_of("mimi://c.example/u/a@b").as_deref(),
            Some("a@b@c.example")
        );
    }

    #[test]
    fn what_is_not_a_user_is_refused() {
        for bad in [
            "",
            "alice",
            "@a.example",
            "alice@",
            "al ice@a.example",
            "al/ice@a.example",
            "alice@a example",
            "alice@-a.example",
            "alice@a.example:",
            "alice@a.example:x",
            "alice\u{0}@a.example",
            "mimi://a.example/r/room",
            "mimi://a.example/u/",
            "mimi://a.example/u/a/b",
            "mimi:///u/alice",
            "https://a.example/u/alice",
        ] {
            assert_eq!(user_uri_of(bad), None, "{bad:?}");
            assert_eq!(handle_of(bad), None, "{bad:?}");
            assert_eq!(state_dir_for(Path::new("/b"), bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_default_provider_is_https_at_the_domain() {
        assert_eq!(
            default_provider_url("alice@A.example").as_deref(),
            Some("https://a.example")
        );
        assert_eq!(
            default_provider_url("mimi://a.example:8443/u/alice").as_deref(),
            Some("https://a.example:8443")
        );
    }

    #[test]
    fn plain_http_only_to_loopback() {
        for ok in [
            "https://a.example",
            "HTTPS://a.example/base",
            "http://127.0.0.1:8080",
            "http://[::1]:80/",
            "http://localhost:1",
        ] {
            assert_eq!(check_provider_url(ok), Ok(()), "{ok}");
        }
        for bad in [
            "http://a.example",
            "http://10.0.0.1",
            "ws://a.example",
            "a.example",
            "https://",
            "https://user@a.example",
        ] {
            assert!(check_provider_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn state_directories_are_one_file_name_under_mimi() {
        let base = Path::new("/home/u/.purple");
        assert_eq!(
            state_dir_for(base, "alice@a.example").unwrap(),
            base.join("mimi/alice@a.example")
        );
        assert_eq!(
            state_dir_for(base, "mimi://a.example/u/%C3%A9mile").unwrap(),
            base.join("mimi/%25C3%25A9mile@a.example")
        );
        assert_eq!(
            state_dir_for(base, "\u{e9}mile@a.example").unwrap(),
            base.join("mimi/%C3%A9mile@a.example")
        );
        let name = state_dir_for(base, "a:b\\c@a.example").unwrap();
        assert_eq!(name, base.join("mimi/a%3Ab%5Cc@a.example"));
    }
}
