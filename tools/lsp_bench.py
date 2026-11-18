#!/usr/bin/env python3
"""Drives askama-lsp over stdio against a real crate and times each kind of request, for docs/design.md's "Performance".

The crate should be a real one, with many templates and structs: the costs that matter grow with them. By default every template is opened;
with --one-open only one page template is, as in normal use, so the others are read from disk. The page is the first template that a struct
names, that extends another and that has a dotted expression (`{{ a.b }}`). With --rust-analyzer, member completion, hover and type checks
through the private rust-analyzer are timed too, and its memory is measured.

Usage: lsp_bench.py SERVER CRATE_DIR OUT.json [--one-open] [--rust-analyzer PATH] [--repeat N]

Build the server with `cargo build --release -p askama-lsp` and pass target/release/askama-lsp. The results are printed and written to
OUT.json; times are in milliseconds.
"""

import json
import os
import re
import statistics
import subprocess
import sys
import threading
import time
import urllib.parse
from pathlib import Path

args = sys.argv[1:]
server, crate, out = args[0], Path(args[1]).resolve(), args[2]
ra = args[args.index("--rust-analyzer") + 1] if "--rust-analyzer" in args else None
repeat = int(args[args.index("--repeat") + 1]) if "--repeat" in args else 20
one_open = "--one-open" in args


def uri(path):
    return "file://" + urllib.parse.quote(str(path))


