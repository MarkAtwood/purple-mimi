// SPDX-License-Identifier: GPL-3.0-or-later
//! The provider's bearer token, and its copy in the state directory
//! (docs/design/purple-mimi.md §12.3; decision PQ3).
//!
//! libpurple 2 writes a remembered password into `accounts.xml` in clear
//! text, so after the first connection the token lives in
//! `<state>/provider-token` (0600, beside `mimid.sqlite`, which holds the
//! keys) and the glue clears libpurple's copy. Later connections read it
//! from there; a token given by the user replaces it.
//!
//! # Limitations
//!
//! - The file is as safe as the state directory: anyone who can read
//!   `mimid.sqlite` can read it.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// The token file in the state directory.
pub const TOKEN_FILE: &str = "provider-token";

/// The longest token read from the file, in octets. INVENTED VALUE.
const MAX_TOKEN_LEN: usize = 4096;

/// A bearer token. Its `Debug` output is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// `text` without surrounding whitespace, or `None` if that is empty
    /// or longer than [`MAX_TOKEN_LEN`] octets.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty() && text.len() <= MAX_TOKEN_LEN).then(|| Self(text.to_owned()))
    }

    /// The token itself, for the stream's hello only.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// The token stored in state directory `dir`, or `None` if there is none.
///
/// # Errors
///
/// If the file exists and cannot be read.
pub fn read_stored(dir: &Path) -> std::io::Result<Option<Token>> {
    match std::fs::read_to_string(dir.join(TOKEN_FILE)) {
        Ok(text) => Ok(Token::new(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Stores `token` in state directory `dir`, mode 0600, replacing the file
/// whole (written aside, synced, then renamed).
///
/// # Errors
///
/// If it cannot be written.
pub fn store(dir: &Path, token: &Token) -> std::io::Result<()> {
    let path = dir.join(TOKEN_FILE);
    let aside = dir.join(format!("{TOKEN_FILE}.new"));
    match std::fs::remove_file(&aside) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&aside)?;
    file.write_all(token.expose().as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&aside, &path)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    #[test]
    fn a_stored_token_reads_back_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_stored(dir.path()).unwrap(), None);
        let token = Token::new("  secret-1\n").unwrap();
        store(dir.path(), &token).unwrap();
        assert_eq!(read_stored(dir.path()).unwrap(), Some(token));
        let mode = std::fs::metadata(dir.path().join(TOKEN_FILE))
            .unwrap()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        store(dir.path(), &Token::new("secret-2").unwrap()).unwrap();
        assert_eq!(
            read_stored(dir.path()).unwrap().unwrap().expose(),
            "secret-2"
        );
        assert!(!dir.path().join("provider-token.new").exists());
    }

    #[test]
    fn debug_output_hides_the_token() {
        let token = Token::new("canary-7f3a").unwrap();
        assert!(!format!("{token:?}").contains("canary"));
    }

    #[test]
    fn empty_and_oversized_tokens_are_none() {
        assert_eq!(Token::new(" \n"), None);
        assert_eq!(Token::new(&"x".repeat(MAX_TOKEN_LEN + 1)), None);
        assert!(Token::new(&"x".repeat(MAX_TOKEN_LEN)).is_some());
    }
}
