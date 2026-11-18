//! `askama-lsp`: the language server behind the Askama VS Code extension.
//!
//! It speaks the Language Server Protocol over stdin and stdout. The VS Code client starts it and sends it the templates the user opens.
//! The analysis itself lives in `askama-analysis`; this crate translates between it and LSP. See `docs/design.md` for the design.
//!
//! tower-lsp runs a few requests at once, and they share the server's state behind one lock. Each request holds it only to copy out what it
//! needs (a document's text, a crate's configuration), never while it reads files or parses Rust, so one slow request does not hold up the
//! others. The Rust index has a lock of its own for the same reason.

use std::collections::{HashMap, HashSet};
use std::ops::Range as ByteRange;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use askama_analysis::completion::{self, Completion, KEYWORDS};
use askama_analysis::config::{CONFIG_FILE_NAME, Config, ConfigError};
use askama_analysis::filters::{self, BUILTINS, CrateFilters, FilterUse};
use askama_analysis::lines::{LineIndex, Position as TextPosition};
use askama_analysis::lowering::{self, Template};
use askama_analysis::navigation::{self, FileCache, Project, Reader};
use askama_analysis::outline::{self, Region, RegionKind};
use askama_analysis::probe;
use askama_analysis::ranges::{self, FoldKind};
use askama_analysis::rust::{CrateFacts, Member, MemberKind, RustIndex, TemplateStruct};
use askama_analysis::scopes::{self, Binding, BindingKind, LOOP_MEMBERS};
use askama_analysis::{project, tags, template};
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::request::{GotoImplementationParams, GotoImplementationResponse};
use tower_lsp_server::ls_types::{
    CodeLens, CodeLensOptions, CodeLensParams, Command, CompletionItem, CompletionItemKind, CompletionOptions, CompletionParams, CompletionResponse,
    CompletionTextEdit, Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentLink, DocumentLinkOptions,
    DocumentLinkParams, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, Documentation, FileChangeType, FileSystemWatcher,
    FoldingRange, FoldingRangeKind, FoldingRangeParams, FoldingRangeProviderCapability, GlobPattern, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, HoverProviderCapability, ImplementationProviderCapability, InitializeParams,
    InitializeResult, InitializedParams, Location, MarkupContent, MarkupKind, MessageType, NumberOrString, OneOf, Position,
    PrepareRenameResponse, Range, ReferenceParams, Registration, RenameOptions, RenameParams, SelectionRange, SelectionRangeParams,
    SelectionRangeProviderCapability, ServerCapabilities, ServerInfo, SymbolKind, TextDocumentPositionParams, TextDocumentSyncCapability,
    RelativePattern, TextDocumentSyncKind, TextEdit, Uri, WorkDoneProgressOptions, WorkspaceEdit, WorkspaceSymbol, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

mod analyzer;
mod panics;

use analyzer::Analyzer;
use panics::CatchPanics;

/// The `source` shown beside each diagnostic.
const SOURCE: &str = "askama";

/// How long a crate's rust-analyzer may go unasked before it is stopped, to free its memory. Starting it again takes seconds.
const ANALYZER_IDLE_LIMIT: Duration = Duration::from_secs(10 * 60);

/// How often an idle rust-analyzer is looked for.
const ANALYZER_IDLE_CHECK: Duration = Duration::from_secs(60);

/// How long after an edit a template's type check starts, if no newer edit came.
const TYPE_CHECK_DELAY: Duration = Duration::from_millis(250);

/// How long the list of crates in the workspace folders is reused by workspace symbols, which the editor asks for on every keystroke of the
/// query.
const CRATE_ROOTS_LIFETIME: Duration = Duration::from_secs(10);

/// The language server. Cloning it is cheap and shares everything, so a background task can have its own.
#[derive(Clone)]
struct Backend {
    /// The connection to the editor.
    client: Client,
    /// Whether the client lets the server ask it to watch `askama.toml` files.
    can_watch_files: Arc<AtomicBool>,
    /// Whether the client can watch files under a given directory (LSP's relative patterns), which watching template directories needs.
    can_watch_relative: Arc<AtomicBool>,
    /// Everything else the requests share.
    state: Arc<Mutex<State>>,
    /// What each crate's Rust code says: custom filters and template structs, reread for files that changed on disk. Behind its own lock,
    /// since reading it may parse Rust.
    rust: Arc<Mutex<RustIndex>>,
}

/// What the requests share.
#[derive(Default)]
struct State {
    /// The workspace folders the client opened, searched for crates by workspace symbols.
    folders: Vec<PathBuf>,
    /// The crates found in `folders`, and when they were looked for.
    crate_roots: Option<(Instant, Vec<PathBuf>)>,
    /// The documents the editor has open, by URI.
    documents: HashMap<Uri, Document>,
    /// Each crate's configuration, by crate root, read on first use and dropped when its `askama.toml` changes.
    configs: HashMap<PathBuf, Arc<Config>>,
    /// Whether completion may ask rust-analyzer about types (`askama.typeCompletion`).
    type_completion: bool,
    /// The rust-analyzer executable to start.
    rust_analyzer: String,
    /// What the private rust-analyzers may run of a crate's code, from the user's rust-analyzer settings.
    permissions: analyzer::Permissions,
    /// Whether the client shows progress the server starts.
    can_show_progress: bool,
    /// Each crate's rust-analyzer, by crate root, started on first use.
    analyzers: HashMap<PathBuf, Arc<tokio::sync::OnceCell<Option<Arc<Analyzer>>>>>,
    /// The type check waiting or running for each template, cancelled by the next edit.
    type_checks: HashMap<Uri, tokio::task::AbortHandle>,
    /// The latest type errors of each template, kept in what the check publishes until the next type check replaces them.
    type_errors: HashMap<Uri, Vec<Diagnostic>>,
    /// The diagnostics the check last published for each template, with the version.
    checked: HashMap<Uri, (i32, Vec<Diagnostic>)>,
    /// The file cache of each crate whose template directories are watched, by crate root, cleared on every change there.
    file_caches: HashMap<PathBuf, Arc<Mutex<FileCache>>>,
    /// The crates whose template directories have been asked to be watched (or could not be), so each is asked once per configuration.
    watched_roots: HashSet<PathBuf>,
    /// The crates whose configuration was read and whose template directories are still to be watched.
    unwatched_roots: Vec<PathBuf>,
    /// How many watcher registrations have been made, for unique registration ids.
    registrations: usize,
}

/// An open document.
struct Document {
    /// Its text in the editor, saved or not.
    text: Arc<str>,
    /// The editor's version of that text.
    version: i32,
    /// A Rust file the client sends for the struct-to-template features, not a template.
    rust: bool,
}

/// A position in an open template, with what the requests about it need.
struct Cursor<'a> {
    /// The template's URI.
    uri: &'a Uri,
    /// Its crate's configuration.
    config: &'a Config,
    /// Its text.
    text: &'a str,
    /// The byte offset of the position.
    offset: usize,
}

/// What completion does next: answer now, or ask rust-analyzer about a probe first.
enum CompletionStep {
    /// The items, from the server's own analysis.
    Done(Option<Vec<CompletionItem>>),
    /// The probe to ask about.
    Probe(ProbePlan),
}

/// Where a probe for a template position comes from (see `Backend::probe_scope`).
struct ProbeScope {
    /// The crate's root.
    root: PathBuf,
    /// The struct whose file the probe goes in.
    template_struct: TemplateStruct,
    /// The statements that bring the template's names into scope.
    setup: lowering::Setup,
    /// The template's expressions the probe was asked to write (see `probe_scope`), in Rust as the probe's method sees them.
    lowered: Vec<String>,
    /// The struct's file on disk.
    rust_text: String,
}

/// rust-analyzer's diagnostic codes that are type errors in a template expression: no such field or method, the wrong number of arguments,
/// a private field or method. It gives rustc's error codes where they exist (`E0599`), and its own names elsewhere or in older releases.
const TYPE_ERROR_CODES: &[&str] = &[
    "E0559", "E0609", "E0599", "E0061", "E0616", "E0624",
    "unresolved-field", "unresolved-method", "mismatched-arg-count", "private-field", "private-assoc-item",
];

/// What each expression line of a type probe starts with, before the expression's code.
const TYPE_PROBE_PREFIX: &str = "let _ = &(";

/// A probe with every expression of a template, for its type errors.
struct TypeProbe {
    /// The crate's rust-analyzer, running and ready.
    analyzer: Arc<Analyzer>,
    /// The struct's Rust file, which the probe replaces in rust-analyzer's memory.
    file: Arc<Path>,
    /// The probed file's text.
    text: String,
    /// The template's version the probe was made from.
    version: i32,
    /// The template's text the probe was made from.
    template_text: Arc<str>,
    /// The probe line of each expression, and the expression's way back to the template.
    blocks: Vec<(u32, lowering::Mapped)>,
}

/// A probe for a name in a template path, and the name's range in the template.
struct PathProbe {
    /// The crate's root.
    root: PathBuf,
    /// The struct's Rust file, which the probe replaces in rust-analyzer's memory.
    file: Arc<Path>,
    /// The probe, positioned at the name.
    probe: probe::Probe,
    /// The name's range in the template.
    range: Range,
}

/// A probe to show the crate's rust-analyzer, and where its answers go in the template.
struct ProbePlan {
    /// The crate's root.
    root: PathBuf,
    /// The struct's Rust file, which the probe replaces in rust-analyzer's memory.
    file: Arc<Path>,
    /// The probed file's text.
    text: String,
    /// Where to ask for completion, in `text`.
    position: TextPosition,
    /// The range in the template that a chosen item replaces.
    range: Range,
}

/// Diagnostics computed while holding the state lock, published after releasing it.
struct Publication {
    /// The file they are for.
    uri: Uri,
    /// The diagnostics.
    diagnostics: Vec<Diagnostic>,
    /// The document version they were computed for, `None` for a file that is not open.
    version: Option<i32>,
}

