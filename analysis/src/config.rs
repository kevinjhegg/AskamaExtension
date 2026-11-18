//! A crate's Askama configuration, read from `askama.toml` beside its `Cargo.toml` the way `askama_derive` 0.16 reads it. Only what the
//! analysis needs is kept: the template directories and the syntaxes. Escapers and whitespace settings are ignored.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use askama_parser::{Syntax, SyntaxBuilder};
use serde::Deserialize;

use crate::project::{MAX_WALKED_FILES, Visited};

/// The file name Askama reads its configuration from.
pub const CONFIG_FILE_NAME: &str = "askama.toml";

/// The most template directories one `dirs` pattern expands to.
const MAX_TEMPLATE_DIRS: usize = 64;

/// The name of the syntax Askama defines itself, with the default delimiters.
const DEFAULT_SYNTAX_NAME: &str = "default";

/// The parts of a crate's Askama configuration the analysis uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The template directories, in the order Askama searches them.
    dirs: Vec<PathBuf>,
    /// The syntaxes `askama.toml` defines, by name, with Askama's own `default`.
    syntaxes: HashMap<String, Delimiters>,
    /// The name of the syntax templates use unless they name another.
    default_syntax: String,
}

/// A mistake in `askama.toml`, with the byte range it concerns when that is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// What is wrong.
    pub message: String,
    /// Where in `askama.toml`, when that can be found.
    pub range: Option<Range<usize>>,
}

/// The delimiters of one syntax. Unset ones take Askama's defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
struct Delimiters {
    /// What opens a block tag, `{%` by default.
    block_start: Option<String>,
    /// What closes a block tag, `%}` by default.
    block_end: Option<String>,
    /// What opens an expression, `{{` by default.
    expr_start: Option<String>,
    /// What closes an expression, `}}` by default.
    expr_end: Option<String>,
    /// What opens a comment, `{#` by default.
    comment_start: Option<String>,
    /// What closes a comment, `#}` by default.
    comment_end: Option<String>,
}

impl Delimiters {
    /// The syntax these delimiters make, checked as Askama checks it.
    fn to_syntax(&self) -> Result<Syntax<'_>, String> {
        SyntaxBuilder {
            name: "",
            block_start: self.block_start.as_deref(),
            block_end: self.block_end.as_deref(),
            expr_start: self.expr_start.as_deref(),
            expr_end: self.expr_end.as_deref(),
            comment_start: self.comment_start.as_deref(),
            comment_end: self.comment_end.as_deref(),
        }
        .to_syntax()
    }
}

/// `askama.toml` as written.
#[derive(Default, Deserialize)]
struct RawConfig {
    /// The `[general]` table.
    general: Option<RawGeneral>,
    /// The `[[syntax]]` tables.
    syntax: Option<Vec<RawSyntax>>,
}

/// The `[general]` table of `askama.toml`.
#[derive(Deserialize)]
struct RawGeneral {
    /// The template directories, relative to the crate root.
    dirs: Option<Vec<String>>,
    /// The name of the syntax templates use unless they name another.
    default_syntax: Option<String>,
}

/// One `[[syntax]]` table of `askama.toml`.
#[derive(Deserialize)]
struct RawSyntax {
    /// The name templates choose it by.
    name: String,
    /// Its delimiters.
    #[serde(flatten)]
    delimiters: Delimiters,
}

impl Config {
    /// Askama's configuration when a crate has no `askama.toml`.
    pub fn default_for(crate_root: &Path) -> Self {
        Self {
            dirs: vec![crate_root.join("templates")],
            syntaxes: HashMap::from([(DEFAULT_SYNTAX_NAME.to_owned(), Delimiters::default())]),
            default_syntax: DEFAULT_SYNTAX_NAME.to_owned(),
        }
    }

