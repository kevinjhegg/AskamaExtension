//! What can be completed at the cursor: a filter after `|` (or after `{% filter`), or a tag
//! keyword at the start of a `{% %}` tag.

use std::ops::Range;

use askama_parser::Syntax;

use crate::outline::{self, Region};
use crate::tags::{self, TagKind};

/// The keywords that can start a `{% %}` tag.
pub const KEYWORDS: &[&str] = &[
    "block", "break", "call", "continue", "elif", "else", "endblock", "endcall", "endfilter", "endfor",
    "endif", "endlet", "endmacro", "endmatch", "endraw", "endset", "endwhen", "extends", "filter", "for",
    "if", "import", "include", "let", "macro", "match", "raw", "set", "when",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion {
    /// A filter name, replacing `replace`.
    Filter { replace: Range<usize> },
    /// A tag keyword, replacing `replace`. `closer` is the tag that closes the innermost
    /// region still open at the cursor, offered first.
    Keyword { replace: Range<usize>, closer: Option<&'static str> },
    /// A variable, bound in the template or from its struct, replacing `replace`.
    Variable { replace: Range<usize> },
    /// A member of `object`, the dotted path before the cursor (`loop`, `order.customer`),
    /// replacing `replace`.
    Member { object: String, replace: Range<usize> },
}

/// What to offer at `offset`, if anything.
pub fn at(source: &str, syntax: &Syntax<'_>, offset: usize) -> Option<Completion> {
    let offset = offset.min(source.len());
    let before = source.get(..offset)?;
    // Only the text before the cursor counts: the tag being typed is the last one, unclosed.
    let tags = tags::scan(before, syntax);
    let tag = tags.last().filter(|tag| !tag.closed && tag.kind != TagKind::Comment)?;

    let word_start = before.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_').len();
    let replace = word_start..offset;
    let typed = before[tag.inner.start.min(word_start)..word_start].trim_end();
    if inside_literal(before, tag.inner.start) {
        return None;
    }

    if typed.ends_with('|') && !typed.ends_with("||") {
        return Some(Completion::Filter { replace });
    }
    if tag.kind == TagKind::Block && typed == "filter" {
        return Some(Completion::Filter { replace });
    }
    if tag.kind == TagKind::Block && typed.is_empty() {
        let earlier = &source[..tag.range.start];
        let regions = outline::regions(earlier, &tags::scan(earlier, syntax));
        let closer = innermost_open(&regions, earlier.len()).and_then(|region| {
            if region.label.starts_with("set") { Some("endset") } else { region.kind.closer() }
        });
        return Some(Completion::Keyword { replace, closer });
    }
    if let Some(object_end) = before[..word_start].strip_suffix('.').map(str::len) {
        let object_start = before[..object_end].trim_end_matches(|c: char| c.is_alphanumeric() || c == '_' || c == '.').len();
        let object = &before[object_start..object_end];
        if object.is_empty() || object.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
            return None;
        }
        return Some(Completion::Member { object: object.to_owned(), replace });
    }
    // Anything else in an expression is a variable, unless it follows `::` (a path) or the
    // tag names something else (`{% block name %}`).
    let follows_member = before[..word_start].ends_with(':');
    let names_something = tag.kind == TagKind::Block && crate::navigation::NOT_EXPRESSIONS.contains(&tag.keyword(before));
    let starts_with_digit = before[word_start..].starts_with(|c: char| c.is_ascii_digit());
    if follows_member || names_something || starts_with_digit {
        return None;
    }
    Some(Completion::Variable { replace })
}

/// What to offer after a space typed at `offset`, which editors send as a trigger character.
/// Only where a name is expected next: right after an opening delimiter, after `|`, after a
/// keyword that takes an expression (`if`, `elif`, `in`, `match`, `when`, `filter`), or after
/// an operator, `(`, `[` or `,`. Elsewhere, as after a complete name, a space is just a space.
pub fn after_space(source: &str, syntax: &Syntax<'_>, offset: usize) -> Option<Completion> {
    let found = at(source, syntax, offset)?;
    let before = source.get(..offset)?;
    let tag = tags::scan(before, syntax).pop()?;
    let typed = before[tag.inner.start.min(offset)..].trim_end();
    let last_word = typed.rsplit(|c: char| !(c.is_alphanumeric() || c == '_')).next().unwrap_or("");
    let wants = typed.is_empty()
        || typed.ends_with(['=', '(', '[', ',', '!', '&', '|', '+', '-', '*', '/', '%', '<', '>'])
        || matches!(last_word, "if" | "elif" | "in" | "match" | "when" | "filter");
    wants.then_some(found)
}

/// Whether the end of `text` is inside a string or char literal that starts at or after
/// `from`, so the cursor is in the literal and nothing should be offered.
fn inside_literal(text: &str, from: usize) -> bool {
    let mut at = from;
    while at < text.len() {
        if let Some(end) = tags::literal_end(text, at) {
            // A literal cut off by the cursor runs to the end without its closing quote.
            if end >= text.len() && !text.ends_with(['"', '\'', '#']) {
                return true;
            }
            at = end;
        } else {
            at += text[at..].chars().next().map_or(1, char::len_utf8);
        }
    }
    false
}

/// The deepest region still open at `end`, the end of the text the regions were built from.
fn innermost_open(regions: &[Region], end: usize) -> Option<&Region> {
    let open = regions.iter().rev().find(|region| region.closer.is_none() && region.kind.closer().is_some() && region.range.end == end)?;
    innermost_open(&open.children, end).or(Some(open))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_syntax;

    /// The completion at the `$` in `marked`, with `$` removed.
    fn at_marker(marked: &str) -> (Option<Completion>, String) {
        let offset = marked.find('$').unwrap();
        let source = marked.replacen('$', "", 1);
        (at(&source, &default_syntax(), offset), source)
    }

    fn replaced(marked: &str) -> (String, String) {
        let (completion, source) = at_marker(marked);
        match completion {
            Some(Completion::Filter { replace }) => ("filter".to_owned(), source[replace].to_owned()),
            Some(Completion::Keyword { replace, .. }) => ("keyword".to_owned(), source[replace].to_owned()),
            Some(Completion::Variable { replace }) => ("variable".to_owned(), source[replace].to_owned()),
            Some(Completion::Member { object, replace }) => (format!("member of {object}"), source[replace].to_owned()),
            None => ("none".to_owned(), String::new()),
        }
    }

    #[test]
    fn offers_filters_after_a_pipe() {
        assert_eq!(replaced("{{ name|$ }}"), ("filter".to_owned(), String::new()));
        assert_eq!(replaced("{{ name | up$ }}"), ("filter".to_owned(), "up".to_owned()));
        assert_eq!(replaced("{% filter lo$ %}x{% endfilter %}"), ("filter".to_owned(), "lo".to_owned()));
    }

    #[test]
    fn offers_variables_in_expressions() {
        assert_eq!(replaced("{{ $ }}"), ("variable".to_owned(), String::new()));
        assert_eq!(replaced("{{ ord$ }}"), ("variable".to_owned(), "ord".to_owned()));
        assert_eq!(replaced("{% if a || b$ %}"), ("variable".to_owned(), "b".to_owned()));
        assert_eq!(replaced("{% for x in ite$ %}"), ("variable".to_owned(), "ite".to_owned()));
        assert_eq!(replaced("{{ order.$ }}"), ("member of order".to_owned(), String::new()));
        assert_eq!(replaced("{{ order.customer.na$ }}"), ("member of order.customer".to_owned(), "na".to_owned()));
        assert_eq!(replaced("{{ 1.$ }}").0, "none");
        assert_eq!(replaced("{{ m::$ }}").0, "none");
        assert_eq!(replaced("{% block ti$ %}").0, "none");
        assert_eq!(replaced("{{ 12$ }}").0, "none");
    }

    #[test]
    fn opens_completion_after_a_space_only_where_a_name_belongs() {
        let after = |marked: &str| {
            let offset = marked.find('$').unwrap();
            let source = marked.replacen('$', "", 1);
            match after_space(&source, &default_syntax(), offset) {
                Some(Completion::Filter { .. }) => "filter",
                Some(Completion::Keyword { .. }) => "keyword",
                Some(Completion::Variable { .. }) => "variable",
                Some(Completion::Member { .. }) => "member",
                None => "none",
            }
        };
        assert_eq!(after("{{ $}}"), "variable");
        assert_eq!(after("{%- $"), "keyword");
        assert_eq!(after("{% if $"), "variable");
        assert_eq!(after("{% for x in $"), "variable");
        assert_eq!(after("{{ a || $"), "variable");
        assert_eq!(after("{{ a| $"), "filter");
        assert_eq!(after("{% filter $"), "filter");
        assert_eq!(after("{{ f(a, $"), "variable");
        assert_eq!(after("{{ title $}}"), "none");
        assert_eq!(after("{% for $"), "none");
        assert_eq!(after("{% block $"), "none");
        assert_eq!(after("<p>Hello $"), "none");
    }

    #[test]
    fn offers_nothing_outside_tags() {
        assert_eq!(replaced("<p>a|b$</p>").0, "none");
        assert_eq!(replaced("{{ \"a|$\" }}").0, "none");
        assert_eq!(replaced("{# x|$ #}").0, "none");
    }

    #[test]
    fn offers_keywords_at_the_start_of_a_block_tag() {
        assert_eq!(replaced("{% fo$"), ("keyword".to_owned(), "fo".to_owned()));
        assert_eq!(replaced("{%- $"), ("keyword".to_owned(), String::new()));
        assert_eq!(replaced("{% for x$ in xs %}").0, "variable");
    }

    #[test]
    fn puts_the_innermost_closer_first() {
        let closer = |marked: &str| match at_marker(marked).0 {
            Some(Completion::Keyword { closer, .. }) => closer,
            other => panic!("{other:?}"),
        };
        assert_eq!(closer("{% block a %}{% for x in xs %}{% if x %}{% endif %}{% end$"), Some("endfor"));
        assert_eq!(closer("{% block a %}{% for x in xs %}{% endfor %}\n{% end$ %}\n{% endblock %}"), Some("endblock"));
        assert_eq!(closer("{% set x %}{% en$"), Some("endset"));
        assert_eq!(closer("{% extends \"a.html\" %}{% $"), None);
    }
}