impl LanguageServer for Backend {
    async fn initialize(&self, mut params: InitializeParams) -> Result<InitializeResult> {
        let capabilities = &params.capabilities;
        let can_watch_files = capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.did_change_watched_files.as_ref())
            .and_then(|watched| watched.dynamic_registration)
            .unwrap_or(false);
        let can_watch_relative = capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.did_change_watched_files.as_ref())
            .and_then(|watched| watched.relative_pattern_support)
            .unwrap_or(false);
        let can_show_progress = capabilities.window.as_ref().and_then(|window| window.work_done_progress).unwrap_or(false);
        self.can_watch_files.store(can_watch_files, Ordering::Relaxed);
        self.can_watch_relative.store(can_watch_files && can_watch_relative, Ordering::Relaxed);
        // Set by the client from `askama.typeCompletion`, `askama.rustAnalyzer.path` and the user's rust-analyzer settings for build
        // scripts and procedural macros.
        let options = params.initialization_options.take().unwrap_or_default();
        let folders = params
            .workspace_folders
            .unwrap_or_default()
            .iter()
            .filter_map(|folder| folder.uri.to_file_path().map(std::borrow::Cow::into_owned))
            .collect();
        {
            let mut state = self.lock();
            state.type_completion = options["typeCompletion"].as_bool().unwrap_or(true);
            let defaults = analyzer::Permissions::default();
            state.permissions = analyzer::Permissions {
                build_scripts: options["buildScripts"].as_bool().unwrap_or(defaults.build_scripts),
                proc_macros: options["procMacros"].as_bool().unwrap_or(defaults.proc_macros),
            };
            options["rustAnalyzerPath"].as_str().filter(|path| !path.is_empty()).unwrap_or("rust-analyzer").clone_into(&mut state.rust_analyzer);
            state.can_show_progress = can_show_progress;
            state.folders = folders;
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
                document_symbol_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                document_link_provider: Some(DocumentLinkOptions { resolve_provider: Some(false), work_done_progress_options: WorkDoneProgressOptions::default() }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    // A space opens the list only where a name belongs, as in `{{ ` or `{% if `. A `.` opens members where they are known,
                    // as after `loop.`.
                    trigger_characters: Some(vec!["|".to_owned(), " ".to_owned(), ".".to_owned()]),
                    ..CompletionOptions::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                code_lens_provider: Some(CodeLensOptions { resolve_provider: Some(false) }),
                rename_provider: Some(OneOf::Right(RenameOptions { prepare_provider: Some(true), work_done_progress_options: WorkDoneProgressOptions::default() })),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: env!("CARGO_PKG_NAME").to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            ..InitializeResult::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(
                MessageType::INFO,
                concat!("askama-lsp ", env!("CARGO_PKG_VERSION"), " started."),
            )
            .await;
        if self.can_watch_files.load(Ordering::Relaxed) {
            self.watch_config_files().await;
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let document = params.text_document;
        // Store the document before the first `.await`: requests run concurrently with notifications, and a client asks about a document
        // right after opening it.
        let text = Arc::from(document.text);
        let rust = document.language_id == "rust";
        self.lock().documents.insert(document.uri.clone(), Document { text, version: document.version, rust });
        self.client
            .log_message(MessageType::INFO, format!("Opened {}.", document.uri.as_str()))
            .await;
        self.check(vec![document.uri.clone()]).await;
        self.schedule_type_check(document.uri);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // Full synchronization: the last change holds the whole new text.
        let Some(change) = params.content_changes.into_iter().next_back() else {
            return;
        };
        let uri = params.text_document.uri;
        {
            let mut state = self.lock();
            let rust = state.documents.get(&uri).is_some_and(|document| document.rust);
            state.documents.insert(uri.clone(), Document { text: Arc::from(change.text), version: params.text_document.version, rust });
        }
        self.check(vec![uri.clone()]).await;
        self.schedule_type_check(uri);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        {
            let mut state = self.lock();
            state.documents.remove(&uri);
            state.checked.remove(&uri);
            state.type_errors.remove(&uri);
            if let Some(type_check) = state.type_checks.remove(&uri) {
                type_check.abort();
            }
        }
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let mut cleared = Vec::new();
        let open = {
            let mut state = self.lock();
            for change in params.changes {
                let Some(path) = change.uri.to_file_path().map(std::borrow::Cow::into_owned) else {
                    continue;
                };
                if path.file_name().is_some_and(|name| name == CONFIG_FILE_NAME) {
                    // The configuration is read again, and its template directories watched again, on next use.
                    if let Some(root) = path.parent() {
                        state.configs.remove(root);
                        state.file_caches.remove(root);
                        state.watched_roots.remove(root);
                    }
                    if change.typ == FileChangeType::DELETED {
                        cleared.push(change.uri);
                    }
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    self.rust.lock().unwrap_or_else(PoisonError::into_inner).changed(&path);
                } else {
                    // A change in a template directory: everything known of that crate's templates may have changed.
                    let State { configs, file_caches, .. } = &*state;
                    for (root, cache) in file_caches {
                        if configs.get(root).is_some_and(|config| config.dirs().iter().any(|dir| path.starts_with(dir))) {
                            cache.lock().unwrap_or_else(PoisonError::into_inner).clear();
                        }
                    }
                }
            }
            state.documents.keys().cloned().collect()
        };
        for uri in cleared {
            self.client.publish_diagnostics(uri, Vec::new(), None).await;
        }
        // Rechecking reloads each configuration and republishes its diagnostics.
        self.check(open).await;
    }

    async fn document_symbol(&self, params: DocumentSymbolParams) -> Result<Option<DocumentSymbolResponse>> {
        Ok(self.with_structure(&params.text_document.uri, |structure| {
            let symbols = structure.regions.iter().map(|region| symbol(&structure.text, &structure.lines, region)).collect();
            DocumentSymbolResponse::Nested(symbols)
        }))
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        Ok(self.with_structure(&params.text_document.uri, |structure| {
            ranges::folds(&structure.text, &structure.lines, &structure.tags, &structure.regions)
                .into_iter()
                .map(|fold| FoldingRange {
                    start_line: fold.start_line,
                    end_line: fold.end_line,
                    kind: Some(match fold.kind {
                        FoldKind::Region => FoldingRangeKind::Region,
                        FoldKind::Comment => FoldingRangeKind::Comment,
                    }),
                    ..FoldingRange::default()
                })
                .collect()
        }))
    }

    async fn selection_range(&self, params: SelectionRangeParams) -> Result<Option<Vec<SelectionRange>>> {
        Ok(self.with_structure(&params.text_document.uri, |structure| {
            params
                .positions
                .iter()
                .map(|&position| {
                    let offset = structure.lines.offset(&structure.text, text_position(position));
                    let chain = ranges::selection(&structure.text, &structure.tags, &structure.regions, offset);
                    // LSP wants the smallest range first, each pointing to the one around it.
                    let mut selection: Option<SelectionRange> = None;
                    for range in chain.into_iter().rev() {
                        let range = to_range(&structure.text, &structure.lines, range);
                        selection = Some(SelectionRange { range, parent: selection.map(Box::new) });
                    }
                    selection.unwrap_or(SelectionRange { range: Range { start: position, end: position }, parent: None })
                })
                .collect()
        }))
    }

    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        Ok(self.with_project(&params.text_document.uri, |project, file, text| {
            let lines = LineIndex::new(text);
            project
                .resolved_paths(file, text)
                .into_iter()
                .filter_map(|(path, target)| {
                    Some(DocumentLink {
                        range: to_range(text, &lines, path.name.range),
                        target: Some(Uri::from_file_path(target?)?),
                        tooltip: Some(format!("Open {}", path.name.name)),
                        data: None,
                    })
                })
                .collect()
        }))
    }

    async fn goto_definition(&self, params: GotoDefinitionParams) -> Result<Option<GotoDefinitionResponse>> {
        let position = params.text_document_position_params;
        let found = self
            .with_project(&position.text_document.uri, |project, file, text| {
                let offset = LineIndex::new(text).offset(text, text_position(position.position));
                project.definition(file, text, offset).and_then(|found| location(&found))
            })
            .flatten()
            .or_else(|| self.filter_definition(&position))
            .or_else(|| self.binding_definition(&position))
            .or_else(|| self.member_definition(&position));
        let found = match found {
            Some(found) => Some(found),
            None => self.path_definition(&position).await,
        };
        Ok(found.or_else(|| self.template_of_struct(&position)).map(GotoDefinitionResponse::Scalar))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let items = match self.completion_step(&params) {
            CompletionStep::Done(items) => items,
            CompletionStep::Probe(plan) => self.probe_members(plan).await,
        };
        Ok(items.map(CompletionResponse::Array))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let position = &params.text_document_position_params;
        let Some((used, range, facts)) = self.filter_at(position) else {
            if let Some(hover) = self.binding_hover(position) {
                return Ok(Some(hover));
            }
            if let Some(hover) = self.path_hover(position).await {
                return Ok(Some(hover));
            }
            return Ok(self.members_at(position).map(|(found, range)| {
                let value = found
                    .iter()
                    .map(|(owner, member)| {
                        let code = match member.kind {
                            MemberKind::Field => format!("{}: {}", member.name, member.detail),
                            MemberKind::Method => member.detail.clone(),
                        };
                        format!("```rust\n{code}\n```\n\n{}", member_docs(member, &[owner.as_str()]))
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n");
                markdown_hover(value, Some(range))
            }));
        };
        let text = if let Some(custom) = facts.filters.find(&used.name).filter(|_| !used.qualified) {
            let mut text = format!("```rust\n{}\n```", custom.signature);
            if !custom.docs.is_empty() {
                text.push_str("\n\n");
                text.push_str(&custom.docs);
            }
            text
        } else if let Some(builtin) = filters::builtin(&used.name).filter(|_| !used.qualified) {
            format!("`{}` (Askama built-in filter)\n\n{}", builtin.usage, builtin.description)
        } else {
            return Ok(None);
        };
        Ok(Some(markdown_hover(text, Some(range))))
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let uri = &params.text_document.uri;
        let Some(file) = uri.to_file_path().map(std::borrow::Cow::into_owned) else {
            return Ok(None);
        };
        let config = {
            let mut state = self.lock();
            if !state.documents.get(uri).is_some_and(|document| document.rust) {
                return Ok(None);
            }
            state.config_for(uri, &mut Vec::new())
        };
        let Some(disk) = project::read_text(&file) else {
            return Ok(None);
        };
        let lines = LineIndex::new(&disk);
        let facts = self.facts(uri);
        let lenses = facts
            .structs
            .iter()
            .filter(|template_struct| navigation::same_file(&template_struct.file, &file))
            .map(|template_struct| {
                let target = config.find_template(&template_struct.template, None).and_then(Uri::from_file_path);
                let command = match target {
                    Some(target) => Command {
                        title: format!("Open template {}", template_struct.template),
                        command: "askama.openTemplate".to_owned(),
                        arguments: Some(vec![serde_json::Value::String(target.as_str().to_owned())]),
                    },
                    None => Command { title: format!("Template {} not found", template_struct.template), command: String::new(), arguments: None },
                };
                CodeLens { range: to_range(&disk, &lines, template_struct.range.clone()), command: Some(command), data: None }
            })
            .collect();
        Ok(Some(lenses))
    }

    async fn goto_implementation(&self, params: GotoImplementationParams) -> Result<Option<GotoImplementationResponse>> {
        Ok(self
            .block_overrides(&params.text_document_position_params)
            .map(|(_, overrides)| GotoDefinitionResponse::Array(overrides)))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let include_declaration = params.context.include_declaration;
        let position = &params.text_document_position;
        if let Some((declaration, overrides)) = self.block_overrides(position) {
            let declaration = include_declaration.then_some(declaration).flatten();
            return Ok(Some(declaration.into_iter().chain(overrides).collect()));
        }
        Ok(self.binding_occurrences(position).map(|(uri, text, ranges)| {
            let lines = LineIndex::new(&text);
            let skip = usize::from(!include_declaration);
            ranges.into_iter().skip(skip).map(|range| Location { uri: uri.clone(), range: to_range(&text, &lines, range) }).collect()
        }))
    }

    async fn prepare_rename(&self, params: TextDocumentPositionParams) -> Result<Option<PrepareRenameResponse>> {
        Ok(self.with_structure(&params.text_document.uri, |structure| {
            let offset = structure.lines.offset(&structure.text, text_position(params.position));
            let binding = scopes::binding_at(&structure.text, &structure.tags, &structure.bindings, offset).filter(|binding| binding.kind != BindingKind::Loop)?;
            // The name under the cursor: the binding itself or one of its uses.
            let occurrences = scopes::occurrences(&structure.text, &structure.tags, &structure.bindings, binding);
            let here = occurrences.into_iter().find(|range| range.start <= offset && offset <= range.end)?;
            Some(PrepareRenameResponse::Range(to_range(&structure.text, &structure.lines, here)))
        })
        .flatten())
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let new_name = params.new_name;
        let valid = new_name.starts_with(|c: char| c.is_alphabetic() || c == '_') && new_name.chars().all(|c| c.is_alphanumeric() || c == '_');
        if !valid {
            return Err(tower_lsp_server::jsonrpc::Error::invalid_params(format!("`{new_name}` is not a valid name")));
        }
        Ok(self.binding_occurrences(&params.text_document_position).map(|(uri, text, ranges)| {
            let lines = LineIndex::new(&text);
            let edits = ranges.into_iter().map(|range| TextEdit { range: to_range(&text, &lines, range), new_text: new_name.clone() }).collect();
            WorkspaceEdit { changes: Some(HashMap::from([(uri, edits)])), ..WorkspaceEdit::default() }
        }))
    }

    async fn symbol(&self, params: WorkspaceSymbolParams) -> Result<Option<WorkspaceSymbolResponse>> {
        let query = params.query.to_lowercase();
        let mut symbols = Vec::new();
        for root in self.crate_roots() {
            let config = self.lock().config(&root, &mut Vec::new());
            let cache = self.file_cache(&root);
            let read = |path: &Path| self.read_in(path, cache.as_deref());
            for symbol in new_project(&config, &read, cache.as_deref()).symbols() {
                if !is_subsequence(&query, &symbol.name.to_lowercase()) {
                    continue;
                }
                let Some(location) = location(&symbol.location) else {
                    continue;
                };
                symbols.push(WorkspaceSymbol {
                    name: symbol.name,
                    kind: match symbol.kind {
                        navigation::SymbolKind::Block => SymbolKind::NAMESPACE,
                        navigation::SymbolKind::Macro => SymbolKind::FUNCTION,
                    },
                    tags: None,
                    container_name: Some(symbol.template),
                    location: OneOf::Left(location),
                    data: None,
                });
            }
        }
        self.watch_template_dirs().await;
        Ok(Some(WorkspaceSymbolResponse::Nested(symbols)))
    }
}

/// An open template's text with its tags and regions, for the structural requests.
struct Structure {
    /// The template's text.
    text: Arc<str>,
    /// The line index of `text`.
    lines: LineIndex,
    /// Its tags.
    tags: Vec<tags::Tag>,
    /// Its regions.
    regions: Vec<Region>,
    /// The names it binds.
    bindings: Vec<Binding>,
}

impl Backend {
    /// The shared state, locked. Every lock is short and never held across an `.await`; a panic while it was held does not stop the others
    /// from using it.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the Rust code of the crate a document belongs to says.
    fn facts(&self, uri: &Uri) -> Arc<CrateFacts> {
        match uri.to_file_path().and_then(|path| project::crate_root(&path)) {
            Some(root) => self.rust.lock().unwrap_or_else(PoisonError::into_inner).crate_facts(&root),
            None => Arc::default(),
        }
    }

    /// A template's text: the editor's, if it is open, otherwise the file's. The file is read without the state locked.
    fn read(&self, path: &Path) -> Option<Arc<str>> {
        self.read_in(path, None)
    }

    /// Like `read`, taking the file's text from `cache` when there is one, so a file is read from disk once until the cache is cleared.
    fn read_in(&self, path: &Path, cache: Option<&Mutex<FileCache>>) -> Option<Arc<str>> {
        let open = self.lock().open_text(path);
        open.or_else(|| match cache {
            Some(cache) => cache.lock().unwrap_or_else(PoisonError::into_inner).text(path),
            None => project::read_text(path).map(Arc::from),
        })
    }

    /// The file cache kept for the crate at `root`, if its template directories are watched (see `watch_template_dirs`).
    fn file_cache(&self, root: &Path) -> Option<Arc<Mutex<FileCache>>> {
        self.lock().file_caches.get(root).cloned()
    }

    /// An open template's text and its crate's configuration. `None` for a Rust file or a document the server does not have.
    fn template(&self, uri: &Uri) -> Option<(Arc<str>, Arc<Config>)> {
        let mut state = self.lock();
        let text = state.template_text(uri)?;
        // The configuration is normally cached by the check on open; diagnostics from reading it here would repeat that check's, so they
        // are dropped.
        Some((text, state.config_for(uri, &mut Vec::new())))
    }

    /// Every crate in the workspace folders and every crate with an open template. The folders are searched again only once
    /// `CRATE_ROOTS_LIFETIME` has passed.
    fn crate_roots(&self) -> Vec<PathBuf> {
        let (folders, found, configured, open) = {
            let state = self.lock();
            let found = state.crate_roots.as_ref().filter(|(when, _)| when.elapsed() < CRATE_ROOTS_LIFETIME).map(|(_, roots)| roots.clone());
            let open: Vec<Uri> = state.documents.iter().filter(|(_, document)| !document.rust).map(|(uri, _)| uri.clone()).collect();
            (state.folders.clone(), found, state.configs.keys().cloned().collect::<Vec<_>>(), open)
        };
        let mut roots = found.unwrap_or_else(|| {
            let roots: Vec<PathBuf> = folders.iter().flat_map(|folder| project::crates_under(folder)).collect();
            self.lock().crate_roots = Some((Instant::now(), roots.clone()));
            roots
        });
        roots.extend(configured);
        // An open template's crate counts even before its check has read the crate's configuration, which a request right after opening
        // it can come before.
        roots.extend(open.iter().filter_map(|uri| uri.to_file_path().and_then(|path| project::crate_root(&path))));
        roots.sort();
        roots.dedup();
        roots
    }

    /// Asks the client to report changes to any `askama.toml` and any Rust file.
    /// Once Rust files are watched, the Rust index trusts what it has read until a change is reported, rather than checking every file of
    /// `src` on every request.
    async fn watch_config_files(&self) {
        let globs = [format!("**/{CONFIG_FILE_NAME}"), "**/*.rs".to_owned()];
        let watchers = globs.into_iter().map(|glob| FileSystemWatcher { glob_pattern: GlobPattern::String(glob), kind: None }).collect();
        match self.register_watchers("askama-files", watchers).await {
            Ok(()) => self.rust.lock().unwrap_or_else(PoisonError::into_inner).set_watched(true),
            Err(error) => {
                self.client
                    .log_message(MessageType::WARNING, format!("Could not watch {CONFIG_FILE_NAME} and Rust files: {error}"))
                    .await;
            }
        }
    }

    /// Asks the client to watch the template directories of each crate whose configuration was read since the last call, and keeps a file
    /// cache for each crate it watches. A crate is watched only if every one of its template directories exists: a watcher on a directory
    /// that does not exist yet would miss the files made in it.
    async fn watch_template_dirs(&self) {
        if !self.can_watch_relative.load(Ordering::Relaxed) {
            return;
        }
        let unwatched: Vec<(PathBuf, Vec<PathBuf>, usize)> = {
            let mut state = self.lock();
            let roots = std::mem::take(&mut state.unwatched_roots);
            let mut found = Vec::new();
            for root in roots {
                if !state.watched_roots.insert(root.clone()) {
                    continue;
                }
                if let Some(config) = state.configs.get(&root) {
                    let dirs = config.dirs().to_vec();
                    state.registrations += 1;
                    found.push((root, dirs, state.registrations));
                }
            }
            found
        };
        for (root, dirs, number) in unwatched {
            if dirs.is_empty() || !dirs.iter().all(|dir| dir.is_dir()) {
                continue;
            }
            let watchers = dirs
                .iter()
                .filter_map(|dir| {
                    let base_uri = Uri::from_file_path(dir)?;
                    Some(FileSystemWatcher { glob_pattern: GlobPattern::Relative(RelativePattern { base_uri: OneOf::Right(base_uri), pattern: "**/*".to_owned() }), kind: None })
                })
                .collect();
            if self.register_watchers(&format!("askama-templates-{number}"), watchers).await.is_ok() {
                self.lock().file_caches.insert(root, Arc::default());
            }
        }
    }

    /// Registers file watchers with the client under the registration id `id`.
    async fn register_watchers(&self, id: &str, watchers: Vec<FileSystemWatcher>) -> std::result::Result<(), String> {
        let options = DidChangeWatchedFilesRegistrationOptions { watchers };
        let register_options = serde_json::to_value(options).map_err(|error| error.to_string())?;
        let registration = Registration { id: id.to_owned(), method: "workspace/didChangeWatchedFiles".to_owned(), register_options: Some(register_options) };
        self.client.register_capability(vec![registration]).await.map_err(|error| error.to_string())
    }

    /// The completion items that need nothing but the server's own analysis, or the probe to ask rust-analyzer when they need a type.
    fn completion_step(&self, params: &CompletionParams) -> CompletionStep {
        let position = &params.text_document_position;
        let uri = &position.text_document.uri;
        let Some((text, config)) = self.template(uri) else {
            return CompletionStep::Done(None);
        };
        let lines = LineIndex::new(&text);
        let offset = lines.offset(&text, text_position(position.position));
        let after_space = params.context.as_ref().and_then(|context| context.trigger_character.as_deref()) == Some(" ");
        let found = if after_space {
            completion::after_space(&text, &config.default_syntax(), offset)
        } else {
            completion::at(&text, &config.default_syntax(), offset)
        };
        let Some(found) = found else {
            return CompletionStep::Done(None);
        };
        let cursor = Cursor { uri, config: &config, text: &text, offset };
        let items = match found {
            Completion::Filter { replace } => Some(self.filter_items(uri, to_range(&text, &lines, replace))),
            Completion::Variable { replace } => self.variable_items(&cursor, to_range(&text, &lines, replace)),
            Completion::Member { object, object_range, replace } => {
                let range = to_range(&text, &lines, replace.clone());
                let tags = tags::scan(&text, &config.default_syntax());
                let bindings = scopes::bindings(&text, &tags, &outline::regions(&text, &tags));
                let in_loop = scopes::visible_at(&bindings, offset).iter().any(|binding| binding.kind == BindingKind::Loop && binding.name == object);
                if !in_loop {
                    return self.completion_probe(&cursor, object_range, &text[replace], range).map_or(CompletionStep::Done(None), CompletionStep::Probe);
                }
                Some(loop_items(range))
            }
            Completion::Keyword { replace, closer } => Some(keyword_items(closer, to_range(&text, &lines, replace))),
        };
        CompletionStep::Done(items)
    }

    /// The crate's custom filters, then Askama's built-in ones.
    fn filter_items(&self, uri: &Uri, range: Range) -> Vec<CompletionItem> {
        let facts = self.facts(uri);
        let custom = facts.filters.filters.iter().map(|filter| CompletionItem {
            detail: Some(filter.signature.clone()),
            documentation: Some(markdown(filter.docs.clone())),
            sort_text: Some(format!("0{}", filter.name)),
            ..item(&filter.name, CompletionItemKind::FUNCTION, range)
        });
        let builtins = BUILTINS.iter().map(|builtin| CompletionItem {
            detail: Some(format!("{} (Askama)", builtin.usage)),
            documentation: Some(markdown(builtin.description.to_owned())),
            sort_text: Some(format!("1{}", builtin.name)),
            ..item(builtin.name, CompletionItemKind::FUNCTION, range)
        });
        custom.chain(builtins).collect()
    }

    /// The names the template binds visible at the cursor, nearest first, then the members of its structs. A member only some of the
    /// structs have comes after the others.
    fn variable_items(&self, cursor: &Cursor<'_>, range: Range) -> Option<Vec<CompletionItem>> {
        let file = cursor.uri.to_file_path()?;
        let facts = self.facts(cursor.uri);
        let cache = project::crate_root(&file).and_then(|root| self.file_cache(&root));
        let read = |path: &Path| self.read_in(path, cache.as_deref());
        let structs = new_project(cursor.config, &read, cache.as_deref()).structs_for(&file, &facts.structs);
        let tags = tags::scan(cursor.text, &cursor.config.default_syntax());
        let bindings = scopes::bindings(cursor.text, &tags, &outline::regions(cursor.text, &tags));
        let mut items: Vec<CompletionItem> = scopes::visible_at(&bindings, cursor.offset)
            .into_iter()
            .enumerate()
            .map(|(depth, binding)| CompletionItem {
                detail: Some(binding_summary(binding)),
                sort_text: Some(format!("0{depth:03}{}", binding.name)),
                ..item(&binding.name, CompletionItemKind::VARIABLE, range)
            })
            .collect();
        for template_struct in &structs {
            for member in &template_struct.members {
                if items.iter().any(|item| item.label == member.name) {
                    continue;
                }
                let owners: Vec<&str> = structs.iter().filter(|other| other.member(&member.name).is_some()).map(|other| other.name.as_str()).collect();
                let everywhere = owners.len() == structs.len();
                let detail = if everywhere { member.detail.clone() } else { format!("{} (only in {})", member.detail, owners.join(", ")) };
                let kind = match member.kind {
                    MemberKind::Field => CompletionItemKind::FIELD,
                    MemberKind::Method => CompletionItemKind::METHOD,
                };
                items.push(CompletionItem {
                    detail: Some(detail),
                    documentation: Some(markdown(member_docs(member, &owners))),
                    sort_text: Some(format!("{}{}", if everywhere { 1 } else { 2 }, member.name)),
                    ..item(&member.name, kind, range)
                });
            }
        }
        Some(items)
    }

    /// Where a probe for a template position comes from: the struct whose file it goes in, the statements that bring the template's names
    /// into scope there, and whether `first` (the first name of the path asked about) is one of those names rather than a struct member.
    ///
    /// The statements are the template's loops, `let`s and patterns around the cursor, as Askama writes them. Inside a macro, whose
    /// arguments get their values where it is called, they start from the one place that calls it (none if there are several) and pass the
    /// arguments as Askama does. `None` if any of it cannot be written in Rust.
    fn probe_scope(&self, cursor: &Cursor<'_>, first: &str, expressions: &[ByteRange<usize>]) -> Option<ProbeScope> {
        if !self.lock().type_completion {
            return None;
        }
        let Cursor { uri, config, text, offset } = *cursor;
        let file = uri.to_file_path()?.into_owned();
        let root = project::crate_root(&file)?;
        let facts = self.facts(uri);
        let cache = self.file_cache(&root);
        let read = |path: &Path| self.read_in(path, cache.as_deref());
        let project = new_project(config, &read, cache.as_deref());
        let syntax = config.default_syntax();

        let tags = tags::scan(text, &syntax);
        let regions = outline::regions(text, &tags);
        let bindings = scopes::bindings(text, &tags, &regions);
        let local = scopes::visible_at(&bindings, offset).iter().any(|binding| binding.name == first);
        let macro_body = Region::walk(&regions)
            .filter(|region| region.kind == RegionKind::Macro && region.opener.end <= offset && region.closer.as_ref().is_none_or(|closer| offset <= closer.start))
            .min_by_key(|region| region.range.len());

        let lower = |template: &Template<'_>| expressions.iter().map(|range| template.expression(range.clone(), range.start)).collect::<Option<Vec<_>>>();
        let (template_struct, setup, lowered) = match macro_body {
            Some(macro_region) if local => {
                let name = macro_region.label.strip_prefix("macro")?.trim().split(|c: char| !(c.is_alphanumeric() || c == '_')).next()?.to_owned();
                let mut parameters: Vec<&Binding> = bindings
                    .iter()
                    .filter(|binding| binding.kind == BindingKind::MacroArgument && macro_region.opener.contains(&binding.range.start))
                    .collect();
                parameters.sort_by_key(|binding| binding.range.start);
                let parameters: Vec<String> = parameters.into_iter().map(|binding| binding.name.clone()).collect();

                let mut sites = project.macro_call_sites(&file, &name);
                if sites.len() != 1 {
                    return None;
                }
                let (site_file, site_text, call_range) = sites.pop()?;
                let template_struct = project.structs_for(&site_file, &facts.structs).into_iter().next()?.clone();
                let is_member = |name: &str| template_struct.member(name).is_some();
                let site_tags = tags::scan(&site_text, &syntax);
                let site_regions = outline::regions(&site_text, &site_tags);
                let site_bindings = scopes::bindings(&site_text, &site_tags, &site_regions);
                let site = Template { source: &site_text, tags: &site_tags, regions: &site_regions, bindings: &site_bindings, is_member: &is_member };
                let call = tags::starting_at(&site_tags, call_range.start)?;
                let mut setup = site.setup(call_range.start, 0)?;
                setup.extend(&site.macro_arguments(lowering::call_arguments(&site_text, call)?, &parameters)?);
                // A macro's body sees its arguments and its own names, not the struct's.
                let no_members = |_: &str| false;
                let body = Template { source: text, tags: &tags, regions: &regions, bindings: &bindings, is_member: &no_members };
                setup.extend(&body.setup(offset, macro_region.opener.end)?);
                let lowered = lower(&body)?;
                (template_struct, setup, lowered)
            }
            Some(_) => return None,
            None => {
                let structs = project.structs_for(&file, &facts.structs);
                let template_struct = if local { structs.first() } else { structs.iter().find(|found| found.member(first).is_some()) };
                let template_struct = (*template_struct?).clone();
                let is_member = |name: &str| template_struct.member(name).is_some();
                let here = Template { source: text, tags: &tags, regions: &regions, bindings: &bindings, is_member: &is_member };
                let setup = here.setup(offset, 0)?;
                let lowered = lower(&here)?;
                (template_struct, setup, lowered)
            }
        };
        // The struct's file as on disk, where the index found the struct.
        let rust_text = project::read_text(&template_struct.file)?;
        Some(ProbeScope { root, template_struct, setup, lowered, rust_text })
    }

    /// The probe for the members of the chain at `object` (`order.customer`, `order.items.first()`), with `typed` the part of the member's
    /// name typed so far. The chain is lowered as any expression is, so names in a call's arguments are the struct's or the template's as
    /// they are elsewhere.
    fn completion_probe(&self, cursor: &Cursor<'_>, object: ByteRange<usize>, typed: &str, range: Range) -> Option<ProbePlan> {
        let first = cursor.text[object.clone()].split(|c: char| !(c.is_alphanumeric() || c == '_')).next()?;
        let scope = self.probe_scope(cursor, first, std::slice::from_ref(&object))?;
        let probe = probe::members_in(&scope.rust_text, &scope.template_struct, &scope.setup, &scope.lowered[0], typed);
        Some(ProbePlan { root: scope.root, file: scope.template_struct.file, text: probe.text, position: probe.position, range })
    }

    /// For the name at a position that follows a `.` in an expression (`customer` in `order.customer.name`): the probe whose position is
    /// that name in Rust, and its range in the template.
    fn path_probe(&self, position: &TextDocumentPositionParams) -> Option<PathProbe> {
        let uri = &position.text_document.uri;
        let (text, config) = self.template(uri)?;
        let lines = LineIndex::new(&text);
        let offset = lines.offset(&text, text_position(position.position));
        let tags = tags::scan(&text, &config.default_syntax());
        let tag = tags::containing(&tags, offset).filter(|tag| matches!(tag.kind, tags::TagKind::Expr | tags::TagKind::Block) && tag.inner.start <= offset && offset <= tag.inner.end)?;
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let start = text.get(..offset)?.trim_end_matches(is_word).len().max(tag.inner.start);
        let end = offset + text.get(offset..tag.inner.end)?.find(|c: char| !is_word(c)).unwrap_or(tag.inner.end - offset);
        let before = &text[..start];
        if start == end || !before.ends_with('.') || before.ends_with("..") {
            return None;
        }
        // The chain before the name, which may hold calls and indexing (`order.items.first().name`).
        let chain_start = askama_analysis::text::chain_start(&text, start).max(tag.inner.start);
        if chain_start == start {
            return None;
        }
        let first = text[chain_start..start].split(|c: char| !is_word(c)).next()?;
        // The chain is lowered up to the name and through it; the first gives the name's place in the second.
        let scope = self.probe_scope(&Cursor { uri, config: &config, text: &text, offset }, first, &[chain_start..start, chain_start..end])?;
        let line = format!("{TYPE_PROBE_PREFIX}{});", scope.lowered[1]);
        let cursor = TYPE_PROBE_PREFIX.len() + scope.lowered[0].len();
        let probe = probe::at(&scope.rust_text, &scope.template_struct, &scope.setup, &line, cursor);
        Some(PathProbe { root: scope.root, file: scope.template_struct.file, probe, range: to_range(&text, &lines, start..end) })
    }

    /// Asks the crate's rust-analyzer about a probe and turns its answers into completion items for the template.
    async fn probe_members(&self, plan: ProbePlan) -> Option<Vec<CompletionItem>> {
        let analyzer = self.ready_analyzer(&plan.root).await?;
        let uri = Uri::from_file_path(&plan.file)?;
        let items = analyzer.completion(uri.as_str(), &plan.text, plan.position.line, plan.position.character).await;
        Some(items.iter().filter_map(|found| analyzer_item(found, plan.range)).collect())
    }

    /// rust-analyzer's hover for the name at a position that follows a `.`.
    async fn path_hover(&self, position: &TextDocumentPositionParams) -> Option<Hover> {
        let plan = self.path_probe(position)?;
        let analyzer = self.ready_analyzer(&plan.root).await?;
        let uri = Uri::from_file_path(&plan.file)?;
        let mut hover = analyzer.hover(uri.as_str(), &plan.probe.text, plan.probe.position.line, plan.probe.position.character).await?;
        let contents = serde_json::from_value::<HoverContents>(hover.get_mut("contents")?.take()).ok()?;
        Some(Hover { contents, range: Some(plan.range) })
    }

    /// Where the name at a position that follows a `.` is defined, by rust-analyzer. A place in the probed file is moved back to where it
    /// is on disk.
    async fn path_definition(&self, position: &TextDocumentPositionParams) -> Option<Location> {
        let plan = self.path_probe(position)?;
        let analyzer = self.ready_analyzer(&plan.root).await?;
        let uri = Uri::from_file_path(&plan.file)?;
        let found = analyzer.definition(uri.as_str(), &plan.probe.text, plan.probe.position.line, plan.probe.position.character).await;
        let mut location = serde_json::from_value::<Location>(found.into_iter().next()?).ok()?;
        if location.uri == uri {
            location.range.start.line = plan.probe.original_line(location.range.start.line)?;
            location.range.end.line = plan.probe.original_line(location.range.end.line)?;
        }
        Some(location)
    }

    /// Type checks a template a moment from now, in a task of its own, so the edit that asked for it is answered at once. A newer edit
    /// cancels it, so a burst of edits gets one type check, after it.
    fn schedule_type_check(&self, uri: Uri) {
        let backend = self.clone();
        let task = tokio::spawn({
            let uri = uri.clone();
            async move {
                tokio::time::sleep(TYPE_CHECK_DELAY).await;
                backend.type_check(uri).await;
            }
        });
        let previous = self.lock().type_checks.insert(uri, task.abort_handle());
        if let Some(previous) = previous {
            previous.abort();
        }
    }

    /// The type errors of a template, from rust-analyzer, published with the check's own diagnostics. Only once the crate's rust-analyzer
    /// is running and ready (opening a template never starts it), only where unknown variables are checked, and only when the template has
    /// no syntax error.
    async fn type_check(&self, uri: Uri) {
        let Some(plan) = self.type_probe(&uri) else {
            return;
        };
        let Some(file_uri) = Uri::from_file_path(&plan.file) else {
            return;
        };
        let Some(items) = plan.analyzer.diagnostics(file_uri.as_str(), &plan.text).await else {
            return;
        };
        let probe_lines = LineIndex::new(&plan.text);
        let errors: Vec<Diagnostic> = items
            .iter()
            .filter(|item| item["code"].as_str().is_some_and(|code| TYPE_ERROR_CODES.contains(&code)))
            .filter_map(|item| {
                let start = position_of(&item["range"]["start"])?;
                let end = position_of(&item["range"]["end"])?;
                let (_, mapped) = plan.blocks.iter().find(|(line, _)| *line == start.line && start.line == end.line)?;
                let line_start = probe_lines.offset(&plan.text, TextPosition { line: start.line, character: 0 });
                let code_start = line_start + probe::LINE_INDENT.len() + TYPE_PROBE_PREFIX.len();
                let from = probe_lines.offset(&plan.text, start).checked_sub(code_start)?;
                let to = probe_lines.offset(&plan.text, end).checked_sub(code_start)?;
                if to > mapped.code.len() {
                    return None;
                }
                let range = mapped.template_offset(from)..mapped.template_offset(to);
                Some(error(&plan.template_text, range, item["message"].as_str()?.to_owned()))
            })
            .collect();

        let publication = {
            let mut state = self.lock();
            let current = state.documents.get(&uri).is_some_and(|document| document.version == plan.version);
            let checked = state.checked.get(&uri).filter(|(version, _)| *version == plan.version).map(|(_, diagnostics)| diagnostics.clone());
            match (current, checked) {
                (true, Some(mut diagnostics)) => {
                    state.type_errors.insert(uri.clone(), errors.clone());
                    diagnostics.extend(errors);
                    Some(diagnostics)
                }
                _ => None,
            }
        };
        if let Some(diagnostics) = publication {
            self.client.publish_diagnostics(uri, diagnostics, Some(plan.version)).await;
        }
    }

    /// A probe with one block per expression of a template, or `None` when it should not be type checked (see `type_check`).
    fn type_probe(&self, uri: &Uri) -> Option<TypeProbe> {
        let file = uri.to_file_path()?.into_owned();
        let root = project::crate_root(&file)?;
        let (text, version, analyzer, config) = {
            let mut state = self.lock();
            if !state.type_completion {
                return None;
            }
            let document = state.documents.get(uri).filter(|document| !document.rust)?;
            let (text, version) = (Arc::clone(&document.text), document.version);
            let (_, checked) = state.checked.get(uri).filter(|(checked, _)| *checked == version)?;
            if checked.iter().any(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR)) {
                return None;
            }
            let analyzer = state.analyzers.get(&root).and_then(|cell| cell.get().cloned().flatten())?;
            (text, version, analyzer, state.config_for(uri, &mut Vec::new()))
        };
        if !analyzer.is_ready() {
            return None;
        }
        let facts = self.facts(uri);
        let cache = self.file_cache(&root);
        let read = |path: &Path| self.read_in(path, cache.as_deref());
        let project = new_project(&config, &read, cache.as_deref());
        let structs = project.structs_for(&file, &facts.structs);
        if structs.is_empty() || !project.sees_only_struct_names(&file, &structs) {
            return None;
        }
        let template_struct = structs[0];
        let tags = tags::scan(&text, &config.default_syntax());
        let regions = outline::regions(&text, &tags);
        let bindings = scopes::bindings(&text, &tags, &regions);
        let is_member = |name: &str| template_struct.member(name).is_some();
        let template = Template { source: &text, tags: &tags, regions: &regions, bindings: &bindings, is_member: &is_member };
        let macros: Vec<&ByteRange<usize>> = Region::walk(&regions).filter(|region| region.kind == RegionKind::Macro).map(|region| &region.range).collect();
        // A call block's caller parameters get their values inside the macro, so their types are not known here.
        let caller_bodies: Vec<&ByteRange<usize>> = bindings.iter().filter(|binding| binding.kind == BindingKind::CallerArgument).map(|binding| &binding.scope).collect();

        let mut blocks = Vec::new();
        let mut mapped_blocks = Vec::new();
        for tag in tags.iter().filter(|tag| matches!(tag.kind, tags::TagKind::Expr | tags::TagKind::Block)) {
            let Some(range) = scopes::expression_part(&text, tag) else {
                continue;
            };
            // `loop` is a Rust keyword, and its members are Askama's, not a type's.
            let mentions_loop = text[range.clone()].split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|word| word == "loop");
            let in_macro = macros.iter().chain(&caller_bodies).any(|macro_range| macro_range.contains(&range.start));
            if text[range.clone()].trim().is_empty() || in_macro || mentions_loop {
                continue;
            }
            let (Some(mapped), Some(setup)) = (template.mapped_expression(range.clone(), range.start), template.setup(range.start, 0)) else {
                continue;
            };
            blocks.push((setup, format!("{TYPE_PROBE_PREFIX}{});", mapped.code)));
            mapped_blocks.push(mapped);
        }
        if blocks.is_empty() {
            return None;
        }
        let rust_text = project::read_text(&template_struct.file)?;
        let (probe_text, lines) = probe::with_blocks(&rust_text, template_struct, &blocks);
        Some(TypeProbe {
            analyzer,
            file: Arc::clone(&template_struct.file),
            text: probe_text,
            version,
            template_text: text,
            blocks: lines.into_iter().zip(mapped_blocks).collect(),
        })
    }

    /// The crate's rust-analyzer once it has loaded the crate, waiting for that briefly: the editor asks again as the user types, and the
    /// progress shown says when it is ready.
    async fn ready_analyzer(&self, root: &Path) -> Option<Arc<Analyzer>> {
        let analyzer = self.analyzer(root).await?;
        analyzer.wait_ready(Duration::from_secs(3)).await.then_some(analyzer)
    }

    /// The crate's rust-analyzer, started on first use and stopped once idle for `ANALYZER_IDLE_LIMIT` (see `stop_when_idle`); the next use
    /// starts it again. `None` if it cannot start, which is reported once; a server restart tries again.
    async fn analyzer(&self, root: &Path) -> Option<Arc<Analyzer>> {
        let (cell, program, progress, permissions) = {
            let mut state = self.lock();
            let cell = Arc::clone(state.analyzers.entry(root.to_path_buf()).or_default());
            (cell, state.rust_analyzer.clone(), state.can_show_progress, state.permissions)
        };
        cell.get_or_init(|| async {
            match Analyzer::start(&program, root, permissions).await {
                Ok(analyzer) => {
                    if progress {
                        // In a task of its own: it waits on the client, which must not hold up the requests that wait for this analyzer.
                        tokio::spawn(show_loading(self.client.clone(), root.to_path_buf(), Arc::clone(&analyzer)));
                    }
                    tokio::spawn(stop_when_idle(self.clone(), root.to_path_buf(), Arc::downgrade(&analyzer)));
                    Some(analyzer)
                }
                Err(error) => {
                    let message = format!("Askama: rust-analyzer (`{program}`) could not start: {error}. Completion of members is off. Set `askama.rustAnalyzer.path` to a rust-analyzer executable.");
                    self.client.show_message(MessageType::WARNING, message).await;
                    None
                }
            }
        })
        .await
        .clone()
    }

    /// Runs `answer` on the structure of an open template, scanned with its crate's syntax. `None` for a document the server does not have.
    fn with_structure<T>(&self, uri: &Uri, answer: impl FnOnce(Structure) -> T) -> Option<T> {
        let (text, config) = self.template(uri)?;
        let tags = tags::scan(&text, &config.default_syntax());
        let regions = outline::regions(&text, &tags);
        let bindings = scopes::bindings(&text, &tags, &regions);
        let lines = LineIndex::new(&text);
        Some(answer(Structure { text, lines, tags, regions, bindings }))
    }

    /// Runs `answer` with the project an open template belongs to, the template's path and its text. `None` for a document the server does
    /// not have or that is not a file.
    fn with_project<T>(&self, uri: &Uri, answer: impl FnOnce(&Project<'_>, &Path, &Arc<str>) -> T) -> Option<T> {
        let file = uri.to_file_path()?.into_owned();
        let (text, config) = self.template(uri)?;
        let cache = project::crate_root(&file).and_then(|root| self.file_cache(&root));
        let read = |path: &Path| self.read_in(path, cache.as_deref());
        Some(answer(&new_project(&config, &read, cache.as_deref()), &file, &text))
    }

    /// The filter applied at a position, its range, and the crate's facts for its filters.
    fn filter_at(&self, position: &TextDocumentPositionParams) -> Option<(FilterUse, Range, Arc<CrateFacts>)> {
        let uri = &position.text_document.uri;
        let (text, config) = self.template(uri)?;
        let lines = LineIndex::new(&text);
        let offset = lines.offset(&text, text_position(position.position));
        let tags = tags::scan(&text, &config.default_syntax());
        let used = filters::filter_uses(&text, &tags).into_iter().find(|used| used.range.start <= offset && offset <= used.range.end)?;
        let range = to_range(&text, &lines, used.range.clone());
        Some((used, range, self.facts(uri)))
    }

    /// Where the custom filter applied at a position is defined.
    fn filter_definition(&self, position: &TextDocumentPositionParams) -> Option<Location> {
        let (used, _, facts) = self.filter_at(position)?;
        let custom = facts.filters.find(&used.name).filter(|_| !used.qualified)?;
        let text: Arc<str> = Arc::from(project::read_text(&custom.file)?);
        location(&navigation::Location { path: custom.file.clone(), text, range: custom.range.clone() })
    }

    /// The hover for a name the template binds, or a `loop` member, at a position.
    fn binding_hover(&self, position: &TextDocumentPositionParams) -> Option<Hover> {
        self.with_structure(&position.text_document.uri, |structure| {
            let offset = structure.lines.offset(&structure.text, text_position(position.position));
            if let Some((name, description, range)) = scopes::loop_member_at(&structure.text, &structure.bindings, offset) {
                let value = format!("**`loop.{name}`**\n\n{description}");
                return Some(markdown_hover(value, Some(to_range(&structure.text, &structure.lines, range))));
            }
            let binding = scopes::binding_at(&structure.text, &structure.tags, &structure.bindings, offset)?;
            Some(markdown_hover(format!("**`{}`**: {}", binding.name, binding_summary(binding)), None))
        })
        .flatten()
    }

    /// Where the name the template binds at a position is bound.
    fn binding_definition(&self, position: &TextDocumentPositionParams) -> Option<Location> {
        let uri = &position.text_document.uri;
        self.with_structure(uri, |structure| {
            let offset = structure.lines.offset(&structure.text, text_position(position.position));
            let binding = scopes::binding_at(&structure.text, &structure.tags, &structure.bindings, offset)?;
            Some(Location { uri: uri.clone(), range: to_range(&structure.text, &structure.lines, binding.range.clone()) })
        })
        .flatten()
    }

    /// Every place the name the template binds at a position is written, the binding first, with the document's URI and text.
    fn binding_occurrences(&self, position: &TextDocumentPositionParams) -> Option<(Uri, Arc<str>, Vec<ByteRange<usize>>)> {
        let uri = &position.text_document.uri;
        self.with_structure(uri, |structure| {
            let offset = structure.lines.offset(&structure.text, text_position(position.position));
            let binding = scopes::binding_at(&structure.text, &structure.tags, &structure.bindings, offset).filter(|binding| binding.kind != BindingKind::Loop)?;
            let ranges = scopes::occurrences(&structure.text, &structure.tags, &structure.bindings, binding);
            Some((uri.clone(), Arc::clone(&structure.text), ranges))
        })
        .flatten()
    }

    /// The struct members named by the variable at a position, each with its struct's name, and the variable's range.
    fn members_at(&self, position: &TextDocumentPositionParams) -> Option<(Vec<(String, Member)>, Range)> {
        let uri = &position.text_document.uri;
        let file = uri.to_file_path()?.into_owned();
        let (text, config) = self.template(uri)?;
        let lines = LineIndex::new(&text);
        let offset = lines.offset(&text, text_position(position.position));
        let variable = navigation::variable_at(&text, &tags::scan(&text, &config.default_syntax()), offset)?;
        let facts = self.facts(uri);
        let cache = project::crate_root(&file).and_then(|root| self.file_cache(&root));
        let read = |path: &Path| self.read_in(path, cache.as_deref());
        let found: Vec<(String, Member)> = new_project(&config, &read, cache.as_deref())
            .structs_for(&file, &facts.structs)
            .into_iter()
            .filter_map(|template_struct| Some((template_struct.name.clone(), template_struct.member(&variable.name)?.clone())))
            .collect();
        (!found.is_empty()).then(|| (found, to_range(&text, &lines, variable.range)))
    }

    /// Where the struct member named by the variable at a position is declared.
    fn member_definition(&self, position: &TextDocumentPositionParams) -> Option<Location> {
        let (found, _) = self.members_at(position)?;
        let (_, member) = found.into_iter().next()?;
        let text: Arc<str> = Arc::from(project::read_text(&member.file)?);
        location(&navigation::Location { path: member.file.to_path_buf(), text, range: member.range })
    }

    /// In a Rust file, the template named by the `#[template(path = "...")]` at a position.
    fn template_of_struct(&self, position: &TextDocumentPositionParams) -> Option<Location> {
        let uri = &position.text_document.uri;
        let file = uri.to_file_path()?.into_owned();
        let config = {
            let mut state = self.lock();
            if !state.documents.get(uri)?.rust {
                return None;
            }
            state.config_for(uri, &mut Vec::new())
        };
        let disk: Arc<str> = Arc::from(project::read_text(&file)?);
        let offset = LineIndex::new(&disk).offset(&disk, text_position(position.position));
        let facts = self.facts(uri);
        let template_struct = facts
            .structs
            .iter()
            .find(|found| navigation::same_file(&found.file, &file) && found.template_range.start <= offset && offset <= found.template_range.end)?;
        let target = config.find_template(&template_struct.template, None)?;
        let text = self.read(&target)?;
        location(&navigation::Location { path: target, text, range: 0..0 })
    }

    /// For the block named at a position: the block itself, and the blocks that override it in templates extending this one.
    fn block_overrides(&self, position: &TextDocumentPositionParams) -> Option<(Option<Location>, Vec<Location>)> {
        self.with_project(&position.text_document.uri, |project, file, text| {
            let offset = LineIndex::new(text).offset(text, text_position(position.position));
            let block = project.block_at(text, offset)?;
            let declaration = navigation::Location { path: file.to_path_buf(), text: Arc::clone(text), range: block.range };
            let overrides = project.overrides(file, &block.name).iter().filter_map(location).collect();
            Some((location(&declaration), overrides))
        })
        .flatten()
    }

    /// Parses the given open templates and publishes their diagnostics, along with those of any `askama.toml` read along the way.
    async fn check(&self, uris: Vec<Uri>) {
        let mut publications = Vec::new();
        for uri in uris {
            let (text, version, config) = {
                let mut state = self.lock();
                let Some(document) = state.documents.get(&uri).filter(|document| !document.rust) else {
                    continue;
                };
                let (text, version) = (Arc::clone(&document.text), document.version);
                (text, version, state.config_for(&uri, &mut publications))
            };
            let mut diagnostics = self.diagnostics(&uri, &text, &config);
            {
                let mut state = self.lock();
                // A newer edit has its own check, whose results these must not replace.
                if state.documents.get(&uri).is_some_and(|document| document.version == version) {
                    state.checked.insert(uri.clone(), (version, diagnostics.clone()));
                }
                // The type check that follows replaces these; until then the last ones stand, so they do not blink off and on while typing.
                if diagnostics.iter().all(|diagnostic| diagnostic.severity != Some(DiagnosticSeverity::ERROR)) {
                    diagnostics.extend(state.type_errors.get(&uri).cloned().unwrap_or_default());
                } else {
                    state.type_errors.remove(&uri);
                }
            }
            publications.push(Publication { uri, diagnostics, version: Some(version) });
        }
        self.watch_template_dirs().await;
        for publication in publications {
            // Checks run concurrently, one per change. One that finished after a newer edit would put its diagnostics where the text no
            // longer matches, so it gives way.
            let outdated = publication.version.is_some_and(|version| {
                self.lock().documents.get(&publication.uri).is_some_and(|document| document.version != version)
            });
            if outdated {
                continue;
            }
            self.client
                .publish_diagnostics(publication.uri, publication.diagnostics, publication.version)
                .await;
        }
    }

    /// A template's own diagnostics: its syntax error, or else its unknown filters and variables, and its missing templates.
    fn diagnostics(&self, uri: &Uri, text: &Arc<str>, config: &Config) -> Vec<Diagnostic> {
        let mut diagnostics = match template::parse(Arc::clone(text), &config.default_syntax()) {
            Ok(_) => Vec::new(),
            Err(problem) => vec![error(text, problem.range, problem.message)],
        };
        let Some(file) = uri.to_file_path() else {
            return diagnostics;
        };
        let cache = project::crate_root(&file).and_then(|root| self.file_cache(&root));
        let read = |path: &Path| self.read_in(path, cache.as_deref());
        let project = new_project(config, &read, cache.as_deref());
        // While a tag is unclosed, the rest of the file reads as part of it, so names found there are not names: the syntax error is the
        // only thing to report.
        if diagnostics.is_empty() {
            let facts = self.facts(uri);
            diagnostics.extend(unknown_filters(text, config, &facts.filters));
            diagnostics.extend(unknown_variables(&project, config, &facts, &file, text));
        }
        diagnostics.extend(missing_templates(&project, &file, text));
        diagnostics
    }
}

impl State {
    /// An open template's text. `None` for a Rust file or a document the server does not have.
    fn template_text(&self, uri: &Uri) -> Option<Arc<str>> {
        self.documents.get(uri).filter(|document| !document.rust).map(|document| Arc::clone(&document.text))
    }

    /// The editor's text of the file at `path`, if it is open.
    fn open_text(&self, path: &Path) -> Option<Arc<str>> {
        Uri::from_file_path(path).and_then(|uri| self.documents.get(&uri)).map(|document| Arc::clone(&document.text))
    }

    /// The configuration of the crate a document belongs to, or Askama's defaults for a file outside any crate.
    fn config_for(&mut self, uri: &Uri, publications: &mut Vec<Publication>) -> Arc<Config> {
        uri.to_file_path()
            .and_then(|path| project::crate_root(&path))
            .map_or_else(|| Arc::new(Config::default_for(Path::new(""))), |root| self.config(&root, publications))
    }

    /// The crate's configuration, read on first use. A broken `askama.toml` gets its own diagnostic, and its templates are checked with
    /// Askama's defaults meanwhile.
    fn config(&mut self, root: &Path, publications: &mut Vec<Publication>) -> Arc<Config> {
        if let Some(config) = self.configs.get(root) {
            return Arc::clone(config);
        }
        let path = root.join(CONFIG_FILE_NAME);
        let config = match project::read_text(&path) {
            Some(source) => {
                let (config, diagnostics) = match Config::parse(root, &source) {
                    Ok(config) => (config, Vec::new()),
                    Err(problem) => (Config::default_for(root), vec![config_error(&source, problem)]),
                };
                if let Some(uri) = Uri::from_file_path(&path) {
                    publications.push(Publication { uri, diagnostics, version: None });
                }
                config
            }
            None => Config::default_for(root),
        };
        let config = Arc::new(config);
        self.configs.insert(root.to_path_buf(), Arc::clone(&config));
        self.unwatched_roots.push(root.to_path_buf());
        config
    }
}

/// Stops the crate's rust-analyzer once nothing has asked it anything for `ANALYZER_IDLE_LIMIT`, freeing its memory (gigabytes, for a large
/// workspace). It is dropped from the server's state; the process ends when the last request still holding it finishes, since it is killed
/// when dropped. Type checks never start one, so they pause until a member completion or hover starts it again.
async fn stop_when_idle(backend: Backend, root: PathBuf, analyzer: std::sync::Weak<Analyzer>) {
    loop {
        tokio::time::sleep(ANALYZER_IDLE_CHECK).await;
        let Some(running) = analyzer.upgrade() else {
            return;
        };
        if running.idle_for() < ANALYZER_IDLE_LIMIT {
            continue;
        }
        let stopped = {
            let mut state = backend.lock();
            let current = state.analyzers.get(&root).and_then(|cell| cell.get().cloned().flatten());
            // Only this analyzer: a newer one for the same crate has its own task.
            let is_this = current.as_ref().is_some_and(|current| Arc::ptr_eq(current, &running));
            if is_this {
                state.analyzers.remove(&root);
            }
            is_this
        };
        drop(running);
        if stopped {
            let message = format!("Stopped the rust-analyzer for {} after {} idle minutes.", root.display(), ANALYZER_IDLE_LIMIT.as_secs() / 60);
            backend.client.log_message(MessageType::INFO, message).await;
        }
        return;
    }
}

/// Shows progress in the editor until rust-analyzer has loaded the crate at `root`.
async fn show_loading(client: Client, root: PathBuf, analyzer: Arc<Analyzer>) {
    let token = NumberOrString::String(format!("askama-rust-analyzer:{}", root.display()));
    if client.create_work_done_progress(token.clone()).await.is_err() {
        return;
    }
    let progress = client.progress(token, "Askama").with_message("rust-analyzer is loading the crate").begin().await;
    let ready = analyzer.wait_ready(Duration::from_secs(600)).await;
    progress.finish_with_message(if ready { "rust-analyzer is ready" } else { "rust-analyzer is still loading" }).await;
}

/// An error diagnostic on a range of a template.
fn error(text: &str, range: ByteRange<usize>, message: String) -> Diagnostic {
    Diagnostic {
        range: to_range(text, &LineIndex::new(text), range),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some(SOURCE.to_owned()),
        message,
        ..Diagnostic::default()
    }
}

/// A project for `config`'s crate, keeping what it learns of the files in `cache` when the crate has one (see `Backend::file_cache`).
fn new_project<'a>(config: &'a Config, read: &'a Reader<'a>, cache: Option<&'a Mutex<FileCache>>) -> Project<'a> {
    match cache {
        Some(cache) => Project::with_cache(config, read, cache),
        None => Project::new(config, read),
    }
}

/// A warning on each template path Askama would not find.
fn missing_templates(project: &Project<'_>, file: &Path, text: &str) -> Vec<Diagnostic> {
    project
        .resolved_paths(file, text)
        .into_iter()
        .filter(|(_, target)| target.is_none())
        .map(|(path, _)| {
            let message = format!("template \"{}\" not found beside this template or in the template directories", path.name.name);
            Diagnostic {
                severity: Some(DiagnosticSeverity::WARNING),
                ..error(text, path.name.range, message)
            }
        })
        .collect()
}

/// A warning on each filter that is neither built in nor in the crate's `filters` module, unless the module brings in names this reading
/// cannot follow.
fn unknown_filters(text: &str, config: &Config, crate_filters: &CrateFilters) -> Vec<Diagnostic> {
    if crate_filters.incomplete {
        return Vec::new();
    }
    let tags = tags::scan(text, &config.default_syntax());
    filters::filter_uses(text, &tags)
        .into_iter()
        .filter(|used| !used.qualified && filters::builtin(&used.name).is_none() && crate_filters.find(&used.name).is_none())
        .map(|used| {
            let message = format!("unknown filter `{}`: not built into Askama and not in the crate's `filters` module", used.name);
            Diagnostic { severity: Some(DiagnosticSeverity::WARNING), ..error(text, used.range, message) }
        })
        .collect()
}

/// A warning on each variable that is neither bound in the template nor a member of its structs. Only for a template whose structs are
/// known and that is not included elsewhere, where it could also see the names bound around the `include`.
fn unknown_variables(project: &Project<'_>, config: &Config, facts: &CrateFacts, file: &Path, text: &str) -> Vec<Diagnostic> {
    let structs = project.structs_for(file, &facts.structs);
    if structs.is_empty() || !project.sees_only_struct_names(file, &structs) {
        return Vec::new();
    }
    let tags = tags::scan(text, &config.default_syntax());
    let bindings = scopes::bindings(text, &tags, &outline::regions(text, &tags));
    let owners: Vec<&str> = structs.iter().map(|template_struct| template_struct.name.as_str()).collect();
    scopes::variable_uses(text, &tags, &bindings)
        .into_iter()
        .filter(|used| {
            let bound = scopes::visible_at(&bindings, used.range.start).iter().any(|binding| binding.name == used.name);
            let member = structs.iter().any(|template_struct| template_struct.member(&used.name).is_some());
            !bound && !member
        })
        .map(|used| {
            let message = format!("unknown variable `{}`: not bound in this template and not a field or method of {}", used.name, owners.join(", "));
            Diagnostic { severity: Some(DiagnosticSeverity::WARNING), ..error(text, used.range, message) }
        })
        .collect()
}

/// What a binding is, in a few words.
fn binding_summary(binding: &Binding) -> String {
    match binding.kind {
        BindingKind::LoopVariable => format!("loop variable over `{}`", binding.origin),
        BindingKind::Loop => format!("the state of the loop over `{}`: `index`, `index0`, `first`, `last`, `cycle(…)`", binding.origin),
        BindingKind::Let => format!("set by `let` to `{}`", binding.origin),
        BindingKind::MacroArgument => format!("argument of macro `{}`", binding.origin),
        BindingKind::CallerArgument => format!("passed by macro `{}` through `caller(…)`", binding.origin),
        BindingKind::MatchArm | BindingKind::IfLet => format!("bound by `{}`", binding.origin),
    }
}

/// The members of `loop`, inside a `for` loop.
fn loop_items(range: Range) -> Vec<CompletionItem> {
    LOOP_MEMBERS
        .iter()
        .map(|&(name, description)| CompletionItem {
            documentation: Some(markdown(description.to_owned())),
            ..item(name, if name == "cycle" { CompletionItemKind::METHOD } else { CompletionItemKind::PROPERTY }, range)
        })
        .collect()
}

/// The tag keywords, the one that closes the innermost open region (`closer`) first and preselected.
fn keyword_items(closer: Option<&str>, range: Range) -> Vec<CompletionItem> {
    KEYWORDS
        .iter()
        .map(|&keyword| {
            let first = closer == Some(keyword);
            CompletionItem {
                sort_text: Some(format!("{}{keyword}", u8::from(!first))),
                preselect: first.then_some(true),
                detail: first.then(|| "closes the innermost open tag".to_owned()),
                ..item(keyword, CompletionItemKind::KEYWORD, range)
            }
        })
        .collect()
}

/// A completion item from rust-analyzer's answer about a probe, made to fit the template: only fields and methods, without ones that would
/// need an import, a method inserted with its parentheses, and the replaced range the template's.
fn analyzer_item(found: &serde_json::Value, range: Range) -> Option<CompletionItem> {
    let kind = match found["kind"].as_u64()? {
        2 => CompletionItemKind::METHOD,
        5 | 10 => CompletionItemKind::FIELD,
        _ => return None,
    };
    if found["additionalTextEdits"].as_array().is_some_and(|edits| !edits.is_empty()) {
        return None;
    }
    // The label, not `filterText`, which can carry doc aliases run together (`count_onespopcountpopcnt`).
    let label = found["label"].as_str()?;
    let name = label.split('(').next().unwrap_or(label).trim();
    let new_text = if kind == CompletionItemKind::METHOD { format!("{name}()") } else { name.to_owned() };
    let detail = found["detail"].as_str().or_else(|| found["labelDetails"]["description"].as_str()).map(str::to_owned);
    let documentation = match &found["documentation"] {
        serde_json::Value::String(text) => Some(markdown(text.clone())),
        serde_json::Value::Object(content) => content.get("value").and_then(serde_json::Value::as_str).map(|text| markdown(text.to_owned())),
        _ => None,
    };
    Some(CompletionItem {
        label: name.to_owned(),
        kind: Some(kind),
        detail,
        documentation,
        sort_text: found["sortText"].as_str().map(str::to_owned),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text })),
        ..CompletionItem::default()
    })
}