    /// Reads the crate's `askama.toml`, or gives the defaults when there is none.
    ///
    /// # Errors
    ///
    /// When the file exists but cannot be read, or is not a valid configuration (see `parse`).
    pub fn load(crate_root: &Path) -> Result<Self, ConfigError> {
        match crate::project::read_text_or_error(&crate_root.join(CONFIG_FILE_NAME)) {
            Ok(source) => Self::parse(crate_root, &source),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self::default_for(crate_root))
            }
            Err(error) => Err(ConfigError {
                message: format!("unable to read {CONFIG_FILE_NAME}: {error}"),
                range: None,
            }),
        }
    }

    /// Reads a configuration from the text of an `askama.toml`, with the rules Askama applies.
    ///
    /// # Errors
    ///
    /// When the text is not valid TOML or breaks one of Askama's rules: a syntax defined twice, a syntax with invalid delimiters, or a
    /// default syntax that is not defined. The error has the range of the offending value when it can be found.
    pub fn parse(crate_root: &Path, source: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(source).map_err(|error| ConfigError {
            message: format!("invalid TOML in {CONFIG_FILE_NAME}: {}", error.message()),
            range: error.span(),
        })?;
        let mut config = Self::default_for(crate_root);

        if let Some(general) = raw.general {
            if let Some(dirs) = general.dirs {
                config.dirs = dirs.iter().flat_map(|dir| expand_dir(crate_root, dir)).collect();
            }
            if let Some(default_syntax) = general.default_syntax {
                config.default_syntax = default_syntax;
            }
        }

        for syntax in raw.syntax.unwrap_or_default() {
            if config.syntaxes.contains_key(&syntax.name) {
                return Err(ConfigError::at(source, format!("syntax {:?} is already defined", syntax.name), &syntax.name));
            }
            if let Err(message) = syntax.delimiters.to_syntax() {
                return Err(ConfigError::at(source, message, &syntax.name));
            }
            config.syntaxes.insert(syntax.name, syntax.delimiters);
        }

        if !config.syntaxes.contains_key(&config.default_syntax) {
            let message = format!("default syntax \"{}\" not found", config.default_syntax);
            return Err(ConfigError::at(source, message, &config.default_syntax));
        }
        Ok(config)
    }

    /// The directories templates are found in, in the order Askama searches them.
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// The syntax templates use unless their struct names another.
    pub fn default_syntax(&self) -> Syntax<'_> {
        match self.syntaxes.get(&self.default_syntax).map(Delimiters::to_syntax) {
            Some(Ok(syntax)) => syntax,
            // Unreachable for a configuration built by `parse`, which validates every syntax.
            _ => default_syntax(),
        }
    }

    /// The name a template file goes by in `#[template(path = "...")]`, `extends` and `include`: its path relative to the first template
    /// directory that holds it, with `/` between the parts. `None` for a file outside every template directory.
    pub fn template_name(&self, file: &Path) -> Option<String> {
        self.dirs.iter().find_map(|dir| {
            let relative = file.strip_prefix(dir).ok()?;
            let parts: Option<Vec<&str>> = relative.iter().map(|part| part.to_str()).collect();
            Some(parts?.join("/"))
        })
    }

    /// The file a template name refers to, found the way Askama finds it: beside the template that names it first, then in each template
    /// directory in order.
    pub fn find_template(&self, name: &str, from: Option<&Path>) -> Option<PathBuf> {
        let beside = from.map(|file| file.with_file_name(name));
        beside.into_iter().chain(self.dirs.iter().map(|dir| dir.join(name))).find(|path| path.is_file())
    }

    /// Every file in the template directories, each once, in a stable order: going at most `MAX_DEPTH` directories deep, entering each
    /// directory once (a symbolic link back to one already walked is skipped), and stopping at `MAX_WALKED_FILES`.
    pub fn template_files(&self) -> Vec<PathBuf> {
        /// How deep below a template directory the walk goes.
        const MAX_DEPTH: usize = 16;
        /// Adds the files under `dir` to `files`, in path order.
        fn walk(dir: &Path, depth: usize, visited: &mut Visited, files: &mut Vec<PathBuf>) {
            if !visited.enter(dir) {
                return;
            }
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            let mut entries: Vec<PathBuf> = entries.filter_map(|entry| entry.ok().map(|entry| entry.path())).collect();
            entries.sort();
            for path in entries {
                if files.len() >= MAX_WALKED_FILES {
                    return;
                }
                if path.is_dir() {
                    if depth < MAX_DEPTH {
                        walk(&path, depth + 1, visited, files);
                    }
                } else if path.is_file() {
                    files.push(path);
                }
            }
        }
        let mut files = Vec::new();
        let mut visited = Visited::default();
        for dir in &self.dirs {
            walk(dir, 0, &mut visited, &mut files);
        }
        let mut seen = std::collections::HashSet::new();
        files.retain(|file| seen.insert(file.clone()));
        files
    }
}

impl ConfigError {
    /// An error about a quoted value, placed on its first appearance in the file.
    fn at(source: &str, message: String, value: &str) -> Self {
        let quoted = format!("\"{value}\"");
        let range = source.find(&quoted).map(|start| start..start + quoted.len());
        Self { message, range }
    }
}

/// Askama's built-in delimiters.
pub fn default_syntax() -> Syntax<'static> {
    // The defaults always pass Askama's own validation.
    SyntaxBuilder::default().to_syntax().unwrap_or_else(|_| unreachable!("default delimiters are valid"))
}

