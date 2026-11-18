//! Template constructs as the Rust `askama_derive` 0.16 generates for them, for probes that need the names a template binds.
//!
//! What the generated code does, and what a probe writes instead:
//!
//! | Template | Generated | Probe |
//! |---|---|---|
//! | `{% for p in e %}` | `for (p, _) in TemplateLoop::new(ITER)` | `for p in ITER {` |
//! | `{% let p = e %}` | `let p = &(e);`, or `= e;` for a local or a literal | the same |
//! | `{% if let p = e %}` | `if let p = e {` | the same |
//! | `{% match e %}{% when p %}` | `match &e { p => {` | `if let p = &e {` |
//! | macro argument `a` given `v` | `let a = &(v);`, or `= v;` for a literal | the same |
//!
//! `ITER` is `e` for a range, `e.iter()` for an array, `(e).into_iter()` for a call or an index, `(&e).into_iter()` for a field of `self`
//! or of anything else, and `(e).into_iter()` for a local. Inside expressions a name the template does not bind becomes `self.name`, as
//! Askama's generated code reads it from the struct.

use std::ops::Range;

use crate::outline::{Region, RegionKind};
use crate::scopes::{Binding, BindingKind, visible_at};
use crate::tags::{self, Tag, TagKind, literal_end, next_char};
use crate::text::{call_parts, if_let, rest_range as rest, split_top_level, starts_word, top_level_assignment as assignment, top_level_word as word_at, word_length};

/// A template and what is known of it, for lowering.
pub struct Template<'a> {
    /// The template's text.
    pub source: &'a str,
    /// Its tags.
    pub tags: &'a [Tag],
    /// Its regions.
    pub regions: &'a [Region],
    /// The names it binds.
    pub bindings: &'a [Binding],
    /// Whether a name is a field or method of the template's struct.
    pub is_member: &'a dyn Fn(&str) -> bool,
}

/// Rust statements that bring the template's names into scope, and how many blocks they open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Setup {
    /// The statements, one per line, indented for the probe's function body.
    pub code: String,
    /// How many of them open a block that the probe must close.
    pub open: usize,
}

impl Setup {
    /// Adds a statement, which opens a block if `opens`.
    fn push(&mut self, line: &str, opens: bool) {
        self.code.push_str("        ");
        self.code.push_str(line);
        self.code.push('\n');
        self.open += usize::from(opens);
    }

    /// Adds the statements of `other` after these.
    pub fn extend(&mut self, other: &Self) {
        self.code.push_str(&other.code);
        self.open += other.open;
    }
}

