//! The names a template binds itself and where each is visible: `for` loop variables and the
//! loop's `loop`, `let` and `set`, macro arguments, `match` arms and `if let`.
//!
//! Only the names are known, not their types (steps 9 and 10 add those). Patterns are read
//! with a few rules rather than a Rust parser: a binding is a lowercase identifier that is not
//! a path segment, a constructor (`Some(x)`), a struct name or a field label (`name: n`).

use std::ops::Range;

use crate::navigation::{Ident, NOT_EXPRESSIONS, variable_at};
use crate::outline::{Region, RegionKind};
use crate::tags::{Tag, TagKind, literal_end};

/// What `loop` offers inside a `for` loop.
pub const LOOP_MEMBERS: &[(&str, &str)] = &[
    ("index", "The current iteration, counting from 1."),
    ("index0", "The current iteration, counting from 0."),
    ("first", "Whether this is the first iteration."),
    ("last", "Whether this is the last iteration."),
    ("cycle", "Takes a list and returns its items in turn, one per iteration: `loop.cycle([\"odd\", \"even\"])`."),
];

/// Words that are never variables in an expression.
const NOT_VARIABLES: &[&str] = &[
    "as", "caller", "defined", "else", "elif", "false", "for", "if", "in", "is", "let", "loop", "match", "mut", "not", "ref",
    "self", "set", "super", "true", "when", "with",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    /// A name in a `for` loop's pattern.
    LoopVariable,
    /// The `loop` value inside a `for` loop.
    Loop,
    Let,
    MacroArgument,
    /// A name in a `when` pattern.
    MatchArm,
    /// A name in an `if let` or `elif let` pattern.
    IfLet,
}

/// A name the template binds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub name: String,
    pub kind: BindingKind,
    /// Where the name is written. For `loop`, the `for` tag.
    pub range: Range<usize>,
    /// Where the name can be used.
    pub scope: Range<usize>,
    /// What gives it its value: the iterated or assigned expression, the macro's name, or
    /// the pattern of a `when` or `if let`.
    pub origin: String,
}

