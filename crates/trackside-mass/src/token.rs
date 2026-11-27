use zeroize::Zeroizing;

/// A Music Assistant long-lived access token. `Debug` never shows it, it has no `Display` or
/// `Serialize`, and its memory is zeroed when the last copy is dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(Zeroizing<String>);

impl Token {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into().trim().to_owned()))
    }

    /// The token itself. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::Token;

    #[test]
    fn debug_never_shows_the_token() {
        let token = Token::new("eyJhbGciOiJIUzI1NiJ9.secret-part");
        let shown = format!("{token:?} {token:#?}");
        assert!(!shown.contains("secret-part"), "{shown}");
        assert!(!shown.contains("eyJ"), "{shown}");
    }

    #[test]
    fn frame_dumps_from_the_websocket_library_are_compiled_out() {
        assert!(log::STATIC_MAX_LEVEL <= log::LevelFilter::Debug);
    }

    #[test]
    fn surrounding_whitespace_from_a_paste_is_dropped() {
        assert_eq!(Token::new("  abc\n").expose(), "abc");
    }
}
