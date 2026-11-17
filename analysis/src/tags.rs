//! Finding Askama's tags in a template: where each `{% %}`, `{{ }}` and `{# #}` starts and ends,
//! with the crate's delimiters.
//!
//! `askama_parser` records only where a tag's keyword is, not where a block ends, and it gives
//! nothing for a template with a syntax error. This scanner gives both: exact tag ranges, and
//! a best effort on a template that does not parse, so the outline survives mid-edit typing.
//! It understands only what it must to find a tag's end: string and char literals inside tags,
//! nested comments, and `raw` blocks, whose content is not scanned.

use std::ops::Range;

use askama_parser::Syntax;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagKind {
    /// `{% … %}`
    Block,
    /// `{{ … }}`
    Expr,
    /// `{# … #}`
    Comment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub kind: TagKind,
    /// From the opening delimiter to the end of the closing one, or to the end of the text
    /// for a tag that is never closed.
    pub range: Range<usize>,
    /// The text between the delimiters, without whitespace markers (`-`, `+`, `~`) and
    /// surrounding whitespace.
    pub inner: Range<usize>,
    pub closed: bool,
}

impl Tag {
    /// The tag's first word, such as `for` or `endblock`. Empty for a tag that starts with
    /// something else.
    pub fn keyword<'s>(&self, source: &'s str) -> &'s str {
        let inner = &source[self.inner.clone()];
        let length = inner.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(inner.len());
        &inner[..length]
    }

    /// The text after the keyword, trimmed.
    pub fn rest<'s>(&self, source: &'s str) -> &'s str {
        source[self.inner.clone()][self.keyword(source).len()..].trim()
    }
}

/// Every tag in the template, in order.
pub fn scan(source: &str, syntax: &Syntax<'_>) -> Vec<Tag> {
    let mut tags = Vec::new();
    let mut at = 0;
    while let Some((start, kind)) = next_open(source, at, syntax) {
        let (open, close) = delimiters(kind, syntax);
        let after_open = start + open.len();
        let (end, closed) = match kind {
            TagKind::Comment => comment_end(source, after_open, open, close),
            TagKind::Block | TagKind::Expr => tag_end(source, after_open, close),
        };
        let inner_end = if closed { end - close.len() } else { end };
        let tag = Tag { kind, range: start..end, inner: trim_markers(source, after_open..inner_end), closed };
        at = end;
        let is_raw = kind == TagKind::Block && closed && tag.keyword(source) == "raw";
        tags.push(tag);
        if is_raw {
            at = endraw_start(source, at, syntax);
        }
    }
    tags
}

/// The earliest opening delimiter at or after `from`. At equal positions the longer one wins,
/// in case a custom syntax makes one a prefix of another.
fn next_open(source: &str, from: usize, syntax: &Syntax<'_>) -> Option<(usize, TagKind)> {
    [TagKind::Block, TagKind::Expr, TagKind::Comment]
        .into_iter()
        .filter_map(|kind| {
            let open = delimiters(kind, syntax).0;
            source[from..].find(open).map(|offset| (from + offset, kind, open.len()))
        })
        .min_by_key(|&(start, _, length)| (start, std::cmp::Reverse(length)))
        .map(|(start, kind, _)| (start, kind))
}

fn delimiters<'s>(kind: TagKind, syntax: &Syntax<'s>) -> (&'s str, &'s str) {
    match kind {
        TagKind::Block => (syntax.block_start, syntax.block_end),
        TagKind::Expr => (syntax.expr_start, syntax.expr_end),
        TagKind::Comment => (syntax.comment_start, syntax.comment_end),
    }
}

/// The end of a block or expression tag: just past `close`, skipping string and char
/// literals, which may contain it.
fn tag_end(source: &str, from: usize, close: &str) -> (usize, bool) {
    let mut at = from;
    while at < source.len() {
        if source[at..].starts_with(close) {
            return (at + close.len(), true);
        }
        at = literal_end(source, at).unwrap_or_else(|| next_char(source, at));
    }
    (source.len(), false)
}

/// Just past the string or char literal that starts at `at`, or `None` if none starts there.
pub(crate) fn literal_end(source: &str, at: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let after_word = at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
    match bytes.get(at)? {
        b'"' => Some(string_end(bytes, at + 1)),
        b'r' if after_word => {
            let hashes = raw_string_hashes(bytes, at + 1)?;
            Some(raw_string_end(bytes, at + 2 + hashes, hashes))
        }
        b'\'' => {
            let end = char_end(bytes, at);
            (end > at + 1).then_some(end)
        }
        _ => None,
    }
}

/// Just past the closing quote of a string whose content starts at `from`.
fn string_end(bytes: &[u8], from: usize) -> usize {
    let mut at = from;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'"' => return at + 1,
            _ => at += 1,
        }
    }
    bytes.len()
}

/// The number of `#` after an `r` that starts a raw string (`r"`, `r#"`), or `None`.
fn raw_string_hashes(bytes: &[u8], from: usize) -> Option<usize> {
    let hashes = bytes[from..].iter().take_while(|&&byte| byte == b'#').count();
    (bytes.get(from + hashes) == Some(&b'"')).then_some(hashes)
}

fn raw_string_end(bytes: &[u8], from: usize, hashes: usize) -> usize {
    let mut at = from;
    while at < bytes.len() {
        if bytes[at] == b'"' && bytes[at + 1..].iter().take(hashes).filter(|&&byte| byte == b'#').count() == hashes {
            return (at + 1 + hashes).min(bytes.len());
        }
        at += 1;
    }
    bytes.len()
}

