//! End-to-end test of member completion through a private rust-analyzer. It needs a rust-analyzer: `ASKAMA_TEST_RUST_ANALYZER`, else the
//! one VS Code's rust-analyzer extension ships, else `rust-analyzer` on the PATH. Without one it is skipped, saying so.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{Crate, Session};
use serde_json::{Value, json};

const MAIN: &str = "\
pub struct Customer {
    /// The customer's name.
    pub name: String,
    pub email: String,
}

pub struct Order {
    pub id: u64,
    pub customer: Customer,
}

#[derive(Template)]
#[template(path = \"page.html\")]
pub struct Page {
    pub order: Order,
}

fn main() {}
";

const PAGE: &str = "{{ order. }}\n{{ order.customer.na }}\n";

fn rust_analyzer() -> Option<String> {
    if let Ok(path) = std::env::var("ASKAMA_TEST_RUST_ANALYZER") {
        return Some(path);
    }
    let home = std::env::var("HOME").ok()?;
    let mut bundled: Vec<PathBuf> = std::fs::read_dir(format!("{home}/.vscode/extensions"))
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("rust-lang.rust-analyzer-")))
        .map(|path| path.join("server/rust-analyzer"))
        .filter(|path| path.is_file())
        .collect();
    bundled.sort();
    if let Some(newest) = bundled.pop() {
        return Some(newest.display().to_string());
    }
    let on_path = std::process::Command::new("rust-analyzer").arg("--version").output();
    on_path.is_ok_and(|output| output.status.success()).then(|| "rust-analyzer".to_owned())
}

fn completion(server: &mut Session, uri: &str, line: u32, character: u32) -> Value {
    server.request("textDocument/completion", json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } }))
}

fn labels(items: &Value) -> Vec<String> {
    let mut labels: Vec<String> = items.as_array().map_or_else(Vec::new, |items| items.iter().map(|item| item["label"].as_str().unwrap().to_owned()).collect());
    labels.sort();
    labels
}

#[test]
fn completes_members_through_rust_analyzer() {
    let Some(program) = rust_analyzer() else {
        eprintln!("skipped: no rust-analyzer found (set ASKAMA_TEST_RUST_ANALYZER)");
        return;
    };
    let krate = Crate::new();
    krate.write("src/main.rs", MAIN);
    krate.write("templates/page.html", PAGE);
    let uri = krate.uri("templates/page.html");
    let mut server = Session::start_with(json!({ "rustAnalyzerPath": program }));
    server.open(&uri, PAGE);

    // The first answers come back empty while rust-analyzer loads the crate.
    let deadline = Instant::now() + Duration::from_secs(180);
    let order = loop {
        let items = completion(&mut server, &uri, 0, 9);
        if !labels(&items).is_empty() {
            break items;
        }
        assert!(Instant::now() < deadline, "rust-analyzer never answered");
        std::thread::sleep(Duration::from_secs(1));
    };
    // Methods every type has through blanket impls (such as `into`) may come too.
    let found = labels(&order);
    assert!(found.contains(&"customer".to_owned()) && found.contains(&"id".to_owned()), "{found:?}");

    let customer = completion(&mut server, &uri, 1, 20);
    let found = labels(&customer);
    assert!(found.contains(&"name".to_owned()) && found.contains(&"email".to_owned()), "{found:?}");
    let name = customer.as_array().unwrap().iter().find(|item| item["label"] == "name").unwrap();
    assert_eq!(name["detail"], "String");
    // The replaced range is the template's: the typed `na`.
    assert_eq!(name["textEdit"]["range"], json!({ "start": { "line": 1, "character": 18 }, "end": { "line": 1, "character": 20 } }));

    // Nothing was written to disk.
    assert_eq!(std::fs::read_to_string(krate.0.join("src/main.rs")).unwrap(), MAIN);
}

const SCOPED_MAIN: &str = "\
pub struct Item {
    pub sku: String,
    pub quantity: u32,
}

impl Item {
    pub fn total(&self) -> u64 { 0 }
}

pub struct Customer {
    pub name: String,
}

pub struct Order {
    pub customer: Customer,
    pub items: Vec<Item>,
}

#[derive(Template)]
#[template(path = \"page.html\")]
pub struct Page {
    pub order: Order,
}

fn main() {}
";

const SCOPED_PAGE: &str = "\
{% import \"macros.html\" as m %}
{% for item in order.items %}{{ item. }}{% call m::cell(item.quantity) %}{% endcall %}{% endfor %}
{% let c = order.customer %}{{ c. }}
";

const MACROS: &str = "{% macro cell(n) %}{{ n. }}{% endmacro %}\n";

/// Chains with a method call and an index before the `.`.
const CHAINED_PAGE: &str = "{{ order.items.first().unwrap(). }}\n{{ order.items[0]. }}\n";

