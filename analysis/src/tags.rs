//! Finding Askama's tags in a template: where each `{% %}`, `{{ }}` and `{# #}` starts and ends, with the crate's delimiters.
//!
//! `askama_parser` records only where a tag's keyword is, not where a block ends, and it gives nothing for a template with a syntax error.
//! This scanner gives both: exact tag ranges, and a best effort on a template that does not parse, so the outline survives mid-edit typing.
//! It understands only what it must to find a tag's end: string and char literals inside tags, nested comments, and `raw` blocks, whose
//! content is not scanned.

use std::cmp::Reverse;
use std::ops::Range;

use askama_parser::Syntax;

/// Which of Askama's three kinds of tag a tag is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagKind {
    /// `{% … %}`
    Block,
    /// `{{ … }}`
    Expr,
    /// `{# … #}`
    Comment,
}

/// One tag in a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    /// Its kind.
    pub kind: TagKind,
    /// From the opening delimiter to the end of the closing one, or to the end of the text for a tag that is never closed.
    pub range: Range<usize>,
    /// The text between the delimiters, without whitespace markers (`-`, `+`, `~`) and surrounding whitespace.
    pub inner: Range<usize>,
    /// Whether the closing delimiter was found.
    pub closed: bool,
}

impl Tag {
    /// The tag's first word, such as `for` or `endblock`. Empty for a tag that starts with something else.
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
    let mut openers = Openers::new(syntax);
    while let Some((start, kind)) = openers.next(source, at) {
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

/// The three opening delimiters, with where each was last found. A delimiter's next position is searched for again only once the scan has
/// passed it, so a delimiter the template never uses is searched for once, not once per tag.
struct Openers<'s> {
    /// Each kind of tag with its opening delimiter.
    delimiters: [(TagKind, &'s str); 3],
    /// For each, what the last search for it found.
    found: [Found; 3],
}

/// What the last search for an opening delimiter found.
#[derive(Debug, Clone, Copy)]
enum Found {
    /// It has not been searched for.
    Unsearched,
    /// Its first occurrence at or after where the search started.
    At(usize),
    /// It does not occur again.
    Nowhere,
}

impl<'s> Openers<'s> {
    /// The opening delimiters of `syntax`, not yet searched for.
    fn new(syntax: &Syntax<'s>) -> Self {
        let delimiters = [TagKind::Block, TagKind::Expr, TagKind::Comment].map(|kind| (kind, delimiters(kind, syntax).0));
        Self { delimiters, found: [Found::Unsearched; 3] }
    }

    /// The earliest opening delimiter at or after `from`. At equal positions the longer one wins, in case a custom syntax makes one a
    /// prefix of another.
    fn next(&mut self, source: &str, from: usize) -> Option<(usize, TagKind)> {
        let mut best: Option<(usize, TagKind, usize)> = None;
        for ((kind, open), found) in self.delimiters.iter().zip(&mut self.found) {
            let at = match *found {
                Found::At(at) if at >= from => Some(at),
                Found::Nowhere => None,
                Found::Unsearched | Found::At(_) => {
                    let at = source.get(from..).and_then(|rest| rest.find(open)).map(|offset| from + offset);
                    *found = at.map_or(Found::Nowhere, Found::At);
                    at
                }
            };
            if let Some(at) = at
                && best.is_none_or(|(start, _, length)| (at, Reverse(open.len())) < (start, Reverse(length)))
            {
                best = Some((at, *kind, open.len()));
            }
        }
        best.map(|(start, kind, _)| (start, kind))
    }
}

/// The opening and closing delimiters of a kind of tag in `syntax`.
fn delimiters<'s>(kind: TagKind, syntax: &Syntax<'s>) -> (&'s str, &'s str) {
    match kind {
        TagKind::Block => (syntax.block_start, syntax.block_end),
        TagKind::Expr => (syntax.expr_start, syntax.expr_end),
        TagKind::Comment => (syntax.comment_start, syntax.comment_end),
    }
}

/// The end of a block or expression tag: just past `close`, skipping string and char literals, which may contain it.
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

/// Just past the closing quote and `hashes` hashes of a raw string whose content starts at `from`.
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

/// The offset of the character after the one at `at`, or `at + 1` at the end of the text.
pub(crate) fn next_char(source: &str, at: usize) -> usize {
    at + source.get(at..).and_then(|rest| rest.chars().next()).map_or(1, char::len_utf8)
}

/// The tag whose range holds `offset`, either end included. Tags are in order and do not overlap, so this is a binary search. Where one tag
/// ends as the next starts, the later one.
pub fn containing(tags: &[Tag], offset: usize) -> Option<&Tag> {
    let index = tags.partition_point(|tag| tag.range.start <= offset).checked_sub(1)?;
    let tag = &tags[index];
    (offset <= tag.range.end).then_some(tag)
}

/// The tag that starts at `start`, by binary search.
pub fn starting_at(tags: &[Tag], start: usize) -> Option<&Tag> {
    tags.binary_search_by_key(&start, |tag| tag.range.start).ok().map(|index| &tags[index])
}

/// The end of a comment, which may contain nested comments.
fn comment_end(source: &str, from: usize, open: &str, close: &str) -> (usize, bool) {
    let mut depth = 1;
    let mut at = from;
    while let Some(close_at) = source[at..].find(close).map(|offset| at + offset) {
        // A nested opener counts only before this closer, so the search stops there.
        if let Some(open_at) = source[at..close_at].find(open).map(|offset| at + offset) {
            depth += 1;
            at = open_at + open.len();
            continue;
        }
        depth -= 1;
        at = close_at + close.len();
        if depth == 0 {
            return (at, true);
        }
    }
    (source.len(), false)
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

/// `range` without a whitespace marker (`-`, `+`, `~`) at either end and without the whitespace inside it.
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
    fn finds_tags_when_a_delimiter_is_never_used() {
        // No `{#` anywhere: its one search finds nothing, and the other tags are still found.
        let found = tags("{{ a }}{% if b %}{{ c }}{% endif %}");
        assert_eq!(found.len(), 4);
        assert_eq!(found[2].1, "{{ c }}");
    }

    #[test]
    fn finds_the_tag_at_an_offset() {
        let source = "a {{ b }}{% c %} d";
        let tags = scan(source, &default_syntax());
        assert_eq!(containing(&tags, 3).map(|tag| tag.kind), Some(TagKind::Expr));
        assert_eq!(containing(&tags, source.find("{%").unwrap()).map(|tag| tag.kind), Some(TagKind::Block));
        assert_eq!(containing(&tags, 0), None);
        assert_eq!(containing(&tags, source.len() - 1), None);
        assert_eq!(starting_at(&tags, source.find("{%").unwrap()).map(|tag| tag.kind), Some(TagKind::Block));
        assert_eq!(starting_at(&tags, 3), None);
    }

    #[test]
    fn gives_keyword_and_rest() {
        let source = "{% for item in order.items %}";
        let tag = &scan(source, &default_syntax())[0];
        assert_eq!(tag.keyword(source), "for");
        assert_eq!(tag.rest(source), "item in order.items");
    }
}