impl Template<'_> {
    /// The statements that bring into scope what is visible at `offset`, from the constructs that start at or after `from`, outermost
    /// first. `None` if one of them cannot be written in Rust (an expression with a filter, for one).
    pub fn setup(&self, offset: usize, from: usize) -> Option<Setup> {
        let mut steps: Vec<(usize, String, bool)> = Vec::new();
        let tag_at = |start: usize| tags::starting_at(self.tags, start);
        for region in Region::walk(self.regions).filter(|region| region.opener.start >= from) {
            let Some(opener) = tag_at(region.opener.start) else {
                continue;
            };
            let segments = segments(region);
            let Some(index) = segments.iter().position(|segment| segment.start <= offset && offset <= segment.end) else {
                continue;
            };
            match region.kind {
                RegionKind::For if index == 0 => {
                    let rest = rest(self.source, opener);
                    let in_at = word_at(self.source, rest.clone(), "in")?;
                    let iter_end = word_at(self.source, in_at + 2..rest.end, "if").unwrap_or(rest.end);
                    let pattern = self.source[rest.start..in_at].trim();
                    let iterated = self.iterated(in_at + 2..iter_end, opener.range.start)?;
                    steps.push((opener.range.start, format!("for {pattern} in {iterated} {{"), true));
                }
                RegionKind::If => {
                    let branch = if index == 0 { opener } else { tag_at(region.branches[index - 1].start)? };
                    if let Some((pattern, value)) = if_let(self.source, branch) {
                        let pattern = self.source[pattern].trim();
                        let value = self.expression(value, branch.range.start)?;
                        steps.push((branch.range.start, format!("if let {pattern} = {value} {{"), true));
                    }
                }
                RegionKind::Match if index > 0 => {
                    let arm = tag_at(region.branches[index - 1].start).filter(|tag| tag.keyword(self.source) == "when")?;
                    let value = self.expression(rest(self.source, opener), opener.range.start)?;
                    let pattern = self.source[rest(self.source, arm)].trim();
                    steps.push((arm.range.start, format!("if let {pattern} = &({value}) {{"), true));
                }
                _ => {}
            }
        }
        for tag in self.tags.iter().filter(|tag| tag.range.start >= from && tag.kind == TagKind::Block && matches!(tag.keyword(self.source), "let" | "set")) {
            let binds_here = self
                .bindings
                .iter()
                .any(|binding| binding.kind == BindingKind::Let && tag.range.contains(&binding.range.start) && binding.scope.start <= offset && offset <= binding.scope.end);
            if !binds_here {
                continue;
            }
            let rest = rest(self.source, tag);
            let line = match assignment(self.source, rest.clone()) {
                Some(equals) => {
                    let pattern = self.source[rest.start..equals].trim();
                    let value = self.expression(equals + 1..rest.end, tag.range.start)?;
                    let borrow = !self.is_local_or_literal(self.source[equals + 1..rest.end].trim(), tag.range.start);
                    if borrow { format!("let {pattern} = &({value});") } else { format!("let {pattern} = {value};") }
                }
                // The block form holds the rendered block.
                None => format!("let {}: String = String::new();", self.source[rest].trim()),
            };
            steps.push((tag.range.start, line, false));
        }
        steps.sort_by_key(|(at, _, _)| *at);
        let mut setup = Setup::default();
        for (_, line, opens) in steps {
            setup.push(&line, opens);
        }
        Some(setup)
    }

    /// The `let` statements a macro call makes of its arguments, as the macro's body sees them. `arguments` is the text inside the call's
    /// parentheses, at `range` in this template, and `parameters` the macro's own argument names in order.
    pub fn macro_arguments(&self, range: Range<usize>, parameters: &[String]) -> Option<Setup> {
        let mut setup = Setup::default();
        for (index, argument) in split_top_level(self.source, range.clone(), b',').into_iter().enumerate() {
            let text = self.source[argument.clone()].trim();
            if text.is_empty() {
                continue;
            }
            // A named argument, `name = value`, or the next positional one.
            let (name, value) = match assignment(self.source, argument.clone()) {
                Some(equals) => (self.source[argument.start..equals].trim().to_owned(), equals + 1..argument.end),
                None => (parameters.get(index)?.clone(), argument),
            };
            let code = self.expression(value.clone(), range.start)?;
            let borrow = !is_literal(self.source[value].trim());
            setup.push(&if borrow { format!("let {name} = &({code});") } else { format!("let {name} = {code};") }, false);
        }
        Some(setup)
    }

    /// An expression in Rust: each name the template does not bind (as seen at `at`) and the struct has becomes `self.name`. Askama-only
    /// syntax is made Rust of the same length or cut: the value before a final filter chain stands for it (`tab.grid|safe` is `tab.grid`),
    /// and `~` becomes `+`, which keeps the operands' types checked. `None` for what cannot be: a filter inside brackets, or `is defined`.
    pub fn expression(&self, range: Range<usize>, at: usize) -> Option<String> {
        self.mapped_expression(range, at).map(|mapped| mapped.code)
    }

    /// Like `expression`, keeping where each part of the code came from in the template.
    pub fn mapped_expression(&self, range: Range<usize>, at: usize) -> Option<Mapped> {
        let source = self.source;
        let visible = visible_at(self.bindings, at);
        let mut code = String::new();
        let mut inserts = Vec::new();
        let mut cursor = range.start;
        let mut position = range.start;
        let mut end = range.end;
        let mut depth = 0usize;
        while position < end {
            if let Some(literal) = literal_end(source, position) {
                position = literal.min(end);
                continue;
            }
            let rest = &source[position..end];
            if rest.starts_with('|') && !rest.starts_with("||") && !source[..position].ends_with('|') {
                if depth > 0 {
                    return None;
                }
                // The filters apply to everything before them; the value is what is checked.
                end = position;
                break;
            }
            if rest.starts_with("is ") && source[..position].ends_with(' ') {
                return None;
            }
            if rest.starts_with('~') {
                // Concatenation: `+` keeps the length, so positions still map back.
                code.push_str(&source[cursor..position]);
                code.push('+');
                position += 1;
                cursor = position;
                continue;
            }
            if !starts_word(source, position) {
                match rest.as_bytes()[0] {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                    _ => {}
                }
                position = next_char(source, position);
                continue;
            }
            let length = word_length(rest);
            let word = &rest[..length];
            let before = source[range.start..position].trim_end();
            // `a.name` is a member, but `a..name` is a range up to a variable.
            let is_path_or_member = (before.ends_with('.') && !before.ends_with("..")) || before.ends_with("::") || source[position + length..end].trim_start().starts_with("::");
            let bound = visible.iter().any(|binding| binding.name == word);
            if !is_path_or_member && !bound && (self.is_member)(word) {
                code.push_str(&source[cursor..position]);
                inserts.push(code.len());
                code.push_str("self.");
                cursor = position;
            }
            position += length;
        }
        code.push_str(&source[cursor..end]);
        let lead = code.len() - code.trim_start().len();
        Some(Mapped {
            code: code.trim().to_owned(),
            start: range.start + lead,
            inserts: inserts.into_iter().map(|at| at - lead).collect(),
        })
    }

    /// What a `for` loop iterates, as Askama writes it.
    fn iterated(&self, range: Range<usize>, at: usize) -> Option<String> {
        let text = self.source[range.clone()].trim();
        let code = self.expression(range, at)?;
        Some(if text.contains("..") {
            code
        } else if text.starts_with('[') {
            format!("{code}.iter()")
        } else if text.ends_with(')') || text.ends_with(']') {
            format!("({code}).into_iter()")
        } else if code.starts_with("self.") || text.contains('.') {
            format!("(&{code}).into_iter()")
        } else {
            format!("({code}).into_iter()")
        })
    }

    /// Whether an expression is a literal or a name the template binds, which Askama assigns without a borrow.
    fn is_local_or_literal(&self, text: &str, at: usize) -> bool {
        is_literal(text) || visible_at(self.bindings, at).iter().any(|binding| binding.name == text)
    }
}

