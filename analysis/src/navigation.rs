//! Navigation between templates: the template paths in `extends`, `include` and `import`, blocks and the blocks they override, macros and
//! the calls that use them.
//!
//! Other templates are read when a question needs them, not indexed ahead of time. A crate's templates are few and small, so reading them
//! per request is quick, and nothing can go stale. The caller supplies the reader, so open documents are read with their unsaved text.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::config::Config;
use crate::rust::TemplateStruct;
use crate::tags::{self, Tag, TagKind, literal_end};

/// Reads a template's text, preferring an editor's unsaved text to the file on disk.
pub type Reader<'a> = dyn Fn(&Path) -> Option<Arc<str>> + 'a;

/// How many `extends` the parent chain follows before giving up, against cycles.
const MAX_PARENTS: usize = 32;

/// How many templates the search for a struct's templates visits at most.
const MAX_REACHED: usize = 256;

/// Block tags whose words are names of their own (a block, a macro, a template), not variables.
pub const NOT_EXPRESSIONS: &[&str] = &["block", "endblock", "macro", "endmacro", "extends", "include", "import", "call", "endcall", "filter", "raw", "endraw"];

/// A place in a template.
#[derive(Debug, Clone)]
pub struct Location {
    /// The template's file.
    pub path: PathBuf,
    /// The template's text, as it was read.
    pub text: Arc<str>,
    /// The byte range in `text`.
    pub range: Range<usize>,
}

/// A name in a tag, such as a block's or a macro's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ident {
    /// The name.
    pub name: String,
    /// Its byte range in the template.
    pub range: Range<usize>,
}

impl Ident {
    /// Whether `offset` is in the name or at either end of it.
    const fn contains(&self, offset: usize) -> bool {
        self.range.start <= offset && offset <= self.range.end
    }
}

/// The tag a template path is named in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// `{% extends "..." %}`.
    Extends,
    /// `{% include "..." %}`.
    Include,
    /// `{% import "..." as scope %}`.
    Import,
}

/// A template named by `extends`, `include` or `import`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplatePath {
    /// The tag that names it.
    pub kind: PathKind,
    /// The name between the quotes, and its range.
    pub name: Ident,
    /// For `import … as m`, the scope `m`.
    pub scope: Option<Ident>,
}

/// A `{% call %}` tag's macro, with its scope when it has one (`m::money_cell`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroCall {
    /// The import scope, `m` in `m::money_cell`.
    pub scope: Option<Ident>,
    /// The macro's name.
    pub name: Ident,
}

/// What a workspace symbol is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    /// A `{% block %}`.
    Block,
    /// A `{% macro %}`.
    Macro,
}

/// A block or macro anywhere in a crate's templates.
#[derive(Debug, Clone)]
pub struct Symbol {
    /// The block's or macro's name.
    pub name: String,
    /// Whether it is a block or a macro.
    pub kind: SymbolKind,
    /// The template's name, such as `partials/header.html`.
    pub template: String,
    /// Where its name is written.
    pub location: Location,
}

/// The template paths in a template, in order.
pub fn template_paths(source: &str, tags: &[Tag]) -> Vec<TemplatePath> {
    block_tags(source, tags)
        .filter_map(|(tag, keyword)| {
            let kind = match keyword {
                "extends" => PathKind::Extends,
                "include" => PathKind::Include,
                "import" => PathKind::Import,
                _ => return None,
            };
            let name = quoted(source, rest_start(source, tag), tag.inner.end)?;
            let scope = (kind == PathKind::Import)
                .then(|| {
                    let after = skip_whitespace(source, name.range.end + 1);
                    let is_as = source.get(after..).is_some_and(|rest| rest.starts_with("as") && !rest[2..].starts_with(|c: char| c.is_alphanumeric() || c == '_'));
                    is_as.then(|| identifier(source, after + 2, tag.inner.end))?
                })
                .flatten();
            Some(TemplatePath { kind, name, scope })
        })
        .collect()
}

/// The names of the blocks a template defines.
pub fn blocks(source: &str, tags: &[Tag]) -> Vec<Ident> {
    named(source, tags, "block")
}

/// The names of the macros a template defines.
pub fn macros(source: &str, tags: &[Tag]) -> Vec<Ident> {
    named(source, tags, "macro")
}

/// The macros a template calls with `{% call %}`.
pub fn macro_calls(source: &str, tags: &[Tag]) -> Vec<MacroCall> {
    block_tags(source, tags)
        .filter(|&(_, keyword)| keyword == "call")
        .filter_map(|(tag, _)| {
            // After the caller parameters, if there are any: `{% call(user) m::row(x) %}`.
            let first = identifier(source, crate::text::call_parts(source, tag).1, tag.inner.end)?;
            let after = skip_whitespace(source, first.range.end);
            // `{% call name %}` has nothing after the name, and `after` is then past the end.
            if source.get(after..tag.inner.end).is_some_and(|rest| rest.starts_with("::")) {
                let name = identifier(source, after + 2, tag.inner.end)?;
                Some(MacroCall { scope: Some(first), name })
            } else {
                Some(MacroCall { scope: None, name: first })
            }
        })
        .collect()
}