/// Every name the template binds.
pub fn bindings(source: &str, tags: &[Tag], regions: &[Region]) -> Vec<Binding> {
    let tag_at = |start: usize| tags.iter().find(|tag| tag.range.start == start);
    let mut found = Vec::new();

    for region in Region::walk(regions) {
        let Some(opener) = tag_at(region.opener.start) else {
            continue;
        };
        let body_end = region.branches.first().map(|branch| branch.start).or(region.closer.as_ref().map(|closer| closer.start)).unwrap_or(region.range.end);
        let body = opener.range.end..body_end;
        match region.kind {
            RegionKind::For => {
                let rest = rest_range(source, opener);
                let Some(in_at) = top_level_word(source, rest.clone(), "in") else {
                    continue;
                };
                let iterated = source[in_at + 2..rest.end].trim();
                let iterated = top_level_word(source, in_at + 2..rest.end, "if").map_or(iterated, |at| source[in_at + 2..at].trim());
                for (name, range) in pattern_names(source, rest.start..in_at) {
                    found.push(Binding { name, kind: BindingKind::LoopVariable, range, scope: body.clone(), origin: iterated.to_owned() });
                }
                found.push(Binding { name: "loop".to_owned(), kind: BindingKind::Loop, range: opener.range.clone(), scope: body.clone(), origin: iterated.to_owned() });
            }
            RegionKind::Macro => {
                let rest = rest_range(source, opener);
                let text = &source[rest.clone()];
                let name = text.trim_start().split(|c: char| !(c.is_alphanumeric() || c == '_')).next().unwrap_or("").to_owned();
                let (Some(open), Some(close)) = (text.find('('), text.rfind(')')) else {
                    continue;
                };
                for argument in split_top_level(source, rest.start + open + 1..rest.start + close, b',') {
                    // Each argument is `name`, `name = default` or `name: Type`.
                    if let Some((arg, range)) = leading_identifier(source, argument) {
                        found.push(Binding { name: arg, kind: BindingKind::MacroArgument, range, scope: body.clone(), origin: name.clone() });
                    }
                }
            }
            RegionKind::Match => {
                let boundaries: Vec<usize> = region.branches.iter().map(|branch| branch.start).chain(region.closer.as_ref().map(|closer| closer.start)).chain(std::iter::once(region.range.end)).collect();
                for (index, branch) in region.branches.iter().enumerate() {
                    let Some(tag) = tag_at(branch.start).filter(|tag| tag.keyword(source) == "when") else {
                        continue;
                    };
                    let pattern = rest_range(source, tag);
                    let scope = tag.range.end..boundaries[index + 1];
                    let origin = format!("when {}", source[pattern.clone()].trim());
                    for (name, range) in pattern_names(source, pattern) {
                        found.push(Binding { name, kind: BindingKind::MatchArm, range, scope: scope.clone(), origin: origin.clone() });
                    }
                }
            }
            RegionKind::If => {
                let branch_tags = std::iter::once(opener).chain(region.branches.iter().filter_map(|branch| tag_at(branch.start)));
                let boundaries: Vec<usize> = region.branches.iter().map(|branch| branch.start).chain(region.closer.as_ref().map(|closer| closer.start)).chain(std::iter::once(region.range.end)).collect();
                for (index, tag) in branch_tags.enumerate() {
                    let rest = rest_range(source, tag);
                    let text = &source[rest.clone()];
                    // `if let`, `elif let` and `else if let`.
                    let Some(let_at) = text.find("let ").filter(|&at| text[..at].trim().is_empty() || text[..at].trim() == "if") else {
                        continue;
                    };
                    let after_let = rest.start + let_at + 3;
                    let Some(equals) = top_level_assignment(source, after_let..rest.end) else {
                        continue;
                    };
                    let origin = format!("if let {}", source[after_let..rest.end].trim());
                    let scope = tag.range.end..boundaries[index];
                    for (name, range) in pattern_names(source, after_let..equals) {
                        found.push(Binding { name, kind: BindingKind::IfLet, range, scope: scope.clone(), origin: origin.clone() });
                    }
                }
            }
            _ => {}
        }
    }

    for tag in tags.iter().filter(|tag| tag.kind == TagKind::Block && matches!(tag.keyword(source), "let" | "set")) {
        let rest = rest_range(source, tag);
        let equals = top_level_assignment(source, rest.clone());
        // The block form binds its name after `endlet`.
        let start = match equals {
            Some(_) => tag.range.end,
            None => Region::walk(regions)
                .find(|region| region.kind == RegionKind::Let && region.opener.start == tag.range.start)
                .and_then(|region| region.closer.as_ref().map(|closer| closer.end))
                .unwrap_or(tag.range.end),
        };
        let pattern_end = equals.unwrap_or(rest.end);
        let pattern_end = top_level_type_colon(source, rest.start..pattern_end).unwrap_or(pattern_end);
        let origin = equals.map_or_else(|| "its block".to_owned(), |at| source[at + 1..rest.end].trim().to_owned());
        let scope = start..segment_end(source, regions, tag.range.start);
        for (name, range) in pattern_names(source, rest.start..pattern_end) {
            found.push(Binding { name, kind: BindingKind::Let, range, scope: scope.clone(), origin: origin.clone() });
        }
    }
    found
}

/// The bindings visible at `offset`, innermost first, each name once.
pub fn visible_at(bindings: &[Binding], offset: usize) -> Vec<&Binding> {
    let mut visible: Vec<&Binding> = bindings.iter().filter(|binding| binding.scope.start <= offset && offset <= binding.scope.end).collect();
    visible.sort_by_key(|binding| (binding.scope.len(), std::cmp::Reverse(binding.range.start)));
    let mut seen = std::collections::HashSet::new();
    visible.retain(|binding| seen.insert(binding.name.clone()));
    visible
}