/// A completion item that inserts its label over `range`.
fn item(label: &str, kind: CompletionItemKind, range: Range) -> CompletionItem {
    CompletionItem {
        label: label.to_owned(),
        kind: Some(kind),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text: label.to_owned() })),
        ..CompletionItem::default()
    }
}

/// A member's doc comment followed by the struct or structs it belongs to.
fn member_docs(member: &Member, owners: &[&str]) -> String {
    let kind = match member.kind {
        MemberKind::Field => "Field",
        MemberKind::Method => "Method",
    };
    let owners: Vec<String> = owners.iter().map(|owner| format!("`{owner}`")).collect();
    let owned = format!("{kind} of {}.", owners.join(", "));
    if member.docs.is_empty() { owned } else { format!("{}\n\n{owned}", member.docs) }
}

/// Markdown documentation for a completion item.
const fn markdown(value: String) -> Documentation {
    Documentation::MarkupContent(MarkupContent { kind: MarkupKind::Markdown, value })
}

/// A hover showing Markdown.
const fn markdown_hover(value: String, range: Option<Range>) -> Hover {
    Hover { contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value }), range }
}

/// The diagnostic for a broken `askama.toml`.
fn config_error(source: &str, problem: ConfigError) -> Diagnostic {
    error(source, problem.range.unwrap_or(0..0), problem.message)
}

