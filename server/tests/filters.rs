//! End-to-end tests of filters and tag keywords: completion, hover, going to a custom
//! filter's Rust function, and the unknown-filter warning.

mod common;

use common::{Crate, Session};
use serde_json::{Value, json};

const MAIN: &str = "\
mod filters {
    /// Formats cents as dollars.
    #[askama::filter_fn]
    pub fn dollars(cents: &u64, _: &dyn askama::Values) -> askama::Result<String> {
        Ok(format!(\"${}.{:02}\", cents / 100, cents % 100))
    }
}
";

const PAGE: &str = "<p>{{ total|dollars }} {{ name|upper }} {{ name|nope }}</p>\n";

fn site() -> (Crate, Session, String) {
    let krate = Crate::new();
    krate.write("src/main.rs", MAIN);
    let uri = krate.uri("templates/page.html");
    let mut server = Session::start();
    server.open(&uri, PAGE);
    (krate, server, uri)
}

fn at(uri: &str, line: u32, character: u32) -> Value {
    json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } })
}

fn labels(items: &Value) -> Vec<(String, String)> {
    let mut items: Vec<(String, String)> = items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| (item["sortText"].as_str().unwrap().to_owned(), item["label"].as_str().unwrap().to_owned()))
        .collect();
    items.sort();
    items
}

#[test]
fn warns_about_an_unknown_filter_only() {
    let (_krate, mut server, uri) = site();
    let diagnostics = server.diagnostics_for(&uri);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0]["severity"], 2);
    assert_eq!(diagnostics[0]["range"]["start"], json!({ "line": 0, "character": 48 }));
    assert!(diagnostics[0]["message"].as_str().unwrap().starts_with("unknown filter `nope`"));
}

#[test]
fn completes_custom_filters_before_builtins() {
    let (_krate, mut server, uri) = site();
    server.change(&uri, 2, "{{ total| }}");
    let items = server.request("textDocument/completion", at(&uri, 0, 9));
    let labels = labels(&items);
    assert_eq!(labels[0], ("0dollars".to_owned(), "dollars".to_owned()));
    assert!(labels.iter().any(|(_, label)| label == "truncate"));
    let dollars = items.as_array().unwrap().iter().find(|item| item["label"] == "dollars").unwrap();
    assert_eq!(dollars["detail"], "fn dollars(cents: &u64, _: &dyn askama::Values) -> askama::Result<String>");
    assert_eq!(dollars["textEdit"]["range"], json!({ "start": { "line": 0, "character": 9 }, "end": { "line": 0, "character": 9 } }));
}

#[test]
fn completes_the_closing_tag_first() {
    let (_krate, mut server, uri) = site();
    server.change(&uri, 2, "{% for x in xs %}\n{% if x %}{% endif %}\n{% end");
    let items = server.request("textDocument/completion", at(&uri, 2, 6));
    let labels = labels(&items);
    assert_eq!(labels[0], ("0endfor".to_owned(), "endfor".to_owned()));
    let endfor = items.as_array().unwrap().iter().find(|item| item["label"] == "endfor").unwrap();
    assert_eq!(endfor["preselect"], true);
    assert_eq!(endfor["textEdit"]["range"]["start"], json!({ "line": 2, "character": 3 }));
}

#[test]
fn describes_filters_on_hover() {
    let (_krate, mut server, uri) = site();
    let custom = server.request("textDocument/hover", at(&uri, 0, 15));
    let text = custom["contents"]["value"].as_str().unwrap();
    assert!(text.starts_with("```rust\nfn dollars(cents: &u64"), "{text}");
    assert!(text.ends_with("Formats cents as dollars."), "{text}");

    let builtin = server.request("textDocument/hover", at(&uri, 0, 33));
    assert!(builtin["contents"]["value"].as_str().unwrap().starts_with("`upper` (Askama built-in filter)"));

    assert_eq!(server.request("textDocument/hover", at(&uri, 0, 1)), Value::Null);
}

#[test]
fn goes_to_a_custom_filter() {
    let (krate, mut server, uri) = site();
    let found = server.request("textDocument/definition", at(&uri, 0, 15));
    assert_eq!(
        found,
        json!({
            "uri": krate.uri("src/main.rs"),
            "range": { "start": { "line": 3, "character": 11 }, "end": { "line": 3, "character": 18 } }
        })
    );
}