/// Whether an expression is a number, string, char or `bool` literal.
fn is_literal(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_digit() || c == '"' || c == '\'' || c == '-') || matches!(text, "true" | "false")
}

/// Rust code made from a template expression, with the way back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapped {
    /// The Rust code.
    pub code: String,
    /// Where the code's first character comes from in the template.
    start: usize,
    /// Where in the code each inserted `self.` starts.
    inserts: Vec<usize>,
}

impl Mapped {
    /// The template offset an offset in the code comes from. Inside an inserted `self.` it is the name the `self.` was put before.
    pub fn template_offset(&self, code_offset: usize) -> usize {
        let mut shift = 0;
        for &insert in &self.inserts {
            if code_offset >= insert + SELF.len() {
                shift += SELF.len();
            } else if code_offset >= insert {
                return self.start + insert - shift;
            }
        }
        self.start + code_offset - shift
    }
}

/// What is put before a name the struct has.
const SELF: &str = "self.";

/// The ranges where a region's branches hold their content: the body, then each later branch's.
fn segments(region: &Region) -> Vec<Range<usize>> {
    let starts = std::iter::once(region.opener.end).chain(region.branches.iter().map(|branch| branch.end));
    let ends = region.branches.iter().map(|branch| branch.start).chain(std::iter::once(region.closer.as_ref().map_or(region.range.end, |closer| closer.start)));
    starts.zip(ends).map(|(start, end)| start..end).collect()
}