/// The variable named at `offset`: an identifier in an expression that is not a field or method of something (`.name`), a path segment
/// (`a::b`), a filter (`|name`), a tag keyword, or inside a literal. `None` also for an offset that is not on a character boundary.
pub fn variable_at(source: &str, tags: &[Tag], offset: usize) -> Option<Ident> {
    if !source.is_char_boundary(offset) {
        return None;
    }
    let tag = tags::containing(tags, offset).filter(|tag| matches!(tag.kind, TagKind::Expr | TagKind::Block) && tag.inner.start <= offset && offset <= tag.inner.end)?;
    if tag.kind == TagKind::Block && NOT_EXPRESSIONS.contains(&tag.keyword(source)) {
        return None;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let start = source[..offset].trim_end_matches(is_word).len().max(tag.inner.start);
    let end = offset + source[offset..tag.inner.end].find(|c: char| !is_word(c)).unwrap_or(tag.inner.end - offset);
    let name = &source[start..end];
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    if tag.kind == TagKind::Block && start == tag.inner.start {
        return None;
    }
    let before = source[tag.inner.start..start].trim_end();
    // A `.` before the name makes it a member, but `..` makes it the end of a range (`0..n`).
    let member = before.ends_with('.') && !before.ends_with("..");
    if member || before.ends_with([':', '|']) || source[end..tag.inner.end].trim_start().starts_with("::") {
        return None;
    }
    let mut at = tag.inner.start;
    while at < start {
        match literal_end(source, at) {
            Some(literal) if literal > start => return None,
            Some(literal) => at = literal,
            None => at = tags::next_char(source, at),
        }
    }
    Some(Ident { name: name.to_owned(), range: start..end })
}

/// The names that the block tags with `keyword` define, such as every `{% block name %}`.
fn named(source: &str, tags: &[Tag], keyword: &str) -> Vec<Ident> {
    block_tags(source, tags)
        .filter(|&(_, found)| found == keyword)
        .filter_map(|(tag, _)| identifier(source, rest_start(source, tag), tag.inner.end))
        .collect()
}

/// The block tags (`{% %}`) among `tags`, each with its keyword.
fn block_tags<'t, 's>(source: &'s str, tags: &'t [Tag]) -> impl Iterator<Item = (&'t Tag, &'s str)> {
    tags.iter().filter(|tag| tag.kind == TagKind::Block).map(move |tag| (tag, tag.keyword(source)))
}

/// Where the text after a tag's keyword starts.
fn rest_start(source: &str, tag: &Tag) -> usize {
    skip_whitespace(source, tag.inner.start + tag.keyword(source).len())
}

/// The offset of the first character at or after `from` that is not whitespace.
fn skip_whitespace(source: &str, from: usize) -> usize {
    let from = from.min(source.len());
    from + (source[from..].len() - source[from..].trim_start().len())
}

/// The identifier starting at `from`, after any whitespace, if it ends before `limit`.
fn identifier(source: &str, from: usize, limit: usize) -> Option<Ident> {
    let start = skip_whitespace(source, from);
    let text = source.get(start..limit)?;
    if !text.starts_with(|c: char| c.is_alphabetic() || c == '_') {
        return None;
    }
    let length = text.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(text.len());
    Some(Ident { name: text[..length].to_owned(), range: start..start + length })
}

/// The text of a string literal starting at `from`, without its quotes.
fn quoted(source: &str, from: usize, limit: usize) -> Option<Ident> {
    let text = source.get(from..limit)?;
    let content = text.strip_prefix('"')?;
    let length = content.find('"')?;
    let start = from + 1;
    Some(Ident { name: content[..length].to_owned(), range: start..start + length })
}

/// What a crate's templates are on disk: their texts, canonical paths, where Askama finds each template name, and the templates each one
/// reaches. Every question that spans templates needs these, and working them out means reading files and asking the file system.
///
/// A project makes a cache of its own, which lives for one request. A caller that watches the crate's template directories can keep one
/// across requests instead (`Project::with_cache`) and `clear` it whenever a file there changes; nothing in it is otherwise checked against
/// the disk. Open documents are not held here: their text comes from the reader, and what is worked out from a text is kept only while the
/// text is the same.
#[derive(Debug, Default)]
pub struct FileCache {
    /// Texts read from disk, `None` for a file that could not be read.
    texts: HashMap<PathBuf, Option<Arc<str>>>,
    /// The canonical form of each path, `None` for one that does not exist.
    canonical: HashMap<PathBuf, Option<PathBuf>>,
    /// Where Askama finds each name, by the directory of the template that names it (`None` for a struct's template) and the name.
    found: HashMap<(Option<PathBuf>, String), Option<PathBuf>>,
    /// The templates each template names in `extends` and `include`, found, with the text they were worked out from.
    reaches: HashMap<PathBuf, (Arc<str>, Arc<[PathBuf]>)>,
    /// Every file in the template directories (`Config::template_files`), once listed.
    files: Option<Arc<[PathBuf]>>,
}

impl FileCache {
    /// Forgets everything, after a change in the template directories.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// A file's text from disk, read once (see `project::read_text`).
    pub fn text(&mut self, path: &Path) -> Option<Arc<str>> {
        self.texts.entry(path.to_path_buf()).or_insert_with(|| crate::project::read_text(path).map(Arc::from)).clone()
    }
}

/// A project's file cache: its own, for one request, or one the caller keeps.
enum CacheRef<'a> {
    /// Made for this project.
    Own(Box<Mutex<FileCache>>),
    /// Kept by the caller across requests.
    Shared(&'a Mutex<FileCache>),
}

/// A crate's templates, for questions that span files.
///
/// A project lives for one request. It remembers the texts it reads, so the answers within one request agree with each other, and keeps
/// what it works out from the file system in a `FileCache`, so a template several questions need, such as a shared base, is read and
/// scanned once.
pub struct Project<'a> {
    /// The crate's Askama configuration.
    config: &'a Config,
    /// Reads a template's text.
    read: &'a Reader<'a>,
    /// The texts read so far in this request, `None` for a file that could not be read.
    texts: RefCell<HashMap<PathBuf, Option<Arc<str>>>>,
    /// What is known of the files on disk.
    cache: CacheRef<'a>,
}

