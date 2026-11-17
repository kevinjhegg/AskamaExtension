//! Parsing one template with Askama's own parser.

use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use askama_parser::{Parsed, Syntax};

/// A mistake in a template: the byte range to mark and Askama's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub range: Range<usize>,
    pub message: String,
}

/// Parses a template. On failure, gives Askama's message on the word where parsing stopped.
/// The parser stops at the first mistake, so there is at most one.
pub fn parse(source: Arc<str>, syntax: &Syntax<'_>) -> Result<Parsed, Problem> {
    let text = Arc::clone(&source);
    // A panic in the parser must cost one diagnostic, not the language server.
    let parsed = catch_unwind(AssertUnwindSafe(|| Parsed::new(source, None, syntax)));
    match parsed {
        Ok(Ok(parsed)) => Ok(parsed),
        Ok(Err(error)) => Err(Problem {
            range: word_at(&text, error.offset),
            message: error
                .message
                .map_or_else(|| "failed to parse template source".to_owned(), |message| message.into_owned()),
        }),
        Err(_) => Err(Problem {
            range: 0..0,
            message: "askama_parser panicked on this template; please report it".to_owned(),
        }),
    }
}

/// The run of non-whitespace text starting at `offset`, so an error marks the word Askama
/// stopped at rather than a single point. Empty at the end of the text.
fn word_at(text: &str, offset: usize) -> Range<usize> {
    let mut start = offset.min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let rest = &text[start..];
    let length = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let length = if length == 0 { rest.chars().next().map_or(0, char::len_utf8) } else { length };
    start..start + length
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, default_syntax};

    fn check(source: &str) -> Result<(), Problem> {
        parse(Arc::from(source), &default_syntax()).map(|_| ())
    }

    fn marked(source: &str) -> (String, &str) {
        let problem = check(source).unwrap_err();
        (problem.message, &source[problem.range])
    }

    #[test]
    fn accepts_the_fixture_templates() {
        for template in ["base.html", "macros.html", "order.html", "partials/header.html"] {
            let path = format!("{}/../fixtures/sample-site/templates/{template}", env!("CARGO_MANIFEST_DIR"));
            let source = std::fs::read_to_string(&path).unwrap();
            assert_eq!(check(&source), Ok(()), "{template}");
        }
    }

    #[test]
    fn reports_a_missing_endcall_on_the_tag_found_instead() {
        let source = "{% for x in xs %}{% call m::cell(x) %}\n{% endfor %}";
        let (message, word) = marked(source);
        assert_eq!(message, "expected `endcall` to terminate `call` node, found `endfor`");
        assert_eq!(word, "endfor");
    }

    #[test]
    fn reports_an_unclosed_expression() {
        let (message, _) = marked("<p>{{ order.id </p>");
        assert!(message.contains("expression"), "{message}");
    }

    #[test]
    fn reports_an_unknown_tag() {
        let (_, word) = marked("{% frobnicate %}");
        assert!(word.starts_with("frobnicate"), "{word}");
    }

    #[test]
    fn marks_one_character_when_the_error_is_on_whitespace() {
        assert_eq!(word_at("ab cd", 2), 2..3);
        assert_eq!(word_at("ab", 2), 2..2);
        assert_eq!(word_at("é x", 1), 0..2);
    }

    #[test]
    fn uses_the_configured_syntax() {
        let source = "[general]\ndefault_syntax = \"square\"\n[[syntax]]\nname = \"square\"\nexpr_start = \"[[\"\nexpr_end = \"]]\"\n";
        let config = Config::parse(std::path::Path::new("/crate"), source).unwrap();
        assert!(parse(Arc::from("<p>[[ x ]]</p>"), &config.default_syntax()).is_ok());
        assert!(parse(Arc::from("<p>[[ x </p>"), &config.default_syntax()).is_err());
    }
}