/// The binding a variable at `offset` refers to, or the binding whose name is written there.
pub fn binding_at<'b>(source: &str, tags: &[Tag], bindings: &'b [Binding], offset: usize) -> Option<&'b Binding> {
    if let Some(binding) = bindings.iter().find(|binding| binding.kind != BindingKind::Loop && binding.range.start <= offset && offset <= binding.range.end) {
        return Some(binding);
    }
    let variable = variable_at(source, tags, offset)?;
    visible_at(bindings, variable.range.start).into_iter().find(|binding| binding.name == variable.name)
}

/// The `loop` member at `offset` (`index` in `loop.index`) inside a `for` loop, with its
/// description and range.
pub fn loop_member_at(source: &str, bindings: &[Binding], offset: usize) -> Option<(&'static str, &'static str, Range<usize>)> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let offset = offset.min(source.len());
    let start = source[..offset].trim_end_matches(is_word).len();
    let end = offset + source[offset..].find(|c: char| !is_word(c)).unwrap_or(source.len() - offset);
    let object = source[..start].strip_suffix("loop.")?;
    if object.ends_with(|c: char| is_word(c) || c == '.') {
        return None;
    }
    visible_at(bindings, offset).into_iter().find(|binding| binding.kind == BindingKind::Loop)?;
    let &(name, description) = LOOP_MEMBERS.iter().find(|(name, _)| *name == &source[start..end])?;
    Some((name, description, start..end))
}

/// Every place a binding is written or used, the binding itself first.
pub fn occurrences(source: &str, tags: &[Tag], bindings: &[Binding], binding: &Binding) -> Vec<Range<usize>> {
    let mut found = vec![binding.range.clone()];
    for used in variable_uses(source, tags, bindings) {
        if used.name == binding.name
            && binding.scope.start <= used.range.start
            && used.range.end <= binding.scope.end
            && visible_at(bindings, used.range.start).into_iter().find(|visible| visible.name == used.name) == Some(binding)
        {
            found.push(used.range);
        }
    }
    found
}