impl<'a> Project<'a> {
    /// The templates of the crate `config` describes, read with `read`, with a file cache for this request only.
    pub fn new(config: &'a Config, read: &'a Reader<'a>) -> Self {
        Self { config, read, texts: RefCell::default(), cache: CacheRef::Own(Box::default()) }
    }

    /// Like `new`, keeping what is learned of the files in `cache`, which the caller keeps across requests and clears when a template file
    /// changes.
    pub fn with_cache(config: &'a Config, read: &'a Reader<'a>, cache: &'a Mutex<FileCache>) -> Self {
        Self { config, read, texts: RefCell::default(), cache: CacheRef::Shared(cache) }
    }

    /// The file cache, locked. It is never held while the reader runs or while another lock is taken.
    fn cache(&self) -> MutexGuard<'_, FileCache> {
        match &self.cache {
            CacheRef::Own(cache) => cache.lock(),
            CacheRef::Shared(cache) => cache.lock(),
        }
        .unwrap_or_else(PoisonError::into_inner)
    }

    /// Every file in the template directories, listed once per cache.
    fn template_files(&self) -> Arc<[PathBuf]> {
        if let Some(files) = &self.cache().files {
            return Arc::clone(files);
        }
        let files: Arc<[PathBuf]> = self.config.template_files().into();
        self.cache().files = Some(Arc::clone(&files));
        files
    }

    /// The file Askama finds for `name`, beside `from` first (see `Config::find_template`), asking the file system once per cache.
    fn find(&self, name: &str, from: Option<&Path>) -> Option<PathBuf> {
        let key = (from.and_then(Path::parent).map(Path::to_path_buf), name.to_owned());
        if let Some(found) = self.cache().found.get(&key) {
            return found.clone();
        }
        let found = self.config.find_template(name, from);
        self.cache().found.insert(key, found.clone());
        found
    }

    /// The templates `file` names in `extends` and `include` that Askama would find, worked out once per text. `import` does not count:
    /// macros do not see the struct's fields.
    fn reaches(&self, file: &Path) -> Arc<[PathBuf]> {
        let Some(text) = self.read(file) else {
            return Arc::from([]);
        };
        if let Some((from, found)) = self.cache().reaches.get(file)
            && Arc::ptr_eq(from, &text)
        {
            return Arc::clone(found);
        }
        let found: Arc<[PathBuf]> = template_paths(&text, &self.tags(&text))
            .into_iter()
            .filter(|path| path.kind != PathKind::Import)
            .filter_map(|path| self.find(&path.name.name, Some(file)))
            .collect::<Vec<_>>()
            .into();
        self.cache().reaches.insert(file.to_path_buf(), (text, Arc::clone(&found)));
        found
    }

    /// A template's tags, scanned with the crate's default syntax.
    fn tags(&self, text: &str) -> Vec<Tag> {
        tags::scan(text, &self.config.default_syntax())
    }

    /// A template's text, read once per project.
    fn read(&self, path: &Path) -> Option<Arc<str>> {
        if let Some(text) = self.texts.borrow().get(path) {
            return text.clone();
        }
        let text = (self.read)(path);
        self.texts.borrow_mut().insert(path.to_path_buf(), text.clone());
        text
    }

    /// A path's canonical form, worked out once per cache.
    fn canonical(&self, path: &Path) -> Option<PathBuf> {
        if let Some(canonical) = self.cache().canonical.get(path) {
            return canonical.clone();
        }
        let canonical = path.canonicalize().ok();
        self.cache().canonical.insert(path.to_path_buf(), canonical.clone());
        canonical
    }

    /// Whether two paths name the same file, as `same_file` decides, with the canonical paths remembered.
    fn same_file(&self, a: &Path, b: &Path) -> bool {
        match (self.canonical(a), self.canonical(b)) {
            (Some(a), Some(b)) => a == b,
            _ => a == b,
        }
    }

    /// The key a path is compared by in a set of visited templates.
    fn key(&self, path: &Path) -> PathBuf {
        self.canonical(path).unwrap_or_else(|| path.to_path_buf())
    }

    /// Each template path in a template, with the file it refers to, or `None` when Askama would not find it.
    pub fn resolved_paths(&self, file: &Path, text: &str) -> Vec<(TemplatePath, Option<PathBuf>)> {
        template_paths(text, &self.tags(text))
            .into_iter()
            .map(|path| {
                let target = self.find(&path.name.name, Some(file));
                (path, target)
            })
            .collect()
    }

    /// Where the name at `offset` is defined: the file of a template path, the parent's block that a block overrides, a called macro, or
    /// the `import` that gives a scope.
    pub fn definition(&self, file: &Path, text: &Arc<str>, offset: usize) -> Option<Location> {
        let tags = self.tags(text);
        let tag = tags.iter().find(|tag| tag.kind == TagKind::Block && tag.range.start <= offset && offset <= tag.range.end)?;
        match tag.keyword(text) {
            "extends" | "include" | "import" => {
                let path = template_paths(text, std::slice::from_ref(tag)).into_iter().next()?;
                if !path.name.contains(offset) {
                    return None;
                }
                let target = self.find(&path.name.name, Some(file))?;
                let target_text = self.read(&target)?;
                Some(Location { path: target, text: target_text, range: 0..0 })
            }
            "block" => {
                let name = blocks(text, std::slice::from_ref(tag)).into_iter().next()?;
                if !name.contains(offset) {
                    return None;
                }
                self.parents(file, text).into_iter().find_map(|(parent, parent_text)| {
                    let block = blocks(&parent_text, &self.tags(&parent_text)).into_iter().find(|block| block.name == name.name)?;
                    Some(Location { path: parent, text: parent_text, range: block.range })
                })
            }
            "call" => {
                let call = macro_calls(text, std::slice::from_ref(tag)).into_iter().next()?;
                if let Some(scope) = call.scope.as_ref().filter(|scope| scope.contains(offset)) {
                    let import = import(text, &tags, &scope.name)?;
                    let scope = import.scope?;
                    return Some(Location { path: file.to_path_buf(), text: Arc::clone(text), range: scope.range });
                }
                if call.name.contains(offset) {
                    return self.macro_definition(file, text, &tags, &call);
                }
                None
            }
            _ => None,
        }
    }

    /// The block named at `offset`, if the cursor is on a block's name.
    pub fn block_at(&self, text: &str, offset: usize) -> Option<Ident> {
        blocks(text, &self.tags(text)).into_iter().find(|block| block.contains(offset))
    }

    /// The blocks in other templates that override `file`'s block `name`: those in every template that extends `file`, directly or through
    /// other templates.
    pub fn overrides(&self, file: &Path, name: &str) -> Vec<Location> {
        self.template_files()
            .iter()
            .filter(|other| !self.same_file(other, file))
            .filter_map(|other| {
                let text = self.read(other)?;
                let extends_file = self.parents(other, &text).iter().any(|(parent, _)| self.same_file(parent, file));
                extends_file.then_some((other, text))
            })
            .flat_map(|(other, text)| {
                blocks(&text, &self.tags(&text))
                    .into_iter()
                    .filter(|block| block.name == name)
                    .map(move |block| Location { path: other.clone(), text: Arc::clone(&text), range: block.range })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Every block and macro in the crate's templates.
    pub fn symbols(&self) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        for file in self.template_files().iter() {
            let Some(text) = self.read(file) else {
                continue;
            };
            let template = self.config.template_name(file).unwrap_or_else(|| file.display().to_string());
            let tags = self.tags(&text);
            let found = blocks(&text, &tags)
                .into_iter()
                .map(|name| (name, SymbolKind::Block))
                .chain(macros(&text, &tags).into_iter().map(|name| (name, SymbolKind::Macro)));
            for (ident, kind) in found {
                symbols.push(Symbol {
                    name: ident.name,
                    kind,
                    template: template.clone(),
                    location: Location { path: file.clone(), text: Arc::clone(&text), range: ident.range },
                });
            }
        }
        symbols
    }

    /// The template structs whose template is `file` or reaches it through `extends` or `include`, so their fields are in scope in it.
    pub fn structs_for<'s>(&self, file: &Path, structs: &'s [TemplateStruct]) -> Vec<&'s TemplateStruct> {
        let file = self.key(file);
        // Structs often share a template, and each template is followed once.
        let mut reaches: HashMap<&str, bool> = HashMap::new();
        structs
            .iter()
            .filter(|template_struct| {
                *reaches.entry(template_struct.template.as_str()).or_insert_with(|| {
                    self.find(&template_struct.template, None).is_some_and(|start| self.reached_from(start).contains(&file))
                })
            })
            .collect()
    }

    /// Whether `file` is the template of one of `structs` or one it extends, so the struct's fields and the template's own bindings are all
    /// the names it can use. A template reached through `include` also sees the names bound where it is included.
    pub fn sees_only_struct_names(&self, file: &Path, structs: &[&TemplateStruct]) -> bool {
        structs.iter().any(|template_struct| {
            let Some(start) = self.find(&template_struct.template, None) else {
                return false;
            };
            if self.same_file(&start, file) {
                return true;
            }
            let Some(text) = self.read(&start) else {
                return false;
            };
            self.parents(&start, &text).iter().any(|(parent, _)| self.same_file(parent, file))
        })
    }

    /// `start` and every template it reaches through `extends` and `include`, by canonical path (see `key`).
    fn reached_from(&self, start: PathBuf) -> HashSet<PathBuf> {
        let mut reached = HashSet::from([self.key(&start)]);
        let mut queue = vec![start];
        while let Some(file) = queue.pop() {
            if reached.len() >= MAX_REACHED {
                break;
            }
            for target in self.reaches(&file).iter() {
                if reached.insert(self.key(target)) {
                    queue.push(target.clone());
                }
            }
        }
        reached
    }

    /// Every `{% call %}` of the macro `name` defined in `macro_file`, in any template: the calling template, its text and the call tag's
    /// range.
    pub fn macro_call_sites(&self, macro_file: &Path, name: &str) -> Vec<(PathBuf, Arc<str>, Range<usize>)> {
        let mut sites = Vec::new();
        for file in self.template_files().iter() {
            let Some(text) = self.read(file) else {
                continue;
            };
            let tags = self.tags(&text);
            for tag in tags.iter().filter(|tag| tag.kind == TagKind::Block && tag.keyword(&text) == "call") {
                let Some(call) = macro_calls(&text, std::slice::from_ref(tag)).into_iter().next().filter(|call| call.name.name == name) else {
                    continue;
                };
                let target = match &call.scope {
                    Some(scope) => import(&text, &tags, &scope.name).and_then(|import| self.find(&import.name.name, Some(file))),
                    None => Some(file.clone()),
                };
                if target.is_some_and(|target| self.same_file(&target, macro_file)) {
                    sites.push((file.clone(), Arc::clone(&text), tag.range.clone()));
                }
            }
        }
        sites
    }

    /// The templates `file` extends, nearest first, with their text.
    fn parents(&self, file: &Path, text: &str) -> Vec<(PathBuf, Arc<str>)> {
        let mut chain: Vec<(PathBuf, Arc<str>)> = Vec::new();
        let mut current_file = file.to_path_buf();
        let mut current_extends = extends(text, &self.tags(text));
        while chain.len() < MAX_PARENTS {
            let Some(name) = current_extends else {
                break;
            };
            let Some(parent) = self.find(&name, Some(&current_file)) else {
                break;
            };
            if self.same_file(&parent, file) || chain.iter().any(|(seen, _)| self.same_file(seen, &parent)) {
                break;
            }
            let Some(parent_text) = self.read(&parent) else {
                break;
            };
            current_extends = extends(&parent_text, &self.tags(&parent_text));
            chain.push((parent.clone(), parent_text));
            current_file = parent;
        }
        chain
    }

    /// A called macro's definition: in the imported template for a scoped call, otherwise in this template or, failing that, a template it
    /// extends.
    fn macro_definition(&self, file: &Path, text: &Arc<str>, tags: &[Tag], call: &MacroCall) -> Option<Location> {
        let candidates: Vec<(PathBuf, Arc<str>)> = match &call.scope {
            Some(scope) => {
                let import = import(text, tags, &scope.name)?;
                let target = self.find(&import.name.name, Some(file))?;
                let target_text = self.read(&target)?;
                vec![(target, target_text)]
            }
            None => std::iter::once((file.to_path_buf(), Arc::clone(text))).chain(self.parents(file, text)).collect(),
        };
        candidates.into_iter().find_map(|(path, candidate)| {
            let found = macros(&candidate, &self.tags(&candidate)).into_iter().find(|name| name.name == call.name.name)?;
            Some(Location { path, text: candidate, range: found.range })
        })
    }
}