/// The text inside the parentheses of a `{% call %}` tag, and the range of it.
pub fn call_arguments(source: &str, tag: &Tag) -> Option<Range<usize>> {
    // The parentheses after the macro's name, not those of the caller parameters before it (`{% call(user) m::row(x) %}`).
    let name_start = call_parts(source, tag).1;
    let open = name_start + source[name_start..tag.inner.end].find('(')?;
    let close = name_start + source[name_start..tag.inner.end].rfind(')')?;
    (open < close).then_some(open + 1..close)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_syntax;
    use crate::outline::regions;
    use crate::scopes::bindings;
    use crate::tags::scan;

    fn setup_at(source: &str, marker: &str, members: &[&str]) -> Option<String> {
        let tags = scan(source, &default_syntax());
        let regions = regions(source, &tags);
        let bindings = bindings(source, &tags, &regions);
        let is_member = |name: &str| members.contains(&name);
        let template = Template { source, tags: &tags, regions: &regions, bindings: &bindings, is_member: &is_member };
        let offset = source.find(marker).unwrap();
        template.setup(offset, 0).map(|setup| {
            assert_eq!(setup.code.lines().filter(|line| line.trim_end().ends_with('{')).count(), setup.open, "{}", setup.code);
            setup.code.lines().map(str::trim).collect::<Vec<_>>().join("\n")
        })
    }

    #[test]
    fn writes_loops_as_askama_iterates() {
        let source = "{% for item in order.items %}{% for (k, v) in item.tags() %}{% for n in 1..count %}{% for x in [1, 2] %}{% for y in names %}HERE{% endfor %}{% endfor %}{% endfor %}{% endfor %}{% endfor %}";
        assert_eq!(
            setup_at(source, "HERE", &["order", "count"]).unwrap(),
            "for item in (&self.order.items).into_iter() {\n\
             for (k, v) in (item.tags()).into_iter() {\n\
             for n in 1..self.count {\n\
             for x in [1, 2].iter() {\n\
             for y in (names).into_iter() {"
        );
    }

    #[test]
    fn writes_lets_if_lets_and_match_arms() {
        let source = "{% let total = order.total() %}{% let n = 3 %}{% if let Some(c) = order.customer %}{% match c.kind %}{% when Kind::A { x, .. } %}HERE{% endmatch %}{% endif %}";
        assert_eq!(
            setup_at(source, "HERE", &["order"]).unwrap(),
            "let total = &(self.order.total());\n\
             let n = 3;\n\
             if let Some(c) = self.order.customer {\n\
             if let Kind::A { x, .. } = &(c.kind) {"
        );
    }

    #[test]
    fn leaves_out_what_is_not_in_scope() {
        let source = "{% for a in xs %}A{% endfor %}{% if x %}{% let b = 1 %}{% endif %}HERE";
        assert_eq!(setup_at(source, "HERE", &[]).unwrap(), "");
    }

    #[test]
    fn reads_a_filtered_value_as_the_value() {
        let source = "{% for item in items|unique %}HERE{% endfor %}";
        assert_eq!(setup_at(source, "HERE", &["items"]).unwrap(), "for item in (&self.items).into_iter() {");
    }

    #[test]
    fn writes_concatenation_and_gives_up_on_nested_filters() {
        let source = "{{ \"a\" ~ name|safe }}{{ f(x|upper) }}";
        let tags = scan(source, &default_syntax());
        let regions = regions(source, &tags);
        let bindings = bindings(source, &tags, &regions);
        let is_member = |name: &str| name == "name";
        let template = Template { source, tags: &tags, regions: &regions, bindings: &bindings, is_member: &is_member };
        let mapped = template.mapped_expression(tags[0].inner.clone(), 0).unwrap();
        assert_eq!(mapped.code, "\"a\" + self.name");
        let name = mapped.code.find("name").unwrap();
        assert_eq!(&source[mapped.template_offset(name)..mapped.template_offset(name + 4)], "name");
        assert!(template.mapped_expression(tags[1].inner.clone(), 0).is_none());
    }

    #[test]
    fn maps_code_back_to_the_template() {
        let source = "{{ order.customer.mail + total }}";
        let tags = scan(source, &default_syntax());
        let regions = regions(source, &tags);
        let bindings = bindings(source, &tags, &regions);
        let is_member = |name: &str| matches!(name, "order" | "total");
        let template = Template { source, tags: &tags, regions: &regions, bindings: &bindings, is_member: &is_member };
        let mapped = template.mapped_expression(tags[0].inner.clone(), 0).unwrap();
        assert_eq!(mapped.code, "self.order.customer.mail + self.total");
        let mail = mapped.code.find("mail").unwrap();
        assert_eq!(&source[mapped.template_offset(mail)..mapped.template_offset(mail + 4)], "mail");
        let total = mapped.code.rfind("total").unwrap();
        assert_eq!(&source[mapped.template_offset(total)..mapped.template_offset(total + 5)], "total");
        // Inside an inserted `self.`, the name it was put before.
        assert_eq!(mapped.template_offset(1), source.find("order").unwrap());
    }

    #[test]
    fn finds_a_calls_arguments_after_its_caller_parameters() {
        let source = "{% call(user) m::list(order.users, 3) %}{% endcall %}";
        let tags = scan(source, &default_syntax());
        assert_eq!(call_arguments(source, &tags[0]).map(|range| &source[range]), Some("order.users, 3"));
    }

    #[test]
    fn writes_macro_arguments() {
        let source = "{% call m::row(item.total_cents(), label = \"x\", 3) %}{% endcall %}";
        let tags = scan(source, &default_syntax());
        let regions = regions(source, &tags);
        let bindings = bindings(source, &tags, &regions);
        let is_member = |_: &str| false;
        let template = Template { source, tags: &tags, regions: &regions, bindings: &bindings, is_member: &is_member };
        let arguments = call_arguments(source, &tags[0]).unwrap();
        let parameters = ["cents".to_owned(), "label".to_owned(), "width".to_owned()];
        let setup = template.macro_arguments(arguments, &parameters).unwrap();
        let lines: Vec<&str> = setup.code.lines().map(str::trim).collect();
        assert_eq!(lines, ["let cents = &(item.total_cents());", "let label = \"x\";", "let width = 3;"]);
    }
}
