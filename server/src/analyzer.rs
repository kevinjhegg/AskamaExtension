//! A private rust-analyzer, for the types the server cannot read from syntax alone.
//!
//! The server starts one rust-analyzer per crate, the first time a template needs a type, and talks LSP to it over its stdin and stdout. It
//! shows it a Rust file with a probe added (`askama_analysis::probe`) as an unsaved change, so nothing is written to disk, and the user's
//! own rust-analyzer and editor never see it.
//!
//! Every message to rust-analyzer is framed whole and queued for one writer task, so a request that is cancelled part way (as editors
//! cancel stale completions all the time) can never leave half a message on its input. Queuing never waits, so the task that reads
//! rust-analyzer's output never blocks on its input either.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, watch};

/// How long a request may take before its answer is given up on.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// An answer from rust-analyzer: its result, or its error.
type Answer = Result<Value, Value>;

/// One rust-analyzer process, for one crate.
pub struct Analyzer {
    /// Whole messages, header and body, for the writer task to send in order.
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
    /// Who waits for the answer to each request sent, by request id.
    pending: Mutex<HashMap<i64, oneshot::Sender<Answer>>>,
    /// The id of the next request.
    next_id: AtomicI64,
    /// True while rust-analyzer reports itself quiescent: the crate is loaded and indexed, so its answers are complete. It goes back to
    /// false while it reloads the workspace.
    ready: watch::Receiver<bool>,
    /// The version of each document shown to it, by URI. Held while a document's change is queued, so versions reach rust-analyzer in
    /// order.
    versions: Mutex<HashMap<String, i32>>,
    /// Set when its output closes: it has exited, and nothing more will be answered.
    exited: AtomicBool,
    /// Held from showing a probe until its question is answered, since every probe of a crate replaces the same file in rust-analyzer's
    /// memory.
    probing: tokio::sync::Mutex<()>,
    /// When it was last asked something, so the server can stop it once it has been idle a while (see `idle_for`).
    last_used: Mutex<Instant>,
    /// Held so the process lives as long as this value; it is killed when this is dropped.
    _child: Child,
}

/// What the private rust-analyzer may run of the crate's own code, as the user's own rust-analyzer is set up.
#[derive(Debug, Clone, Copy)]
pub struct Permissions {
    /// Whether it runs build scripts (`rust-analyzer.cargo.buildScripts.enable`).
    pub build_scripts: bool,
    /// Whether it expands procedural macros (`rust-analyzer.procMacro.enable`), which runs them.
    pub proc_macros: bool,
}

impl Default for Permissions {
    /// rust-analyzer's own defaults: both on.
    fn default() -> Self {
        Self { build_scripts: true, proc_macros: true }
    }
}