/// The LSP range of a byte range of `text`.
fn to_range(text: &str, lines: &LineIndex, range: ByteRange<usize>) -> Range {
    Range { start: position(lines.position(text, range.start)), end: position(lines.position(text, range.end)) }
}

/// The document symbol of a region, with its children.
fn symbol(text: &str, lines: &LineIndex, region: &Region) -> DocumentSymbol {
    let kind = match region.kind {
        RegionKind::Block => SymbolKind::NAMESPACE,
        RegionKind::For => SymbolKind::ARRAY,
        RegionKind::If => SymbolKind::BOOLEAN,
        RegionKind::Match => SymbolKind::ENUM,
        RegionKind::Macro => SymbolKind::FUNCTION,
        RegionKind::Call => SymbolKind::EVENT,
        RegionKind::Filter => SymbolKind::OPERATOR,
        RegionKind::Let => SymbolKind::VARIABLE,
        RegionKind::Raw => SymbolKind::STRING,
        RegionKind::Extends | RegionKind::Include => SymbolKind::FILE,
        RegionKind::Import => SymbolKind::MODULE,
    };
    #[expect(deprecated, reason = "`deprecated` must be given to build the struct; `tags` replaces it")]
    let symbol = DocumentSymbol {
        name: region.label.clone(),
        detail: None,
        kind,
        tags: None,
        deprecated: None,
        range: to_range(text, lines, region.range.clone()),
        selection_range: to_range(text, lines, region.opener.clone()),
        children: Some(region.children.iter().map(|child| symbol(text, lines, child)).collect()),
    };
    symbol
}