/// Every variable used in the template's expressions: each identifier that is a variable
/// (see `variable_at`), is not a word of the syntax, is not called (`f(` or `m!`), starts
/// lowercase, is not where a binding is written, and is not a named argument
/// (`m::cell(x, width = 3)`, where `width` names the macro's parameter).
pub fn variable_uses(source: &str, tags: &[Tag], bindings: &[Binding]) -> Vec<Ident> {
    let mut uses = Vec::new();
    for tag in tags.iter().filter(|tag| matches!(tag.kind, TagKind::Expr | TagKind::Block)) {
        if let Some(range) = expression_part(source, tag) {
            let mut at = range.start;
            while at < range.end {
                if let Some(end) = literal_end(source, at) {
                    at = end;
                    continue;
                }
                let rest = &source[at..range.end];
                let starts_word = rest.starts_with(|c: char| c.is_alphabetic() || c == '_')
                    && !source[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_');
                if !starts_word {
                    at += rest.chars().next().map_or(1, char::len_utf8);
                    continue;
                }
                let length = rest.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(rest.len());
                let word = &rest[..length];
                let after = source[at + length..range.end].trim_start();
                let used = word.starts_with(|c: char| c.is_lowercase() || c == '_')
                    && word != "_"
                    && !NOT_VARIABLES.contains(&word)
                    && !after.starts_with(['(', '!'])
                    && !(after.starts_with(':') && !after.starts_with("::"))
                    && !is_named_argument(after)
                    && !bindings.iter().any(|binding| binding.range.start == at)
                    && variable_at(source, tags, at).is_some_and(|variable| variable.range.start == at);
                if used {
                    uses.push(Ident { name: word.to_owned(), range: at..at + length });
                }
                at += length;
            }
        }
    }
    uses
}

/// Whether the text after a name makes it a named argument: a single `=`, not `==` or `=>`.
fn is_named_argument(after: &str) -> bool {
    after.starts_with('=') && !after.starts_with("==") && !after.starts_with("=>")
}

/// The part of a tag that holds an expression, where variables are used rather than bound.
pub fn expression_part(source: &str, tag: &Tag) -> Option<Range<usize>> {
    if tag.kind == TagKind::Expr {
        return Some(tag.inner.clone());
    }
    let keyword = tag.keyword(source);
    let rest = rest_range(source, tag);
    match keyword {
        _ if NOT_EXPRESSIONS.contains(&keyword) => None,
        "when" => None,
        "for" => top_level_word(source, rest.clone(), "in").map(|at| at + 2..rest.end),
        "let" | "set" => top_level_assignment(source, rest.clone()).map(|at| at + 1..rest.end),
        "if" | "elif" | "else" => {
            let text = &source[rest.clone()];
            match text.find("let ") {
                Some(let_at) if text[..let_at].trim().is_empty() || text[..let_at].trim() == "if" => {
                    top_level_assignment(source, rest.start + let_at + 3..rest.end).map(|at| at + 1..rest.end)
                }
                _ => Some(rest),
            }
        }
        _ => Some(rest),
    }
}

/// The range of a tag's text after its keyword.
fn rest_range(source: &str, tag: &Tag) -> Range<usize> {
    let start = tag.inner.start + tag.keyword(source).len();
    start.min(tag.inner.end)..tag.inner.end
}

/// Where the region segment holding `at` ends: the next branch or closing tag of the
/// innermost region whose body holds it, or the end of the text.
fn segment_end(source: &str, regions: &[Region], at: usize) -> usize {
    let holding = Region::walk(regions)
        .filter(|region| region.opener.end <= at && region.closer.as_ref().map_or(region.range.end, |closer| closer.start) > at)
        .min_by_key(|region| region.range.len());
    let Some(region) = holding else {
        return source.len();
    };
    region
        .branches
        .iter()
        .map(|branch| branch.start)
        .chain(region.closer.as_ref().map(|closer| closer.start))
        .chain(std::iter::once(region.range.end))
        .find(|&boundary| boundary > at)
        .unwrap_or(region.range.end)
}

/// The names a pattern binds, with their ranges.
fn pattern_names(source: &str, range: Range<usize>) -> Vec<(String, Range<usize>)> {
    let mut names = Vec::new();
    let mut at = range.start;
    while at < range.end {
        if let Some(end) = literal_end(source, at) {
            at = end;
            continue;
        }
        let rest = &source[at..range.end];
        let starts_word = rest.starts_with(|c: char| c.is_alphabetic() || c == '_') && !source[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_');
        if !starts_word {
            at += rest.chars().next().map_or(1, char::len_utf8);
            continue;
        }
        let length = rest.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(rest.len());
        let word = &rest[..length];
        let before = source[range.start..at].trim_end();
        let after = source[at + length..range.end].trim_start();
        let binds = word.starts_with(|c: char| c.is_lowercase() || c == '_')
            && word != "_"
            && !matches!(word, "ref" | "mut" | "true" | "false" | "with")
            && !before.ends_with("::")
            && !before.ends_with('.')
            && !after.starts_with(['(', '{', ':', '!']);
        if binds {
            names.push((word.to_owned(), at..at + length));
        }
        at += length;
    }
    names
}

/// The first identifier in `range`, after whitespace.
fn leading_identifier(source: &str, range: Range<usize>) -> Option<(String, Range<usize>)> {
    let text = &source[range.clone()];
    let start = range.start + (text.len() - text.trim_start().len());
    let rest = &source[start..range.end];
    if !rest.starts_with(|c: char| c.is_alphabetic() || c == '_') {
        return None;
    }
    let length = rest.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(rest.len());
    Some((rest[..length].to_owned(), start..start + length))
}

/// Where `word` appears as a whole word outside brackets and literals.
fn top_level_word(source: &str, range: Range<usize>, word: &str) -> Option<usize> {
    top_level_positions(source, range).find(|&at| {
        source[at..].starts_with(word)
            && !source[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_')
            && !source[at + word.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '_')
    })
}

/// Where a single `=` (not `==`, `!=`, `<=`, `>=` or `=>`) appears outside brackets.
fn top_level_assignment(source: &str, range: Range<usize>) -> Option<usize> {
    let bytes = source.as_bytes();
    top_level_positions(source, range).find(|&at| {
        bytes[at] == b'='
            && !matches!(at.checked_sub(1).map(|before| bytes[before]), Some(b'=' | b'!' | b'<' | b'>'))
            && !matches!(bytes.get(at + 1), Some(b'=' | b'>'))
    })
}

/// Where a type annotation's `:` (not `::`) appears outside brackets.
fn top_level_type_colon(source: &str, range: Range<usize>) -> Option<usize> {
    let bytes = source.as_bytes();
    top_level_positions(source, range).find(|&at| bytes[at] == b':' && bytes.get(at + 1) != Some(&b':') && (at == 0 || bytes[at - 1] != b':'))
}

/// `range` split at each top-level `separator`.
fn split_top_level(source: &str, range: Range<usize>, separator: u8) -> Vec<Range<usize>> {
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

/// The byte positions in `range` that are outside brackets and literals.
fn top_level_positions(source: &str, range: Range<usize>) -> impl Iterator<Item = usize> + '_ {
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
            at += source[here..].chars().next().map_or(1, char::len_utf8);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_syntax;
    use crate::outline::regions;
    use crate::tags::scan;

    fn analyze(source: &str) -> (Vec<Tag>, Vec<Binding>) {
        let tags = scan(source, &default_syntax());
        let bindings = bindings(source, &tags, &regions(source, &tags));
        (tags, bindings)
    }

    /// Each binding as its name, kind and the text it is visible over.
    fn sketch(source: &str) -> Vec<(String, BindingKind, String, String)> {
        let (_, bindings) = analyze(source);
        bindings.into_iter().map(|binding| (binding.name, binding.kind, source[binding.scope].to_owned(), binding.origin)).collect()
    }

    #[test]
    fn binds_loop_variables_and_loop() {
        let source = "{% for (key, value) in map.iter() if key != \"x\" %}A{% else %}B{% endfor %}";
        assert_eq!(
            sketch(source),
            [
                ("key".to_owned(), BindingKind::LoopVariable, "A".to_owned(), "map.iter()".to_owned()),
                ("value".to_owned(), BindingKind::LoopVariable, "A".to_owned(), "map.iter()".to_owned()),
                ("loop".to_owned(), BindingKind::Loop, "A".to_owned(), "map.iter()".to_owned()),
            ]
        );
    }

    #[test]
    fn binds_let_to_the_end_of_its_segment() {
        let source = "{% if a %}{% let x = 1 %}X{% else %}Y{% endif %}{% let (b, c): (u8, u8) = (1, 2) %}Z";
        assert_eq!(
            sketch(source),
            [
                ("x".to_owned(), BindingKind::Let, "X".to_owned(), "1".to_owned()),
                ("b".to_owned(), BindingKind::Let, "Z".to_owned(), "(1, 2)".to_owned()),
                ("c".to_owned(), BindingKind::Let, "Z".to_owned(), "(1, 2)".to_owned()),
            ]
        );
    }

    #[test]
    fn binds_block_let_after_its_block() {
        let source = "{% let title %}Hi{% endlet %}T";
        assert_eq!(sketch(source), [("title".to_owned(), BindingKind::Let, "T".to_owned(), "its block".to_owned())]);
    }

    #[test]
    fn binds_macro_arguments() {
        let source = "{% macro cell(cents, label = \"x\", width: u32) %}M{% endmacro %}";
        let names: Vec<(String, String)> = sketch(source).into_iter().map(|binding| (binding.0, binding.2)).collect();
        assert_eq!(names, [("cents".to_owned(), "M".to_owned()), ("label".to_owned(), "M".to_owned()), ("width".to_owned(), "M".to_owned())]);
    }

    #[test]
    fn binds_match_arms_and_if_let() {
        let source = "{% match item %}{% when Some(Item { name: n, size, .. }) %}A{% when None %}B{% endmatch %}{% if let Some(x) = y %}C{% else %}D{% endif %}";
        assert_eq!(
            sketch(source),
            [
                ("n".to_owned(), BindingKind::MatchArm, "A".to_owned(), "when Some(Item { name: n, size, .. })".to_owned()),
                ("size".to_owned(), BindingKind::MatchArm, "A".to_owned(), "when Some(Item { name: n, size, .. })".to_owned()),
                ("x".to_owned(), BindingKind::IfLet, "C".to_owned(), "if let Some(x) = y".to_owned()),
            ]
        );
    }

    #[test]
    fn shows_the_innermost_binding_of_a_name() {
        let source = "{% for x in a %}{% for x in b %}I{% endfor %}O{% endfor %}";
        let (_, bindings) = analyze(source);
        let inner = source.find('I').unwrap();
        let visible: Vec<(&str, &str)> = visible_at(&bindings, inner).into_iter().map(|binding| (binding.name.as_str(), binding.origin.as_str())).collect();
        assert_eq!(visible, [("x", "b"), ("loop", "b")]);
        let outer = source.find('O').unwrap();
        assert_eq!(visible_at(&bindings, outer)[0].origin, "a");
    }

    #[test]
    fn finds_uses_but_not_bindings_calls_or_syntax() {
        let source = "{% for item in items %}{{ item.name|truncate(width) }}{{ format!(\"x\") }}{{ f(x) }}{% if item is defined && !done %}{% endif %}{% endfor %}{% let n: u32 = count %}";
        let (tags, bindings) = analyze(source);
        let names: Vec<String> = variable_uses(source, &tags, &bindings).into_iter().map(|used| used.name).collect();
        assert_eq!(names, ["items", "item", "width", "x", "item", "done", "count"]);
    }

    #[test]
    fn does_not_read_named_arguments_as_variables() {
        let source = "{{ s::tab(\"x\", label ~ \"!\", checked = open, rounded=true) }}{% if a == b %}{% endif %}";
        let (tags, bindings) = analyze(source);
        let names: Vec<String> = variable_uses(source, &tags, &bindings).into_iter().map(|used| used.name).collect();
        assert_eq!(names, ["label", "open", "a", "b"]);
    }

    #[test]
    fn finds_every_occurrence_of_a_binding_but_not_a_shadowing_one() {
        let source = "{% for x in xs %}{{ x }}{% for x in ys %}{{ x }}{% endfor %}{{ x.y }}{% endfor %}{{ x }}";
        let (tags, bindings) = analyze(source);
        let outer = &bindings[0];
        let found: Vec<usize> = occurrences(source, &tags, &bindings, outer).into_iter().map(|range| range.start).collect();
        let positions: Vec<usize> = source.match_indices('x').map(|(at, _)| at).collect();
        // `for x` and `{{ x }}`, then, skipping the inner loop's `for x` and `{{ x }}`, `{{ x.y }}`.
        // `xs` is another name, and the last `{{ x }}` is outside the loop.
        assert_eq!(found, [positions[0], positions[2], positions[5]]);
    }

    #[test]
    fn describes_loop_members_inside_a_loop_only() {
        let source = "{% for x in xs %}{{ loop.index }}{% endfor %}{{ loop.first }}";
        let (_, bindings) = analyze(source);
        let inside = source.find("index").unwrap() + 1;
        assert_eq!(loop_member_at(source, &bindings, inside).map(|found| found.0), Some("index"));
        assert!(loop_member_at(source, &bindings, source.find("first").unwrap() + 1).is_none());
    }

    #[test]
    fn resolves_a_use_to_its_binding() {
        let source = "{% for item in items %}{{ item }}{% endfor %}";
        let (tags, bindings) = analyze(source);
        let used = source.rfind("item").unwrap() + 1;
        assert_eq!(binding_at(source, &tags, &bindings, used).map(|binding| binding.range.start), Some(source.find("item").unwrap()));
        assert!(binding_at(source, &tags, &bindings, source.find("items").unwrap() + 1).is_none());
    }
}