impl Analyzer {
    /// Starts `program` for the crate at `root` and initializes it, running only what `permissions` allow of the crate's code.
    pub async fn start(program: &str, root: &Path, permissions: Permissions) -> std::io::Result<Arc<Self>> {
        let mut child = Command::new(program)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let missing = || std::io::Error::other("rust-analyzer's standard streams are not available");
        let stdin = child.stdin.take().ok_or_else(missing)?;
        let stdout = child.stdout.take().ok_or_else(missing)?;
        let (ready_sender, ready) = watch::channel(false);
        let (outgoing, queued) = mpsc::unbounded_channel();
        let analyzer = Arc::new(Self {
            outgoing,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            ready,
            versions: Mutex::new(HashMap::new()),
            exited: AtomicBool::new(false),
            probing: tokio::sync::Mutex::new(()),
            last_used: Mutex::new(Instant::now()),
            _child: child,
        });
        tokio::spawn(write_messages(stdin, queued));
        tokio::spawn(read_messages(Arc::downgrade(&analyzer), BufReader::new(stdout), ready_sender));

        let root_uri = file_uri(root);
        let name = root.file_name().map_or_else(|| "crate".to_owned(), |name| name.to_string_lossy().into_owned());
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": name }],
            "capabilities": {
                "textDocument": {
                    "completion": {
                        "completionItem": {
                            "snippetSupport": false,
                            "labelDetailsSupport": true,
                            "documentationFormat": ["markdown", "plaintext"]
                        }
                    },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "definition": { "linkSupport": false },
                    "diagnostic": { "dynamicRegistration": false }
                },
                "experimental": { "serverStatusNotification": true }
            },
            // The same keys as the `rust-analyzer.*` settings. It watches files itself, since no editor sends it changes; it never runs
            // `cargo check` on save; the cargo runs it does need (build scripts, proc macros) use their own target directory, so they never
            // wait on the user's builds for the lock; it runs build scripts and procedural macros only if the user's own rust-analyzer
            // does; and it offers only what a template can use.
            "initializationOptions": {
                "files": { "watcher": "server" },
                "checkOnSave": false,
                // A template's struct is never in a test, bench or example, so those targets are not loaded. Most of the memory is the
                // workspace's crates and their dependencies, which this does not change.
                "cargo": { "targetDir": true, "allTargets": false, "buildScripts": { "enable": permissions.build_scripts } },
                "procMacro": { "enable": permissions.proc_macros },
                // Unknown fields and methods are among the diagnostics rust-analyzer calls experimental, which are off by default.
                "diagnostics": { "experimental": { "enable": true } },
                "completion": { "autoimport": { "enable": false }, "postfix": { "enable": false }, "snippets": { "custom": {} } }
            }
        });
        if analyzer.request("initialize", params).await.is_none() {
            return Err(std::io::Error::other("rust-analyzer did not answer `initialize`"));
        }
        analyzer.notify("initialized", &json!({}));
        Ok(analyzer)
    }

    /// How long since it was last asked something.
    pub fn idle_for(&self) -> Duration {
        self.last_used.lock().unwrap_or_else(PoisonError::into_inner).elapsed()
    }

    /// Records that it is being asked something now.
    fn touch(&self) {
        *self.last_used.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    /// Whether the crate is loaded and indexed now.
    pub fn is_ready(&self) -> bool {
        *self.ready.borrow()
    }

    /// Waits up to `limit` for the crate to be loaded and indexed. Whether it is.
    pub async fn wait_ready(&self, limit: Duration) -> bool {
        let mut ready = self.ready.clone();
        matches!(tokio::time::timeout(limit, ready.wait_for(|ready| *ready)).await, Ok(Ok(_)))
    }

    /// Shows rust-analyzer `text` as the file at `uri`, then asks `method` about it with `params`. Nothing else is shown in between.
    pub async fn ask(&self, uri: &str, text: &str, method: &str, params: Value) -> Option<Value> {
        self.touch();
        let _probing = self.probing.lock().await;
        self.set_text(uri, text);
        let answer = self.request(method, params).await;
        self.touch();
        answer
    }

    /// The completion items at a position in `text`, shown as the file at `uri`.
    pub async fn completion(&self, uri: &str, text: &str, line: u32, character: u32) -> Vec<Value> {
        let params = json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } });
        match self.ask(uri, text, "textDocument/completion", params).await {
            Some(Value::Array(items)) => items,
            Some(Value::Object(mut list)) => match list.remove("items") {
                Some(Value::Array(items)) => items,
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// The hover at a position in `text`, shown as the file at `uri`.
    pub async fn hover(&self, uri: &str, text: &str, line: u32, character: u32) -> Option<Value> {
        let params = json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } });
        self.ask(uri, text, "textDocument/hover", params).await.filter(|hover| !hover.is_null())
    }

    /// Where what is at a position in `text`, shown as the file at `uri`, is defined.
    pub async fn definition(&self, uri: &str, text: &str, line: u32, character: u32) -> Vec<Value> {
        let params = json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } });
        match self.ask(uri, text, "textDocument/definition", params).await {
            Some(Value::Array(locations)) => locations,
            Some(location @ Value::Object(_)) => vec![location],
            _ => Vec::new(),
        }
    }

    /// rust-analyzer's own diagnostics for `text`, shown as the file at `uri`.
    pub async fn diagnostics(&self, uri: &str, text: &str) -> Option<Vec<Value>> {
        let params = json!({ "textDocument": { "uri": uri } });
        match self.ask(uri, text, "textDocument/diagnostic", params).await? {
            Value::Object(mut report) => match report.remove("items") {
                Some(Value::Array(items)) => Some(items),
                _ => None,
            },
            _ => None,
        }
    }

    /// Shows rust-analyzer `text` as the unsaved content of the file at `uri`: opens it the first time, changes it after that. The version
    /// is counted and the message queued in one step, so a version is never counted for a message that was not sent.
    fn set_text(&self, uri: &str, text: &str) {
        let mut versions = self.versions.lock().unwrap_or_else(PoisonError::into_inner);
        let version = versions.entry(uri.to_owned()).or_insert(0);
        *version += 1;
        if *version == 1 {
            let document = json!({ "uri": uri, "languageId": "rust", "version": *version, "text": text });
            self.notify("textDocument/didOpen", &json!({ "textDocument": document }));
        } else {
            let document = json!({ "uri": uri, "version": *version });
            self.notify("textDocument/didChange", &json!({ "textDocument": document, "contentChanges": [{ "text": text }] }));
        }
    }

    /// Sends a request and waits for its answer. `None` when it times out, when rust-analyzer answers with an error, or when it has exited.
    async fn request(&self, method: &str, params: Value) -> Option<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(id, sender);
        // The waiter is removed however this ends, also when the request is cancelled.
        let _waiting = Waiting { analyzer: self, id };
        // Checked after the waiter is in place, so an exit in between cannot be missed: the reader clears every waiter when it sets this.
        if self.exited.load(Ordering::Acquire) {
            return None;
        }
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        match tokio::time::timeout(REQUEST_TIMEOUT, receiver).await {
            Ok(Ok(Ok(result))) => Some(result),
            _ => None,
        }
    }

    /// Sends a notification.
    fn notify(&self, method: &str, params: &Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Queues a message, framed whole, for the writer task. A message queued after rust-analyzer has exited is dropped; its request then
    /// fails.
    fn send(&self, message: &Value) {
        let body = message.to_string();
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(body.as_bytes());
        let _ = self.outgoing.send(frame);
    }
}

