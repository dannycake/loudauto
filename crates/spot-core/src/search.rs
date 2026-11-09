//! Search goes through Spotify's context resolver as `spotify:search:<query>`.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::{Result, SpotError};

/// Escape everything except RFC 3986 unreserved characters.
const QUERY_WORD: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Build the `spotify:search:` context URI for free text.
pub fn search_context_uri(query: &str) -> Result<String> {
    let words: Vec<String> = query
        .split_whitespace()
        .map(|w| utf8_percent_encode(w, QUERY_WORD).to_string())
        .collect();
    if words.is_empty() {
        return Err(SpotError::EmptyQuery);
    }
    Ok(format!("spotify:search:{}", words.join("+")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_words_with_plus() {
        assert_eq!(
            search_context_uri("lucy bedroque").unwrap(),
            "spotify:search:lucy+bedroque"
        );
    }

    #[test]
    fn percent_encodes_reserved_characters() {
        // Raw `/` and `&` were rejected live ("invalid uri character"); encoded forms worked.
        assert_eq!(
            search_context_uri("AC/DC").unwrap(),
            "spotify:search:AC%2FDC"
        );
        assert_eq!(search_context_uri("R&B").unwrap(), "spotify:search:R%26B");
    }

    #[test]
    fn percent_encodes_utf8() {
        assert_eq!(search_context_uri("Ø").unwrap(), "spotify:search:%C3%98");
    }

    #[test]
    fn collapses_whitespace() {
        assert_eq!(
            search_context_uri("  a   b ").unwrap(),
            "spotify:search:a+b"
        );
    }

    #[test]
    fn whitespace_only_is_empty_query() {
        assert!(matches!(
            search_context_uri(" \t "),
            Err(SpotError::EmptyQuery)
        ));
    }
}
