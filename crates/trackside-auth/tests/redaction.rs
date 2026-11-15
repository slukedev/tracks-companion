//! Secrets in URLs never reach logs, whatever shape the URL takes.

use proptest::prelude::*;
use trackside_auth::{redact_path, redact_uri};

proptest! {
    #[test]
    fn credentials_in_queries_are_redacted(
        secret in "[A-Za-z0-9_.-]{12,40}",
        name in prop::sample::select(vec!["mt", "MT", "t", "p", "apiKey", "api_key", "s"]),
        before in "[a-z]{1,6}=[a-z0-9]{0,6}",
        after in "[a-z]{1,6}=[a-z0-9]{0,6}",
    ) {
        let uri: http::Uri = format!("/rest/stream?{before}&{name}={secret}&{after}")
            .parse()
            .map_err(|e: http::uri::InvalidUri| TestCaseError::fail(e.to_string()))?;
        let logged = redact_uri(&uri);
        prop_assert!(!logged.contains(&secret), "{logged}");
    }

    #[test]
    fn tokens_in_page_paths_are_redacted(
        token in "[A-Za-z0-9_-]{16,48}",
        page in prop::sample::select(vec!["/invite/", "/reset/", "/api/v1/invites/", "/api/v1/password-resets/", "/base/invite/"]),
        tail in prop::sample::select(vec!["", "/accept"]),
    ) {
        let logged = redact_path(&format!("{page}{token}{tail}"));
        prop_assert!(!logged.contains(&token), "{logged}");
        prop_assert!(logged.ends_with(tail));
    }
}
