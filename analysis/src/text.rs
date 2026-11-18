//! Reading the Rust-like text inside tags: the words and operators that sit outside brackets and literals, which is where a tag's structure
//! (`in`, `=`, `,`, `:`) is found.
//!
//! These are rules, not a parser. They are shared by the scopes, the lowering and the outline, so all three read a tag the same way.

use std::ops::Range;

use crate::tags::{Tag, literal_end, next_char};

/// Whether `c` can be part of an identifier.
pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether an identifier can start at `at`: a letter or `_` there, and no identifier character just before it.
pub fn starts_word(source: &str, at: usize) -> bool {
    source[at..].starts_with(|c: char| c.is_alphabetic() || c == '_') && !source[..at].ends_with(is_word_char)
}

/// The length of the identifier at the start of `text`.
pub fn word_length(text: &str) -> usize {
    text.find(|c: char| !is_word_char(c)).unwrap_or(text.len())
}

/// The range of a tag's text after its keyword.
pub fn rest_range(source: &str, tag: &Tag) -> Range<usize> {
    let start = tag.inner.start + tag.keyword(source).len();
    start.min(tag.inner.end)..tag.inner.end
}

/// The byte positions in `range` that are outside brackets and literals, the brackets themselves excluded.
pub fn top_level_positions(source: &str, range: Range<usize>) -> impl Iterator<Item = usize> + '_ {
    let mut depth = 0usize;
    let mut at = range.start;
    std::iter::from_fn(move || {
        while at < range.end {
            if let Some(end) = literal_end(source, at) {
                at = end;
                continue;
            }
            let here = at;
            let byte = source.as_bytes()[here];
            at = next_char(source, here);
            match byte {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                _ if depth == 0 => return Some(here),
                _ => {}
            }
        }
        None
    })
}

/// Where `word` appears as a whole word outside brackets and literals.
pub fn top_level_word(source: &str, range: Range<usize>, word: &str) -> Option<usize> {
    top_level_positions(source, range).find(|&at| {
        source[at..].starts_with(word) && !source[..at].ends_with(is_word_char) && !source[at + word.len()..].starts_with(is_word_char)
    })
}

/// Where a single `=` (not `==`, `!=`, `<=`, `>=` or `=>`) appears outside brackets and literals.
pub fn top_level_assignment(source: &str, range: Range<usize>) -> Option<usize> {
    let bytes = source.as_bytes();
    top_level_positions(source, range).find(|&at| {
        bytes[at] == b'='
            && !matches!(at.checked_sub(1).map(|before| bytes[before]), Some(b'=' | b'!' | b'<' | b'>'))
            && !matches!(bytes.get(at + 1), Some(b'=' | b'>'))
    })
}

/// Where a type annotation's `:` (not `::`) appears outside brackets and literals.
pub fn top_level_type_colon(source: &str, range: Range<usize>) -> Option<usize> {
    let bytes = source.as_bytes();
    top_level_positions(source, range).find(|&at| bytes[at] == b':' && bytes.get(at + 1) != Some(&b':') && (at == 0 || bytes[at - 1] != b':'))
}

/// `range` split at each `separator` outside brackets and literals.
pub fn split_top_level(source: &str, range: Range<usize>, separator: u8) -> Vec<Range<usize>> {
    let mut parts = Vec::new();
    let mut start = range.start;
    for at in top_level_positions(source, range.clone()) {
        if source.as_bytes()[at] == separator {
            parts.push(start..at);
            start = at + 1;
        }
    }
    parts.push(start..range.end);
    parts
}

/// For a `{% call %}` tag: the range of its caller parameters (`a, b` in `{% call(a, b) m::row(x) %}`, the names the macro passes to its
/// body through `caller(...)`), if it has them, and where the called macro's name (or its import scope) starts.
pub fn call_parts(source: &str, tag: &Tag) -> (Option<Range<usize>>, usize) {
    let rest = rest_range(source, tag);
    let text = &source[rest.clone()];
    let start = rest.start + (text.len() - text.trim_start().len());
    if source[start..rest.end].starts_with('(')
        && let Some(close) = source[start..rest.end].find(')')
    {
        let after = start + close + 1;
        let tail = &source[after..rest.end];
        return (Some(start + 1..start + close), after + (tail.len() - tail.trim_start().len()));
    }
    (None, start)
}