/// One `dirs` entry, relative to the crate root. Askama expands an entry with `*` as a glob.
fn expand_dir(crate_root: &Path, dir: &str) -> Vec<PathBuf> {
    let path = crate_root.join(dir);
    if dir.contains('*')
        && let Some(pattern) = path.to_str()
        && let Ok(matches) = glob::glob(pattern)
    {
        // A crate has a few template directories. The limit stops the expansion of a pattern like `/**`, which names every directory on the
        // disk, early rather than walking all of them.
        matches.filter_map(Result::ok).filter(|path| path.is_dir()).take(MAX_TEMPLATE_DIRS).collect()
    } else {
        vec![path]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn root() -> PathBuf {
        PathBuf::from("/crate")
    }

    #[cfg(unix)]
    #[test]
    fn lists_template_files_once_through_symbolic_link_loops() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        dir.write("templates/a.html", "");
        dir.write("templates/sub/b.html", "");
        for name in ["x", "y", "z"] {
            symlink(dir.path("templates"), dir.path(&format!("templates/sub/{name}"))).unwrap();
        }
        let config = Config::default_for(&dir.path(""));
        assert_eq!(config.template_files(), [dir.path("templates/a.html"), dir.path("templates/sub/b.html")]);
    }

    #[test]
    fn defaults_without_a_file() {
        let dir = TempDir::new();
        let config = Config::load(&dir.path("")).unwrap();
        assert_eq!(config.dirs(), [dir.path("templates")]);
        assert_eq!(config.default_syntax().block_start, "{%");
    }

    #[test]
    fn reads_dirs_and_a_default_syntax() {
        let source = r#"
            [general]
            dirs = ["views", "shared"]
            default_syntax = "square"

            [[syntax]]
            name = "square"
            expr_start = "[["
            expr_end = "]]"
        "#;
        let config = Config::parse(&root(), source).unwrap();
        assert_eq!(config.dirs(), [root().join("views"), root().join("shared")]);
        let syntax = config.default_syntax();
        assert_eq!((syntax.expr_start, syntax.expr_end), ("[[", "]]"));
        assert_eq!(syntax.block_start, "{%");
    }

    #[test]
    fn ignores_settings_it_does_not_need() {
        let source = "[general]\nwhitespace = \"suppress\"\n\n[[escaper]]\npath = \"x\"\nextensions = [\"y\"]\n";
        assert!(Config::parse(&root(), source).is_ok());
    }

    #[test]
    fn expands_glob_dirs() {
        let dir = TempDir::new();
        dir.write("templates/a/x.html", "");
        dir.write("templates/b/y.html", "");
        let config = Config::parse(&dir.path(""), "[general]\ndirs = [\"templates/*\"]").unwrap();
        let mut dirs = config.dirs().to_vec();
        dirs.sort();
        assert_eq!(dirs, [dir.path("templates/a"), dir.path("templates/b")]);
    }

    #[test]
    fn reports_invalid_toml_with_its_place() {
        let error = Config::parse(&root(), "[general\n").unwrap_err();
        assert!(error.message.starts_with("invalid TOML in askama.toml"), "{}", error.message);
        assert!(error.range.is_some());
    }

    #[test]
    fn reports_a_missing_default_syntax() {
        let source = "[general]\ndefault_syntax = \"nope\"\n";
        let error = Config::parse(&root(), source).unwrap_err();
        assert_eq!(error.message, "default syntax \"nope\" not found");
        assert_eq!(error.range, Some(27..33));
    }

    #[test]
    fn reports_a_duplicate_syntax() {
        let source = "[[syntax]]\nname = \"a\"\n[[syntax]]\nname = \"a\"\n";
        let error = Config::parse(&root(), source).unwrap_err();
        assert_eq!(error.message, "syntax \"a\" is already defined");
    }

    #[test]
    fn reports_invalid_delimiters() {
        let source = "[[syntax]]\nname = \"a\"\nblock_start = \"\"\n";
        assert!(Config::parse(&root(), source).is_err());
    }

    #[test]
    fn names_templates_relative_to_their_dir() {
        let source = "[general]\ndirs = [\"views\", \"shared\"]";
        let config = Config::parse(&root(), source).unwrap();
        assert_eq!(config.template_name(&root().join("views/a/b.html")).as_deref(), Some("a/b.html"));
        assert_eq!(config.template_name(&root().join("shared/c.html")).as_deref(), Some("c.html"));
        assert_eq!(config.template_name(&root().join("other/d.html")), None);
    }
}
