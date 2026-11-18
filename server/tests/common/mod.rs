//! The client side of an LSP session with the `askama-lsp` binary, and temporary crates to point it at, shared by the end-to-end tests.

#![allow(dead_code, reason = "each test file uses its own subset of these helpers")]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use serde_json::{Value, json};

/// A running server and the client side of its connection.
pub struct Session {
    child: Child,
    stdin: ChildStdin,
    /// The server's messages, read on a thread so a wait can time out.
    messages: Receiver<Value>,
    next_id: i64,
}

/// How long a test waits for any one message before it fails rather than hangs.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(60);

impl Session {
    pub fn start() -> Self {
        Self::start_with(json!(null))
    }

    /// Starts a server with the client's `initializationOptions`.
    pub fn start_with(initialization_options: Value) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_askama-lsp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("askama-lsp starts");
        let stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (sender, messages) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Some(message) = read_message(&mut stdout) {
                if sender.send(message).is_err() {
                    return;
                }
            }
        });
        let mut session = Self { child, stdin, messages, next_id: 1 };
        let mut params = json!({ "capabilities": {} });
        params["initializationOptions"] = initialization_options;
        session.request("initialize", params);
        session.notify("initialized", json!({}));
        session
    }

    pub fn send(&mut self, message: &Value) {
        let body = message.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        self.stdin.flush().unwrap();
    }

    pub fn receive(&self) -> Value {
        match self.messages.recv_timeout(MESSAGE_TIMEOUT) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => panic!("no message from the server within {MESSAGE_TIMEOUT:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("the server closed its output"),
        }
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        let mut message = json!({ "jsonrpc": "2.0", "method": method });
        message["params"] = params;
        self.send(&message);
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({ "jsonrpc": "2.0", "id": id, "method": method });
        message["params"] = params;
        self.send(&message);
        loop {
            let mut message = self.receive();
            // The server numbers its own requests to the client too, so only a message without a method is the answer.
            if message["id"] == id && message.get("method").is_none() {
                return message["result"].take();
            }
        }
    }

    /// The next diagnostics published for `uri`, skipping other messages.
    pub fn diagnostics_for(&self, uri: &str) -> Vec<Value> {
        loop {
            let message = self.receive();
            if message["method"] == "textDocument/publishDiagnostics" && message["params"]["uri"] == uri {
                return message["params"]["diagnostics"].as_array().unwrap().clone();
            }
        }
    }

    pub fn open(&mut self, uri: &str, text: &str) {
        self.open_as(uri, "html", text);
    }

    pub fn open_as(&mut self, uri: &str, language: &str, text: &str) {
        let document = json!({ "uri": uri, "languageId": language, "version": 1, "text": text });
        self.notify("textDocument/didOpen", json!({ "textDocument": document }));
    }

    pub fn change(&mut self, uri: &str, version: i32, text: &str) {
        let document = json!({ "uri": uri, "version": version });
        let changes = json!([{ "text": text }]);
        self.notify("textDocument/didChange", json!({ "textDocument": document, "contentChanges": changes }));
    }
}

fn read_message(stdout: &mut BufReader<ChildStdout>) -> Option<Value> {
    let mut length = 0;
    loop {
        let mut line = String::new();
        if stdout.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse().ok()?;
        }
    }
    let mut body = vec![0; length];
    stdout.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A crate in a temporary directory, removed when dropped.
pub struct Crate(pub PathBuf);

impl Crate {
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!("askama-lsp-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let root = std::env::temp_dir().join(name);
        std::fs::create_dir_all(root.join("templates")).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
        Self(root)
    }

    pub fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    pub fn uri(&self, relative: &str) -> String {
        uri(&self.0.join(relative))
    }
}

impl Drop for Crate {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