/// Where the chain of names, `.`s and bracketed groups that ends at `end` starts: `order.items.first()` in `{{ order.items.first().`.
/// Brackets are matched by counting, without looking inside literals. The chain is empty when it does not start with a name.
pub fn chain_start(text: &str, end: usize) -> usize {
    let mut at = end;
    let mut depth = 0usize;
    while let Some(c) = text[..at].chars().next_back() {
        match c {
            ')' | ']' => depth += 1,
            '(' | '[' if depth > 0 => depth -= 1,
            _ if depth > 0 => {}
            c if is_word_char(c) || c == '.' => {}
            _ => break,
        }
        at -= c.len_utf8();
    }
    if depth > 0 {
        return end;
    }
    // A chain starts at a name, not at a bracket or a dot.
    while at < end && !text[at..].starts_with(|c: char| c.is_alphabetic() || c == '_') {
        let Some(c) = text[at..].chars().next() else {
            break;
        };
        if c.is_ascii_digit() || matches!(c, '(' | '[' | '.') {
            return end;
        }
        at += c.len_utf8();
    }
    at
}

/// For `if let p = e`, `elif let p = e` or `else if let p = e`: the range of the pattern and the range of the value. `None` for any other
/// tag.
pub fn if_let(source: &str, tag: &Tag) -> Option<(Range<usize>, Range<usize>)> {
    let rest = rest_range(source, tag);
    let text = &source[rest.clone()];
    let let_at = text.find("let ").filter(|&at| text[..at].trim().is_empty() || text[..at].trim() == "if")?;
    let after_let = rest.start + let_at + 3;
    let equals = top_level_assignment(source, after_let..rest.end)?;
    Some((after_let..equals, equals + 1..rest.end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_syntax;
    use crate::tags::scan;

    #[test]
    fn finds_structure_outside_brackets_and_literals() {
        let source = "x = f(a = 1, \"=\") == b";
        assert_eq!(top_level_assignment(source, 0..source.len()), Some(2));
        assert_eq!(top_level_assignment("a == b => c", 0..11), None);
        assert_eq!(split_top_level("a, f(b, c), \",\"", 0..15, b','), [0..1, 2..10, 11..15]);
        assert_eq!(top_level_word("item in items", 0..13, "in"), Some(5));
        assert_eq!(top_level_word("index in items", 0..14, "in"), Some(6));
        assert_eq!(top_level_type_colon("x: a::B", 0..7), Some(1));
    }

    #[test]
    fn reads_a_call_with_and_without_caller_parameters() {
        let source = "{% call(user, i) m::row(x) %}{% call  row(y) %}";
        let tags = scan(source, &default_syntax());
        let (parameters, name) = call_parts(source, &tags[0]);
        assert_eq!(parameters.map(|range| &source[range]), Some("user, i"));
        assert!(source[name..].starts_with("m::row"));
        let (parameters, name) = call_parts(source, &tags[1]);
        assert_eq!(parameters, None);
        assert!(source[name..].starts_with("row(y)"));
    }

    #[test]
    fn finds_where_a_chain_with_calls_starts() {
        fn start(text: &str) -> &str {
            &text[chain_start(text, text.len())..]
        }
        assert_eq!(start("{{ order.items.first()"), "order.items.first()");
        assert_eq!(start("{{ a.get(i + 1)[0].b"), "a.get(i + 1)[0].b");
        assert_eq!(start("{{ x + order.total"), "order.total");
        assert_eq!(start("{{ (a + b)"), "");
        assert_eq!(start("{{ 1.5"), "");
        assert_eq!(start("{{ a.b())"), "");
    }

    #[test]
    fn reads_if_let() {
        let source = "{% if let Some(x) = y %}{% elif let Ok(z) = w %}{% if a == b %}";
        let tags = scan(source, &default_syntax());
        let parts = |index: usize| if_let(source, &tags[index]).map(|(pattern, value)| (source[pattern].trim(), source[value].trim()));
        assert_eq!(parts(0), Some(("Some(x)", "y")));
        assert_eq!(parts(1), Some(("Ok(z)", "w")));
        assert_eq!(parts(2), None);
    }
}
