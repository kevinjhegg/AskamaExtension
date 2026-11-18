//! Probes: Rust code that puts a template expression where rust-analyzer can answer about it.
//!
//! Askama generates its code inside a derive macro, and rust-analyzer does not map that code back to template positions, so it cannot be
//! asked about a template directly. Instead the expression is written into a method of the template's struct, added right after the
//! struct's item so it is in the same module, and rust-analyzer is asked about that. The probe only ever exists in rust-analyzer's memory.

use crate::lines::{LineIndex, Position};
use crate::lowering::Setup;
use crate::rust::TemplateStruct;

/// A Rust file with a probe in it, and the position in it to ask about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The struct's file with the probe in it.
    pub text: String,
    /// Where to ask rust-analyzer, in `text`.
    pub position: Position,
    /// The line where the probe starts and how many lines it adds, to move positions rust- analyzer gives in the rest of the file back to
    /// where they are on disk.
    pub first_line: u32,
    /// How many lines the probe adds.
    pub added_lines: u32,
}

impl Probe {
    /// Where a line of the probed file is in the file on disk, or `None` inside the probe.
    pub const fn original_line(&self, line: u32) -> Option<u32> {
        if line < self.first_line {
            Some(line)
        } else if line >= self.first_line + self.added_lines {
            Some(line - self.added_lines)
        } else {
            None
        }
    }
}

/// The struct's file with a method whose body ends in `self.{path}.{typed}`, the cursor after `typed`: completion there lists the members
/// of the value `path` names.
pub fn members(file_text: &str, template_struct: &TemplateStruct, path: &str, typed: &str) -> Probe {
    members_in(file_text, template_struct, &Setup::default(), &format!("self.{path}"), typed)
}

/// Like `members`, for an expression in the scope that `setup` builds: the template's loops, `let`s and patterns around the cursor, written
/// as Askama writes them.
pub fn members_in(file_text: &str, template_struct: &TemplateStruct, setup: &Setup, expression: &str, typed: &str) -> Probe {
    let line = format!("{expression}.{typed}");
    let cursor = line.len();
    at(file_text, template_struct, setup, &line, cursor)
}

/// The struct's file with a method that runs `setup` and then `line`, the position at `cursor` within `line`.
pub fn at(file_text: &str, template_struct: &TemplateStruct, setup: &Setup, line: &str, cursor: usize) -> Probe {
    let (text, lines) = with_blocks(file_text, template_struct, &[(setup.clone(), line.to_owned())]);
    let index = LineIndex::new(&text);
    let line_start = index.offset(&text, Position { line: lines[0], character: 0 });
    let position = index.position(&text, line_start + LINE_INDENT.len() + cursor);
    let added = u32::try_from(text.lines().count().saturating_sub(file_text.lines().count())).unwrap_or(0);
    let first_line = u32::try_from(file_text[..probe_offset(file_text, template_struct)].matches('\n').count() + 1).unwrap_or(u32::MAX);
    Probe { text, position, first_line, added_lines: added }
}

/// Where the probe goes in the struct's file: right after the struct's item. The offset comes from when the file was last parsed, so for a
/// file changed since, it is kept inside the text and on a character boundary.
fn probe_offset(file_text: &str, template_struct: &TemplateStruct) -> usize {
    file_text.floor_char_boundary(template_struct.item_end)
}

/// Where `with_blocks` indents each block's last line.
pub const LINE_INDENT: &str = "        ";

/// The struct's file with a probe method holding one block per `(setup, line)`, each line after its setup. Also the line number of each
/// `line` in the result.
pub fn with_blocks(file_text: &str, template_struct: &TemplateStruct, blocks: &[(Setup, String)]) -> (String, Vec<u32>) {
    let at = probe_offset(file_text, template_struct);
    let mut text = format!("{}\n#[allow(warnings, clippy::all)]\n{} {{\n    fn __askama_probe(&self) {{\n", &file_text[..at], template_struct.impl_header);
    // The line count so far, kept up as the blocks are added rather than counted each time.
    let mut line_count = text.matches('\n').count();
    let mut lines = Vec::with_capacity(blocks.len());
    for (setup, line) in blocks {
        text.push_str("        {\n");
        text.push_str(&setup.code);
        line_count += 1 + setup.code.matches('\n').count();
        lines.push(u32::try_from(line_count).unwrap_or(u32::MAX));
        text.push_str(LINE_INDENT);
        text.push_str(line);
        text.push('\n');
        text.push_str(&"}\n".repeat(setup.open));
        text.push_str("        }\n");
        line_count += 1 + line.matches('\n').count() + setup.open + 1;
    }
    text.push_str("    }\n}\n");
    text.push_str(&file_text[at..]);
    (text, lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rust::RustIndex;
    use crate::testing::TempDir;

    #[test]
    fn writes_the_expression_into_a_method_after_the_struct() {
        let dir = TempDir::new();
        let main = "mod pages {\n    #[derive(Template)]\n    #[template(path = \"p.html\")]\n    pub struct Page<'a, T: Clone> where T: Default {\n        pub order: &'a T,\n    }\n}\nfn main() {}\n";
        dir.write("src/main.rs", main);
        let facts = RustIndex::default().crate_facts(&dir.path(""));
        let probe = members(main, &facts.structs[0], "order.customer", "na");
        assert_eq!(
            probe.text,
            "mod pages {\n    #[derive(Template)]\n    #[template(path = \"p.html\")]\n    pub struct Page<'a, T: Clone> where T: Default {\n        pub order: &'a T,\n    }\n\
             #[allow(warnings, clippy::all)]\n\
             impl< 'a , T : Clone > Page< 'a , T > where T : Default {\n    fn __askama_probe(&self) {\n        {\n        self.order.customer.na\n        }\n    }\n}\n\n}\nfn main() {}\n"
        );
        assert_eq!(probe.position, Position { line: 10, character: 30 });
        // Lines before the probe stay; lines after it move back by the lines it adds.
        assert_eq!((probe.first_line, probe.added_lines), (6, 9));
        assert_eq!(probe.original_line(5), Some(5));
        assert_eq!(probe.original_line(10), None);
        assert_eq!(probe.original_line(15), Some(6));
    }
}