class Client:
    def __init__(self):
        self.proc = subprocess.Popen([server], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.next_id = 1
        self.lock = threading.Condition()
        self.responses = {}
        self.diagnostics = {}  # uri -> list of (time, version, count)
        self.log = []
        threading.Thread(target=self.reader, daemon=True).start()

    def send(self, message):
        body = json.dumps(message).encode()
        self.proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        self.proc.stdin.flush()

    def reader(self):
        stream = self.proc.stdout
        while True:
            length = None
            while True:
                line = stream.readline()
                if not line:
                    return
                line = line.strip()
                if not line:
                    break
                if line.lower().startswith(b"content-length:"):
                    length = int(line.split(b":")[1])
            message = json.loads(stream.read(length))
            now = time.perf_counter()
            if "id" in message and "method" in message:
                self.send({"jsonrpc": "2.0", "id": message["id"], "result": None})
            elif "id" in message:
                with self.lock:
                    self.responses[message["id"]] = (now, message)
                    self.lock.notify_all()
            elif message.get("method") == "textDocument/publishDiagnostics":
                params = message["params"]
                with self.lock:
                    self.diagnostics.setdefault(params["uri"], []).append((now, params.get("version"), len(params["diagnostics"])))
                    self.lock.notify_all()
            elif message.get("method") == "window/logMessage":
                self.log.append(message["params"]["message"])

    def request(self, method, params, timeout=120):
        id = self.next_id
        self.next_id += 1
        start = time.perf_counter()
        self.send({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        with self.lock:
            if not self.lock.wait_for(lambda: id in self.responses, timeout):
                raise TimeoutError(method)
            end, message = self.responses.pop(id)
        return end - start, message.get("result")

    def notify(self, method, params):
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def wait_diagnostics(self, file_uri, version, since, timeout=60):
        """Time from `since` to the first publication for `version` of `file_uri`."""
        with self.lock:
            ok = self.lock.wait_for(lambda: any(v == version and t >= since for t, v, _ in self.diagnostics.get(file_uri, [])), timeout)
            if not ok:
                return None
            return min(t for t, v, _ in self.diagnostics[file_uri] if v == version and t >= since) - since

    def rss_mb(self, pid=None):
        pid = pid or self.proc.pid
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
        return int(out) / 1024 if out else None


def position(text, offset):
    before = text[:offset]
    line = before.count("\n")
    column = before[before.rfind("\n") + 1:]
    return {"line": line, "character": len(column.encode("utf-16-le")) // 2}


def stats(samples):
    samples = sorted(s * 1000 for s in samples if s is not None)
    if not samples:
        return None
    p95 = samples[min(len(samples) - 1, int(len(samples) * 0.95))]
    return {"n": len(samples), "median_ms": round(statistics.median(samples), 2), "p95_ms": round(p95, 2), "max_ms": round(samples[-1], 2)}


results = {"crate": str(crate)}
templates = sorted(p for p in (crate / "templates").rglob("*") if p.is_file())
struct_templates = set()
for rs in (crate / "src").rglob("*.rs"):
    struct_templates.update(re.findall(r'#\[template\(path\s*=\s*"([^"]+)"', rs.read_text()))
results["templates"] = len(templates)

c = Client()
start = time.perf_counter()
c.request("initialize", {
    "processId": os.getpid(),
    "rootUri": uri(crate),
    "workspaceFolders": [{"uri": uri(crate), "name": crate.name}],
    # Watchers are accepted, as VS Code accepts them, so the server keeps its caches; no file changes during the run.
    "capabilities": {"window": {"workDoneProgress": True}, "workspace": {"didChangeWatchedFiles": {"dynamicRegistration": True, "relativePatternSupport": True}}},
    "initializationOptions": {"typeCompletion": ra is not None, "rustAnalyzerPath": ra or ""},
})
c.notify("initialized", {})
results["initialize_ms"] = round((time.perf_counter() - start) * 1000, 2)
results["rss_idle_mb"] = c.rss_mb()

texts = {path: path.read_text() for path in templates}
# A page template with a struct, an expression and a dotted path, for the per-request timings.
page = None
for path in templates:
    name = path.relative_to(crate / "templates").as_posix()
    if name in struct_templates and re.search(r"\{\{\s*[a-z_]+\.[a-z_]+", texts[path]) and "{% extends" in texts[path]:
        page = path
        break
if page is None:
    sys.exit("no template is named by a struct, extends another and has a dotted expression")

# Opening the templates, one after the other: the first pays for reading the crate's Rust code.
open_times = []
for path in templates:
    text = texts[path]
    if one_open and path != page:
        continue
    since = time.perf_counter()
    c.notify("textDocument/didOpen", {"textDocument": {"uri": uri(path), "languageId": "html", "version": 1, "text": text}})
    open_times.append(c.wait_diagnostics(uri(path), 1, since))
results["open_first_ms"] = round(open_times[0] * 1000, 2)
results["open_rest"] = stats(open_times[1:]) if len(open_times) > 1 else None
results["rss_all_open_mb"] = c.rss_mb()

results["page"] = page.relative_to(crate).as_posix()
text = texts[page]
page_uri = uri(page)
expr = re.search(r"\{\{\s*([a-z_]+)\.([a-z_]+)", text)
var_at = expr.start(1) + 1
member_at = expr.start(2)
extends_at = text.index("{% extends") + text[text.index("{% extends"):].index('"') + 2
block = re.search(r"\{%-?\s*block\s+([a-z_]+)", text)
doc = {"textDocument": {"uri": page_uri}}


def timed(name, method, params, n=repeat):
    times = []
    result = None
    for _ in range(n):
        elapsed, result = c.request(method, params)
        times.append(elapsed)
    results[name] = stats(times)
    return result


timed("documentSymbol", "textDocument/documentSymbol", doc)
timed("foldingRange", "textDocument/foldingRange", doc)
timed("selectionRange", "textDocument/selectionRange", {**doc, "positions": [position(text, var_at)]})
timed("documentLink", "textDocument/documentLink", doc)
timed("hover_variable", "textDocument/hover", {**doc, "position": position(text, var_at)})
timed("definition_extends", "textDocument/definition", {**doc, "position": position(text, extends_at)})
if block:
    timed("references_block", "textDocument/references", {**doc, "position": position(text, block.start(1) + 1), "context": {"includeDeclaration": True}})
# Variable completion right after `{{ `.
blank = text.index("{{") + 2
timed("completion_variable", "textDocument/completion", {**doc, "position": position(text, blank + 1), "context": {"triggerKind": 2, "triggerCharacter": " "}})
timed("completion_filter", "textDocument/completion", {**doc, "position": position(text, var_at), "context": {"triggerKind": 1}})
timed("workspace_symbol_all", "workspace/symbol", {"query": ""}, n=5)
timed("workspace_symbol_query", "workspace/symbol", {"query": "content"}, n=5)

# Edits: the time from a change to its diagnostics, on the page and on the largest template.
largest = max(templates, key=lambda p: len(texts[p]))
results["largest"] = largest.relative_to(crate).as_posix()
for label, path in (("change_page", page),) + (() if one_open else (("change_largest", largest),)):
    times = []
    for version in range(2, 2 + repeat):
        since = time.perf_counter()
        changed = texts[path] + ("\n" if version % 2 else "")
        c.notify("textDocument/didChange", {"textDocument": {"uri": uri(path), "version": version}, "contentChanges": [{"text": changed}]})
        times.append(c.wait_diagnostics(uri(path), version, since))
    results[label] = stats(times)

# A burst: 30 changes 20 ms apart, as fast typing sends, then the time for completion and hover in the middle of it.
burst_completion = []
base_version = 1000
for k in range(30):
    c.notify("textDocument/didChange", {"textDocument": {"uri": page_uri, "version": base_version + k}, "contentChanges": [{"text": text + " " * (k % 2)}]})
    if k % 5 == 0:
        elapsed, _ = c.request("textDocument/hover", {**doc, "position": position(text, var_at)})
        burst_completion.append(elapsed)
    time.sleep(0.02)
results["hover_during_typing"] = stats(burst_completion)
results["rss_after_requests_mb"] = c.rss_mb()

if ra:
    # Member completion through the private rust-analyzer: the first starts it and waits for it to load.
    params = {**doc, "position": position(text, member_at), "context": {"triggerKind": 2, "triggerCharacter": "."}}
    load_start = time.perf_counter()
    first, items = c.request("textDocument/completion", params)
    results["member_completion_first_ms"] = round(first * 1000, 2)
    ready = None
    for _ in range(600):
        elapsed, items = c.request("textDocument/completion", params)
        if items:
            ready = time.perf_counter() - load_start
            break
        time.sleep(1)
    results["rust_analyzer_ready_s"] = round(ready, 1) if ready else None
    timed("member_completion_warm", "textDocument/completion", params, n=10)
    timed("member_hover_warm", "textDocument/hover", {**doc, "position": position(text, member_at + 1)}, n=10)
    # The type check after an edit: the second publication for the version, which adds the type errors.
    times = []
    for version in range(5000, 5005):
        since = time.perf_counter()
        c.notify("textDocument/didChange", {"textDocument": {"uri": page_uri, "version": version}, "contentChanges": [{"text": text + " " * (version % 2)}]})
        deadline = time.time() + 60
        with c.lock:
            c.lock.wait_for(lambda: len([1 for t, v, _ in c.diagnostics.get(page_uri, []) if v == version]) >= 2, 60)
            pubs = [t for t, v, _ in c.diagnostics.get(page_uri, []) if v == version]
        times.append(pubs[1] - since if len(pubs) >= 2 else None)
        time.sleep(0.5)
    results["type_check_after_edit"] = stats(times)
    ra_pids = subprocess.run(["pgrep", "-P", str(c.proc.pid)], capture_output=True, text=True).stdout.split()
    results["rust_analyzer_rss_mb"] = [c.rss_mb(int(p)) for p in ra_pids]
    results["rss_with_ra_mb"] = c.rss_mb()

c.request("shutdown", None)
c.notify("exit", None)
results["log_tail"] = c.log[-3:]
json.dump(results, open(out, "w"), indent=2)
print(json.dumps(results, indent=2))