/// The template a template's `{% extends %}` names, if it has one.
fn extends(text: &str, tags: &[Tag]) -> Option<String> {
    template_paths(text, tags).into_iter().find(|path| path.kind == PathKind::Extends).map(|path| path.name.name)
}

/// The `{% import %}` that gives the scope `scope`.
fn import(text: &str, tags: &[Tag], scope: &str) -> Option<TemplatePath> {
    template_paths(text, tags)
        .into_iter()
        .find(|path| path.kind == PathKind::Import && path.scope.as_ref().is_some_and(|found| found.name == scope))
}

/// Whether two paths name the same file, comparing canonical paths when both exist.
pub fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_syntax;
    use crate::testing::TempDir;

    fn site() -> TempDir {
        let dir = TempDir::new();
        dir.write("templates/base.html", "<title>{% block title %}{% endblock %}</title>\n{% block content %}{% endblock %}");
        dir.write("templates/page.html", "{% extends \"base.html\" %}\n{% import \"macros.html\" as m %}\n{% block content %}{% call m::cell(1) %}{% endcall %}{% endblock %}");
        dir.write("templates/deep/leaf.html", "{% extends \"page.html\" %}{% block title %}Leaf{% endblock %}");
        dir.write("templates/macros.html", "{% macro cell(x) %}<td>{{ x }}</td>{% endmacro %}");
        dir.write("templates/local.html", "{% macro row() %}{% endmacro %}{% call row() %}{% endcall %}{% include \"missing.html\" %}");
        dir
    }

    fn read(path: &Path) -> Option<Arc<str>> {
        std::fs::read_to_string(path).ok().map(Arc::from)
    }

    fn located(location: &Location) -> (String, &str) {
        let name = location.path.file_name().unwrap().to_string_lossy().into_owned();
        (name, &location.text[location.range.clone()])
    }

    fn with_project<T>(dir: &TempDir, question: impl FnOnce(&Project<'_>) -> T) -> T {
        let config = Config::default_for(&dir.path(""));
        let reader: &Reader<'_> = &read;
        question(&Project::new(&config, reader))
    }

    fn at(text: &str, needle: &str) -> usize {
        text.find(needle).unwrap() + 1
    }

    #[test]
    fn finds_paths_with_their_scope() {
        let source = "{% extends \"a.html\" %}{% include \"b/c.html\" %}{% import \"m.html\" as m %}";
        let paths = template_paths(source, &tags::scan(source, &default_syntax()));
        let names: Vec<(&str, Option<&str>)> = paths.iter().map(|path| (path.name.name.as_str(), path.scope.as_ref().map(|scope| scope.name.as_str()))).collect();
        assert_eq!(names, [("a.html", None), ("b/c.html", None), ("m.html", Some("m"))]);
        assert_eq!(&source[paths[0].name.range.clone()], "a.html");
    }

    #[test]
    fn reads_calls_with_and_without_scope() {
        let source = "{% call m::cell(1) %}{% endcall %}{% call row() %}{% endcall %}";
        let calls = macro_calls(source, &tags::scan(source, &default_syntax()));
        assert_eq!(calls[0].scope.as_ref().map(|scope| scope.name.as_str()), Some("m"));
        assert_eq!(calls[0].name.name, "cell");
        assert_eq!((calls[1].scope.clone(), calls[1].name.name.as_str()), (None, "row"));
    }

    #[test]
    fn goes_from_a_path_to_its_template() {
        let dir = site();
        let page = dir.path("templates/page.html");
        let text = read(&page).unwrap();
        let found = with_project(&dir, |project| project.definition(&page, &text, at(&text, "base.html")));
        assert_eq!(found.map(|location| located(&location).0), Some("base.html".to_owned()));
    }

    #[test]
    fn finds_a_path_beside_the_template_first() {
        // `page.html` from `deep/leaf.html` is not beside it, so it comes from `templates/`.
        let dir = site();
        let leaf = dir.path("templates/deep/leaf.html");
        let text = read(&leaf).unwrap();
        let found = with_project(&dir, |project| project.definition(&leaf, &text, at(&text, "page.html")));
        assert_eq!(found.map(|location| location.path), Some(dir.path("templates/page.html")));
    }

    #[test]
    fn goes_from_a_block_to_the_one_it_overrides() {
        let dir = site();
        let leaf = dir.path("templates/deep/leaf.html");
        let text = read(&leaf).unwrap();
        // `title` is not in `page.html`, so the search continues to `base.html`.
        let found = with_project(&dir, |project| project.definition(&leaf, &text, at(&text, "title")));
        assert_eq!(found.as_ref().map(located), Some(("base.html".to_owned(), "title")));
    }

    #[test]
    fn goes_from_a_call_to_the_macro_and_its_scope() {
        let dir = site();
        let page = dir.path("templates/page.html");
        let text = read(&page).unwrap();
        let call = text.find("m::cell").unwrap();
        let (to_macro, to_scope) = with_project(&dir, |project| {
            (project.definition(&page, &text, call + 4), project.definition(&page, &text, call))
        });
        assert_eq!(to_macro.as_ref().map(located), Some(("macros.html".to_owned(), "cell")));
        let to_scope = to_scope.unwrap();
        assert_eq!(located(&to_scope), ("page.html".to_owned(), "m"));
        assert!(to_scope.range.start < call, "the scope is the one in the import");
    }

    #[test]
    fn goes_to_a_macro_in_the_same_template() {
        let dir = site();
        let local = dir.path("templates/local.html");
        let text = read(&local).unwrap();
        let found = with_project(&dir, |project| project.definition(&local, &text, text.rfind("row").unwrap()));
        assert_eq!(found.map(|location| location.range.start), Some(text.find("row").unwrap()));
    }

    #[test]
    fn flags_paths_askama_would_not_find() {
        let dir = site();
        let local = dir.path("templates/local.html");
        let text = read(&local).unwrap();
        let resolved = with_project(&dir, |project| project.resolved_paths(&local, &text));
        assert_eq!(resolved.len(), 1);
        assert_eq!((resolved[0].0.name.name.as_str(), resolved[0].1.clone()), ("missing.html", None));
    }

    #[test]
    fn finds_overrides_in_every_descendant() {
        let dir = site();
        let base = dir.path("templates/base.html");
        let (title, content) = with_project(&dir, |project| (project.overrides(&base, "title"), project.overrides(&base, "content")));
        let names = |locations: &[Location]| locations.iter().map(|location| located(location).0).collect::<Vec<_>>();
        assert_eq!(names(&title), ["leaf.html"]);
        assert_eq!(names(&content), ["page.html"]);
    }

    #[test]
    fn lists_blocks_and_macros() {
        let dir = site();
        let mut symbols: Vec<(String, SymbolKind, String)> = with_project(&dir, |project| project.symbols())
            .into_iter()
            .map(|symbol| (symbol.name, symbol.kind, symbol.template))
            .collect();
        symbols.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
        assert_eq!(
            symbols,
            [
                ("cell".to_owned(), SymbolKind::Macro, "macros.html".to_owned()),
                ("content".to_owned(), SymbolKind::Block, "base.html".to_owned()),
                ("content".to_owned(), SymbolKind::Block, "page.html".to_owned()),
                ("row".to_owned(), SymbolKind::Macro, "local.html".to_owned()),
                ("title".to_owned(), SymbolKind::Block, "base.html".to_owned()),
                ("title".to_owned(), SymbolKind::Block, "deep/leaf.html".to_owned()),
            ]
        );
    }

    #[test]
    fn finds_variables_but_not_members_filters_or_names() {
        let source = "{{ order.id|upper }}{% if a::B %}{% block x %}{% endblock %}{{ \"y\" }}{% for i in items %}";
        let tags = tags::scan(source, &default_syntax());
        let variable = |needle: &str, nth: usize| {
            let offset = source.match_indices(needle).nth(nth).unwrap().0 + 1;
            variable_at(source, &tags, offset).map(|ident| ident.name)
        };
        assert_eq!(variable("order", 0), Some("order".to_owned()));
        assert_eq!(variable("id", 0), None);
        assert_eq!(variable("upper", 0), None);
        assert_eq!(variable("a::", 0), None);
        assert_eq!(variable("B", 0), None);
        assert_eq!(variable("if", 0), None);
        assert_eq!(variable("x %", 0), None);
        assert_eq!(variable("y", 0), None);
        assert_eq!(variable("items", 0), Some("items".to_owned()));
    }

    #[test]
    fn links_templates_to_the_structs_that_reach_them() {
        let dir = site();
        let structs: Vec<TemplateStruct> = ["page.html", "macros.html", "deep/leaf.html"]
            .iter()
            .map(|template| TemplateStruct {
                name: template.to_string(),
                template: template.to_string(),
                template_range: 0..0,
                file: dir.path("src/main.rs").into(),
                range: 0..0,
                members: Vec::new(),
                impl_header: String::new(),
                item_end: 0,
            })
            .collect();
        let names = |template: &str| {
            with_project(&dir, |project| {
                project.structs_for(&dir.path(template), &structs).iter().map(|found| found.name.clone()).collect::<Vec<_>>()
            })
        };
        // `base.html` is extended by `page.html`, which `deep/leaf.html` extends in turn.
        assert_eq!(names("templates/base.html"), ["page.html", "deep/leaf.html"]);
        assert_eq!(names("templates/page.html"), ["page.html", "deep/leaf.html"]);
        // Imported macros do not see the struct's fields.
        assert_eq!(names("templates/macros.html"), ["macros.html"]);
        assert_eq!(names("templates/local.html"), Vec::<String>::new());
    }

    #[test]
    fn reads_the_macro_after_caller_parameters() {
        let source = "{% call(user) m::list(users) %}{% endcall %}{% call (row) table() %}{% endcall %}";
        let calls = macro_calls(source, &tags::scan(source, &default_syntax()));
        let names: Vec<(Option<&str>, &str)> = calls.iter().map(|call| (call.scope.as_ref().map(|scope| scope.name.as_str()), call.name.name.as_str())).collect();
        assert_eq!(names, [(Some("m"), "list"), (None, "table")]);
    }

    #[test]
    fn reads_a_call_with_no_arguments_yet() {
        // While typing, `{% call row %}` has no `(`, which once sliced past the tag's end.
        let source = "{% call row %}{% call m::cell %}";
        let calls = macro_calls(source, &tags::scan(source, &default_syntax()));
        let names: Vec<(Option<&str>, &str)> = calls.iter().map(|call| (call.scope.as_ref().map(|scope| scope.name.as_str()), call.name.name.as_str())).collect();
        assert_eq!(names, [(None, "row"), (Some("m"), "cell")]);
    }

    #[test]
    fn finds_the_end_of_a_range_as_a_variable() {
        let source = "{% for i in 0..n %}{{ i }}{% endfor %}{{ a.b }}";
        let tags = tags::scan(source, &default_syntax());
        assert_eq!(variable_at(source, &tags, source.find("n %").unwrap()).map(|ident| ident.name), Some("n".to_owned()));
        assert_eq!(variable_at(source, &tags, source.find("b }").unwrap()), None);
    }

    #[test]
    fn needs_a_whole_word_for_an_import_scope() {
        let source = "{% import \"m.html\" asm %}";
        let paths = template_paths(source, &tags::scan(source, &default_syntax()));
        assert_eq!(paths[0].scope, None);
    }

    #[test]
    fn keeps_what_a_shared_cache_learns_until_it_is_cleared() {
        let dir = site();
        let config = Config::default_for(&dir.path(""));
        let cache = Mutex::new(FileCache::default());
        let structs = vec![TemplateStruct {
            name: "Leaf".to_owned(),
            template: "deep/leaf.html".to_owned(),
            template_range: 0..0,
            file: dir.path("src/main.rs").into(),
            range: 0..0,
            members: Vec::new(),
            impl_header: String::new(),
            item_end: 0,
        }];
        let reaches_base = |read: &Reader<'_>| !Project::with_cache(&config, read, &cache).structs_for(&dir.path("templates/base.html"), &structs).is_empty();
        let from_disk = |path: &Path| cache_text(&cache, path);
        assert!(reaches_base(&from_disk));
        // `page.html` stops extending `base.html` on disk, unreported: the cache still has the old text and what came of it.
        dir.write("templates/page.html", "{% block content %}{% endblock %}");
        assert!(reaches_base(&from_disk));
        cache.lock().unwrap().clear();
        assert!(!reaches_base(&from_disk));
        // An open document's text comes from the reader, and a new text is read anew even with the cache kept.
        let edited = |path: &Path| if path.ends_with("page.html") { Some(Arc::from("{% extends \"base.html\" %}")) } else { cache_text(&cache, path) };
        assert!(reaches_base(&edited));
    }

    /// A file's text through the cache, as a server's reader takes the text of a file that is not open.
    fn cache_text(cache: &Mutex<FileCache>, path: &Path) -> Option<Arc<str>> {
        cache.lock().unwrap().text(path)
    }

    #[test]
    fn stops_at_an_extends_cycle() {
        let dir = TempDir::new();
        dir.write("templates/a.html", "{% extends \"b.html\" %}{% block x %}{% endblock %}");
        dir.write("templates/b.html", "{% extends \"a.html\" %}");
        let a = dir.path("templates/a.html");
        let text = read(&a).unwrap();
        let found = with_project(&dir, |project| project.definition(&a, &text, at(&text, "x %")));
        assert!(found.is_none());
    }
}