/// Removes a request's waiter when the request ends, answered or not.
struct Waiting<'a> {
    /// Whose request it is.
    analyzer: &'a Analyzer,
    /// The request's id.
    id: i64,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.analyzer.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&self.id);
    }
}

/// Writes queued messages to rust-analyzer's input, each whole, until the queue closes (the analyzer is dropped) or a write fails
/// (rust-analyzer has exited).
async fn write_messages(mut stdin: ChildStdin, mut queued: mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Some(frame) = queued.recv().await {
        if stdin.write_all(&frame).await.is_err() || stdin.flush().await.is_err() {
            return;
        }
    }
}

/// Reads rust-analyzer's messages until it exits: answers to requests go to their waiters, its own requests get empty answers, and its
/// status says when it is ready.
async fn read_messages(analyzer: Weak<Analyzer>, mut stdout: BufReader<ChildStdout>, ready: watch::Sender<bool>) {
    while let Some(mut message) = read_message(&mut stdout).await {
        let Some(analyzer) = analyzer.upgrade() else {
            return;
        };
        match (message.get("id"), message.get("method").and_then(Value::as_str)) {
            (Some(id), Some(method)) => {
                // Requests to the client: configuration gets one `null` per item asked for (the initialization options stand), everything
                // else `null`.
                let result = if method == "workspace/configuration" {
                    let count = message["params"]["items"].as_array().map_or(0, Vec::len);
                    Value::Array(vec![Value::Null; count])
                } else {
                    Value::Null
                };
                analyzer.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            }
            (Some(id), None) => {
                let waiter = id.as_i64().and_then(|id| analyzer.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id));
                if let Some(waiter) = waiter {
                    let answer = match message.get_mut("error") {
                        Some(error) => Err(error.take()),
                        None => Ok(message.get_mut("result").map_or(Value::Null, Value::take)),
                    };
                    let _ = waiter.send(answer);
                }
            }
            (None, Some("experimental/serverStatus")) => {
                let quiescent = message["params"]["quiescent"] == Value::Bool(true);
                ready.send_replace(quiescent);
            }
            _ => {}
        }
    }
    // It has exited (or never really started, as rustup's proxy does without its component): fail everything waiting, and everything asked
    // from now on, at once.
    if let Some(analyzer) = analyzer.upgrade() {
        analyzer.exited.store(true, Ordering::Release);
        analyzer.pending.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}

/// Reads one LSP message: its headers, then a body of the length they give. `None` at the end of the output or on a malformed message.
async fn read_message(stdout: &mut BufReader<ChildStdout>) -> Option<Value> {
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if stdout.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let header = line.trim();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok();
        }
    }
    let mut body = vec![0; length?];
    stdout.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

/// The `file://` URI of a path.
fn file_uri(path: &Path) -> String {
    tower_lsp_server::ls_types::Uri::from_file_path(path).map_or_else(|| format!("file://{}", path.display()), |uri| uri.as_str().to_owned())
}