#[test]
fn completes_members_after_calls_and_indexing() {
    let Some(program) = rust_analyzer() else {
        eprintln!("skipped: no rust-analyzer found (set ASKAMA_TEST_RUST_ANALYZER)");
        return;
    };
    let krate = Crate::new();
    krate.write("src/main.rs", SCOPED_MAIN);
    krate.write("templates/page.html", CHAINED_PAGE);
    let page = krate.uri("templates/page.html");
    let mut server = Session::start_with(json!({ "rustAnalyzerPath": program }));
    server.open(&page, CHAINED_PAGE);

    let deadline = Instant::now() + Duration::from_secs(180);
    let first = loop {
        let items = labels(&completion(&mut server, &page, 0, 32));
        if !items.is_empty() {
            break items;
        }
        assert!(Instant::now() < deadline, "rust-analyzer never answered");
        std::thread::sleep(Duration::from_secs(1));
    };
    assert!(first.contains(&"sku".to_owned()) && first.contains(&"total".to_owned()), "after first().unwrap(): {first:?}");
    let indexed = labels(&completion(&mut server, &page, 1, 18));
    assert!(indexed.contains(&"quantity".to_owned()), "after [0]: {indexed:?}");
}

#[test]
fn completes_members_of_loop_variables_lets_and_macro_arguments() {
    let Some(program) = rust_analyzer() else {
        eprintln!("skipped: no rust-analyzer found (set ASKAMA_TEST_RUST_ANALYZER)");
        return;
    };
    let krate = Crate::new();
    krate.write("src/main.rs", SCOPED_MAIN);
    krate.write("templates/page.html", SCOPED_PAGE);
    krate.write("templates/macros.html", MACROS);
    let page = krate.uri("templates/page.html");
    let macros = krate.uri("templates/macros.html");
    let mut server = Session::start_with(json!({ "rustAnalyzerPath": program }));
    server.open(&page, SCOPED_PAGE);
    server.open(&macros, MACROS);

    let deadline = Instant::now() + Duration::from_secs(180);
    let item = loop {
        let items = labels(&completion(&mut server, &page, 1, 37));
        if !items.is_empty() {
            break items;
        }
        assert!(Instant::now() < deadline, "rust-analyzer never answered");
        std::thread::sleep(Duration::from_secs(1));
    };
    for expected in ["quantity", "sku", "total"] {
        assert!(item.contains(&expected.to_owned()), "loop variable: {item:?}");
    }

    let customer = labels(&completion(&mut server, &page, 2, 33));
    assert!(customer.contains(&"name".to_owned()), "let: {customer:?}");

    // `n` is `&u32`, from the one call `m::cell(item.quantity)` inside the loop.
    let number = labels(&completion(&mut server, &macros, 0, 24));
    assert!(number.contains(&"count_ones".to_owned()), "macro argument: {number:?}");
    // Names are labels, not filter texts with doc aliases appended.
    assert!(!number.iter().any(|name| name.starts_with("count_ones") && name != "count_ones"), "{number:?}");
}

#[test]
fn hovers_finds_and_checks_deep_paths() {
    const DEEP: &str = "{{ order.customer.name }}\n{{ order.customer.mail }}\n";
    let Some(program) = rust_analyzer() else {
        eprintln!("skipped: no rust-analyzer found (set ASKAMA_TEST_RUST_ANALYZER)");
        return;
    };
    let krate = Crate::new();
    krate.write("src/main.rs", MAIN);
    krate.write("templates/page.html", DEEP);
    let uri = krate.uri("templates/page.html");
    let mut server = Session::start_with(json!({ "rustAnalyzerPath": program }));
    server.open(&uri, DEEP);

    // Type errors need a running rust-analyzer; a completion starts it.
    let deadline = Instant::now() + Duration::from_secs(180);
    while labels(&completion(&mut server, &uri, 0, 9)).is_empty() {
        assert!(Instant::now() < deadline, "rust-analyzer never answered");
        std::thread::sleep(Duration::from_secs(1));
    }

    let hover = server.request("textDocument/hover", json!({ "textDocument": { "uri": uri }, "position": { "line": 0, "character": 11 } }));
    let text = hover["contents"].to_string();
    assert!(text.contains("customer: Customer"), "{hover}");
    assert_eq!(hover["range"], json!({ "start": { "line": 0, "character": 9 }, "end": { "line": 0, "character": 17 } }));

    let found = server.request("textDocument/definition", json!({ "textDocument": { "uri": uri }, "position": { "line": 0, "character": 11 } }));
    assert_eq!(found["uri"], krate.uri("src/main.rs"));
    assert_eq!(found["range"]["start"], json!({ "line": 8, "character": 8 }));

    // An edit runs the type check, which publishes the unknown field `mail`.
    server.change(&uri, 2, DEEP);
    let deadline = Instant::now() + Duration::from_secs(30);
    let error = loop {
        let diagnostics = server.diagnostics_for(&uri);
        if let Some(error) = diagnostics.iter().find(|diagnostic| diagnostic["message"].as_str().unwrap_or("").contains("mail")) {
            break error.clone();
        }
        assert!(Instant::now() < deadline, "no type error published");
    };
    assert_eq!(error["severity"], 1);
    assert_eq!(error["range"], json!({ "start": { "line": 1, "character": 18 }, "end": { "line": 1, "character": 22 } }));
}
