//! A private rust-analyzer, for the types the server cannot read from syntax alone.
//!
//! The server starts one rust-analyzer per crate, the first time a template needs a type, and
//! talks LSP to it over its stdin and stdout. It shows it a Rust file with a probe added
//! (`askama_analysis::probe`) as an unsaved change, so nothing is written to disk, and the
//! user's own rust-analyzer and editor never see it.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, watch};

/// How long a request may take before its answer is given up on.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Analyzer {
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    next_id: AtomicI64,
    /// Becomes true when rust-analyzer reports itself quiescent: the crate is loaded and
    /// indexed, so its answers are complete.
    ready: watch::Receiver<bool>,
    /// The version of each document shown to it, by URI.
    versions: Mutex<HashMap<String, i32>>,
    /// Set when its output closes: it has exited, and nothing more will be answered.
    exited: AtomicBool,
    /// Held from showing a probe until its question is answered, since every probe of a
    /// crate replaces the same file in rust-analyzer's memory.
    probing: tokio::sync::Mutex<()>,
    /// Held so the process lives as long as this value; it is killed when this is dropped.
    _child: Child,
}

impl Analyzer {
    /// Starts `program` for the crate at `root` and initializes it.
    pub async fn start(program: &str, root: &Path) -> std::io::Result<Arc<Self>> {
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
        let analyzer = Arc::new(Self {
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            ready,
            versions: Mutex::new(HashMap::new()),
            exited: AtomicBool::new(false),
            probing: tokio::sync::Mutex::new(()),
            _child: child,
        });
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
            // The same keys as the `rust-analyzer.*` settings. It watches files itself, since
            // no editor sends it changes; it never runs `cargo check` on save; the cargo runs it
            // does need (build scripts, proc macros) use their own target directory, so they
            // never wait on the user's builds for the lock; and it offers only what a template
            // can use.
            "initializationOptions": {
                "files": { "watcher": "server" },
                "checkOnSave": false,
                "cargo": { "targetDir": true },
                // Unknown fields and methods are among the diagnostics rust-analyzer calls
                // experimental, which are off by default.
                "diagnostics": { "experimental": { "enable": true } },
                "completion": { "autoimport": { "enable": false }, "postfix": { "enable": false }, "snippets": { "custom": {} } }
            }
        });
        if analyzer.request("initialize", params).await.is_none() {
            return Err(std::io::Error::other("rust-analyzer did not answer `initialize`"));
        }
        analyzer.notify("initialized", json!({})).await;
        Ok(analyzer)
    }

    /// Waits up to `limit` for the crate to be loaded and indexed. Whether it is.
    pub async fn wait_ready(&self, limit: Duration) -> bool {
        let mut ready = self.ready.clone();
        matches!(tokio::time::timeout(limit, ready.wait_for(|ready| *ready)).await, Ok(Ok(_)))
    }

    /// Shows rust-analyzer `text` as the file at `uri`, then asks `method` about it with
    /// `params`. Nothing else is shown in between.
    pub async fn ask(&self, uri: &str, text: &str, method: &str, params: Value) -> Option<Value> {
        let _probing = self.probing.lock().await;
        self.set_text(uri, text).await;
        self.request(method, params).await
    }

    /// The completion items at a position in `text`, shown as the file at `uri`.
    pub async fn completion(&self, uri: &str, text: &str, line: u32, character: u32) -> Vec<Value> {
        let params = json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } });
        match self.ask(uri, text, "textDocument/completion", params).await {
            Some(Value::Array(items)) => items,
            Some(Value::Object(list)) => list.get("items").and_then(Value::as_array).cloned().unwrap_or_default(),
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
        let report = self.ask(uri, text, "textDocument/diagnostic", params).await?;
        report.get("items").and_then(Value::as_array).cloned()
    }

    /// Shows rust-analyzer `text` as the unsaved content of the file at `uri`.
    async fn set_text(&self, uri: &str, text: &str) {
        let version = {
            let mut versions = self.versions.lock().unwrap_or_else(PoisonError::into_inner);
            let version = versions.entry(uri.to_owned()).or_insert(0);
            *version += 1;
            *version
        };
        if version == 1 {
            let document = json!({ "uri": uri, "languageId": "rust", "version": version, "text": text });
            self.notify("textDocument/didOpen", json!({ "textDocument": document })).await;
        } else {
            let document = json!({ "uri": uri, "version": version });
            self.notify("textDocument/didChange", json!({ "textDocument": document, "contentChanges": [{ "text": text }] })).await;
        }
    }

    async fn request(&self, method: &str, params: Value) -> Option<Value> {
        if self.exited.load(Ordering::Relaxed) {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(id, sender);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await;
        match tokio::time::timeout(REQUEST_TIMEOUT, receiver).await {
            Ok(Ok(result)) => Some(result),
            _ => {
                self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                None
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params })).await;
    }

    async fn send(&self, message: Value) {
        let body = message.to_string();
        let mut stdin = self.stdin.lock().await;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        // A failed write means rust-analyzer has exited; its requests then time out.
        let _ = stdin.write_all(header.as_bytes()).await;
        let _ = stdin.write_all(body.as_bytes()).await;
        let _ = stdin.flush().await;
    }
}

/// Reads rust-analyzer's messages until it exits: answers to requests go to their waiters,
/// its own requests get empty answers, and its status says when it is ready.
async fn read_messages(analyzer: Weak<Analyzer>, mut stdout: BufReader<ChildStdout>, ready: watch::Sender<bool>) {
    while let Some(message) = read_message(&mut stdout).await {
        let Some(analyzer) = analyzer.upgrade() else {
            return;
        };
        match (message.get("id"), message.get("method").and_then(Value::as_str)) {
            (Some(id), Some(method)) => {
                // Requests to the client: configuration gets one `null` per item asked for
                // (the initialization options stand), everything else `null`.
                let result = if method == "workspace/configuration" {
                    let count = message["params"]["items"].as_array().map_or(0, Vec::len);
                    Value::Array(vec![Value::Null; count])
                } else {
                    Value::Null
                };
                analyzer.send(json!({ "jsonrpc": "2.0", "id": id, "result": result })).await;
            }
            (Some(id), None) => {
                let waiter = id.as_i64().and_then(|id| analyzer.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id));
                if let Some(waiter) = waiter {
                    let _ = waiter.send(message.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            (None, Some("experimental/serverStatus")) if message["params"]["quiescent"] == Value::Bool(true) => {
                let _ = ready.send(true);
            }
            _ => {}
        }
    }
    // It has exited (or never really started, as rustup's proxy does without its component):
    // fail everything waiting, and everything asked from now on, at once.
    if let Some(analyzer) = analyzer.upgrade() {
        analyzer.exited.store(true, Ordering::Relaxed);
        analyzer.pending.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}

async fn read_message(stdout: &mut BufReader<ChildStdout>) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if stdout.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok();
        }
    }
    let mut body = vec![0; length?];
    stdout.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

fn file_uri(path: &Path) -> String {
    tower_lsp_server::ls_types::Uri::from_file_path(path).map_or_else(|| format!("file://{}", path.display()), |uri| uri.as_str().to_owned())
}