/// Just past a char literal starting at `at` (`'x'`, `'\n'`), or one byte on if it is not one.
fn char_end(bytes: &[u8], at: usize) -> usize {
    let after = if bytes.get(at + 1) == Some(&b'\\') { at + 3 } else { at + 2 };
    match bytes[after.min(bytes.len())..].iter().position(|&byte| byte == b'\'') {
        Some(offset) if offset <= 3 => after + offset + 1,
        _ => at + 1,
    }
}

fn next_char(source: &str, at: usize) -> usize {
    at + source[at..].chars().next().map_or(1, char::len_utf8)
}

/// The end of a comment, which may contain nested comments.
fn comment_end(source: &str, from: usize, open: &str, close: &str) -> (usize, bool) {
    let mut depth = 1;
    let mut at = from;
    loop {
        let next_open = source[at..].find(open).map(|offset| at + offset);
        let next_close = source[at..].find(close).map(|offset| at + offset);
        match (next_open, next_close) {
            (Some(o), Some(c)) if o < c => {
                depth += 1;
                at = o + open.len();
            }
            (_, Some(c)) => {
                depth -= 1;
                at = c + close.len();
                if depth == 0 {
                    return (at, true);
                }
            }
            (_, None) => return (source.len(), false),
        }
    }
}

/// Where the `endraw` tag after a `raw` tag starts, or the end of the text.
fn endraw_start(source: &str, from: usize, syntax: &Syntax<'_>) -> usize {
    let mut at = from;
    while let Some(offset) = source[at..].find(syntax.block_start) {
        let start = at + offset;
        let after_open = start + syntax.block_start.len();
        let (end, closed) = tag_end(source, after_open, syntax.block_end);
        let inner_end = if closed { end - syntax.block_end.len() } else { end };
        let inner = trim_markers(source, after_open..inner_end);
        let tag = Tag { kind: TagKind::Block, range: start..end, inner, closed };
        if tag.keyword(source) == "endraw" {
            return start;
        }
        at = after_open;
    }
    source.len()
}

fn trim_markers(source: &str, range: Range<usize>) -> Range<usize> {
    let is_marker = |byte: Option<&u8>| matches!(byte, Some(b'-' | b'+' | b'~'));
    let (mut start, mut end) = (range.start, range.end);
    if start < end && is_marker(source.as_bytes().get(start)) {
        start += 1;
    }
    if start < end && is_marker(source.as_bytes().get(end - 1)) {
        end -= 1;
    }
    let text = &source[start..end];
    let leading = text.len() - text.trim_start().len();
    let trailing = text.len() - text.trim_end().len();
    if leading == text.len() { start..start } else { start + leading..end - trailing }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_syntax;

    fn tags(source: &str) -> Vec<(TagKind, &str, &str)> {
        scan(source, &default_syntax())
            .into_iter()
            .map(|tag| (tag.kind, &source[tag.range.clone()], &source[tag.inner]))
            .collect()
    }

    #[test]
    fn finds_each_kind_of_tag() {
        assert_eq!(
            tags("<p>{% if x %}{{ y }}{# z #}{% endif %}</p>"),
            [
                (TagKind::Block, "{% if x %}", "if x"),
                (TagKind::Expr, "{{ y }}", "y"),
                (TagKind::Comment, "{# z #}", "z"),
                (TagKind::Block, "{% endif %}", "endif"),
            ]
        );
    }

    #[test]
    fn strips_whitespace_markers() {
        assert_eq!(tags("{%- if x -%}{{~ y +}}")[0].2, "if x");
        assert_eq!(tags("{%- if x -%}{{~ y +}}")[1].2, "y");
    }

    #[test]
    fn skips_delimiters_inside_literals() {
        assert_eq!(tags(r#"{{ "%} }}" }}"#)[0].1, r#"{{ "%} }}" }}"#);
        assert_eq!(tags(r##"{{ r#"}}"# }}"##)[0].1, r##"{{ r#"}}"# }}"##);
        assert_eq!(tags("{{ '}' }}")[0].1, "{{ '}' }}");
        assert_eq!(tags(r#"{{ "a\"}}" }}"#)[0].1, r#"{{ "a\"}}" }}"#);
    }

    #[test]
    fn nests_comments() {
        assert_eq!(tags("{# a {# b #} c #}x")[0].1, "{# a {# b #} c #}");
    }

    #[test]
    fn does_not_scan_raw_content() {
        let found = tags("{% raw %}{% if %}{{ x {% endraw %}{{ y }}");
        let kinds: Vec<&str> = found.iter().map(|tag| tag.1).collect();
        assert_eq!(kinds, ["{% raw %}", "{% endraw %}", "{{ y }}"]);
    }

    #[test]
    fn runs_an_unclosed_tag_to_the_end() {
        let found = scan("a {% if x", &default_syntax());
        assert_eq!(found[0].range, 2..9);
        assert!(!found[0].closed);
    }

    #[test]
    fn gives_keyword_and_rest() {
        let source = "{% for item in order.items %}";
        let tag = &scan(source, &default_syntax())[0];
        assert_eq!(tag.keyword(source), "for");
        assert_eq!(tag.rest(source), "item in order.items");
    }
}