/// An LSP position in JSON as the analysis's position.
fn position_of(value: &serde_json::Value) -> Option<TextPosition> {
    Some(TextPosition { line: u32::try_from(value["line"].as_u64()?).ok()?, character: u32::try_from(value["character"].as_u64()?).ok()? })
}

/// An LSP position as the analysis's position.
const fn text_position(position: Position) -> TextPosition {
    TextPosition { line: position.line, character: position.character }
}

/// An LSP location for a place in a template.
fn location(found: &navigation::Location) -> Option<Location> {
    let lines = LineIndex::new(&found.text);
    Some(Location { uri: Uri::from_file_path(&found.path)?, range: to_range(&found.text, &lines, found.range.clone()) })
}

/// Whether `query`'s characters appear in `name` in order, as editors match symbol names.
fn is_subsequence(query: &str, name: &str) -> bool {
    let mut name = name.chars();
    query.chars().all(|wanted| name.any(|found| found == wanted))
}

/// The analysis's position as an LSP position.
const fn position(position: TextPosition) -> Position {
    Position { line: position.line, character: position.character }
}

#[tokio::main]
async fn main() {
    let (service, socket) = LspService::new(|client| Backend {
        client,
        can_watch_files: Arc::new(AtomicBool::new(false)),
        can_watch_relative: Arc::new(AtomicBool::new(false)),
        state: Arc::new(Mutex::new(State::default())),
        rust: Arc::new(Mutex::new(RustIndex::default())),
    });
    // A panic is logged in the client's output. The report is a plain function, so it hands the sending to a task of its own.
    let client = service.inner().client.clone();
    let report: panics::Report = Arc::new(move |message| {
        let client = client.clone();
        tokio::spawn(async move { client.log_message(MessageType::ERROR, message).await });
    });
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket).serve(CatchPanics::new(service, report)).await;
}
