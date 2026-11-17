// The VS Code side of the Askama extension. It finds the askama-lsp executable, starts it and
// tells it which files are templates. Every language feature lives in the server. Rust files are
// sent too, for the features that go from a template struct to its template.
//
// The client's own part is Askama editing in templates (editing.ts): comment toggling with
// `{# #}`, auto-closing of `{%`, `{{` and `{#`, and Surround with Tag. Templates keep the `html`
// language, so these are added here, limited to template files, rather than in a language
// configuration, which would apply to every HTML file.

import * as fs from "node:fs";
import * as path from "node:path";
import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";
import { SURROUND_TAGS, autoClose, surround, toggleComment } from "./editing.js";

let client: LanguageClient | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  context.subscriptions.push(
    vscode.commands.registerCommand("askama.restartServer", async () => {
      await stopClient();
      await startClient(context);
    }),
    // The server's "Open template" code lens passes the template's URI as a string.
    vscode.commands.registerCommand("askama.openTemplate", async (uri: string) => {
      await vscode.window.showTextDocument(vscode.Uri.parse(uri));
    }),
    vscode.commands.registerCommand("askama.toggleComment", toggleTemplateComment),
    vscode.commands.registerCommand("askama.surroundWithTag", surroundWithTag),
    vscode.workspace.onDidChangeTextDocument(closeTag),
  );
  await startClient(context);
}

export async function deactivate(): Promise<void> {
  await stopClient();
}

async function startClient(context: vscode.ExtensionContext): Promise<void> {
  const command = serverPath(context);
  if (command === undefined) {
    void vscode.window.showErrorMessage(
      "Askama: askama-lsp was not found. Build it with cargo build, or set askama.server.path.",
    );
    return;
  }

  const serverOptions: ServerOptions = { run: { command }, debug: { command } };
  const globs = vscode.workspace
    .getConfiguration("askama")
    .get<string[]>("templateGlobs", ["**/templates/**"]);
  const settings = vscode.workspace.getConfiguration("askama");
  const clientOptions: LanguageClientOptions = {
    initializationOptions: {
      typeCompletion: settings.get<boolean>("typeCompletion", true),
      rustAnalyzerPath: rustAnalyzerPath(settings),
    },
    documentSelector: [
      ...globs.map((pattern) => ({ scheme: "file", pattern })),
      { scheme: "file", language: "rust" },
    ],
  };

  client = new LanguageClient("askama", "Askama Language Server", serverOptions, clientOptions);
  await client.start();
}

async function stopClient(): Promise<void> {
  if (client !== undefined) {
    await client.stop();
    client = undefined;
  }
}

// The setting wins. Otherwise a packaged extension carries the server in server/, and a
// development checkout uses the debug build in the repository's own target directory. Run from
// the checkout (F5), the debug build comes first, so a server/ left by packaging is not used.
function serverPath(context: vscode.ExtensionContext): string | undefined {
  const configured = vscode.workspace.getConfiguration("askama").get<string>("server.path", "");
  if (configured !== "") {
    return configured;
  }
  const executable = process.platform === "win32" ? "askama-lsp.exe" : "askama-lsp";
  const packaged = context.asAbsolutePath(path.join("server", executable));
  const development = context.asAbsolutePath(path.join("..", "..", "target", "debug", executable));
  const candidates = context.extensionMode === vscode.ExtensionMode.Development ? [development, packaged] : [packaged, development];
  return candidates.find((candidate) => fs.existsSync(candidate));
}

// The rust-analyzer for type completion: the setting, else the one the rust-analyzer
// extension ships, else none, and the server looks for `rust-analyzer` on the PATH. The one
// on the PATH is often only rustup's proxy, which fails unless its component is installed.
function rustAnalyzerPath(settings: vscode.WorkspaceConfiguration): string {
  const configured = settings.get<string>("rustAnalyzer.path", "");
  if (configured !== "") {
    return configured;
  }
  const extension = vscode.extensions.getExtension("rust-lang.rust-analyzer");
  if (extension !== undefined) {
    const executable = process.platform === "win32" ? "rust-analyzer.exe" : "rust-analyzer";
    const bundled = path.join(extension.extensionPath, "server", executable);
    if (fs.existsSync(bundled)) {
      return bundled;
    }
  }
  return "";
}


// The templates: HTML files matching `askama.templateGlobs`, as the server is told.
function isTemplate(document: vscode.TextDocument): boolean {
  const globs = vscode.workspace.getConfiguration("askama").get<string[]>("templateGlobs", ["**/templates/**"]);
  const selector = globs.map((pattern) => ({ scheme: "file", language: "html", pattern }));
  return vscode.languages.match(selector, document) > 0;
}

// Comments out the selected lines with `{# … #}`, or uncomments them. Cmd+/ runs this in
// templates; anywhere else it does what Cmd+/ normally does.
async function toggleTemplateComment(): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (editor === undefined) {
    return;
  }
  const document = editor.document;
  if (!isTemplate(document)) {
    await vscode.commands.executeCommand("editor.action.commentLine");
    return;
  }
  const eol = document.eol === vscode.EndOfLine.CRLF ? "\r\n" : "\n";
  const ranges: Array<[number, number]> = [];
  for (const selection of editor.selections) {
    const first = selection.start.line;
    // A selection that ends at the start of a line does not take that line.
    const last = selection.end.line > first && selection.end.character === 0 ? selection.end.line - 1 : selection.end.line;
    if (!ranges.some(([start, end]) => first <= end && last >= start)) {
      ranges.push([first, last]);
    }
  }
  await editor.edit((edit) => {
    for (const [first, last] of ranges) {
      const lines = [];
      for (let line = first; line <= last; line++) {
        lines.push(document.lineAt(line).text);
      }
      edit.replace(new vscode.Range(first, 0, last, document.lineAt(last).text.length), toggleComment(lines).join(eol));
    }
  });
}

// Wraps the selection in a block tag pair the user picks. Text within a line is wrapped where it
// is; whole lines, or the cursor's line when nothing is selected, get the tags on their own lines.
async function surroundWithTag(): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (editor === undefined) {
    return;
  }
  const picked = await vscode.window.showQuickPick(
    SURROUND_TAGS.map(({ tag, closer }) => ({ label: tag, description: `{% ${tag} %} … {% ${closer} %}` })),
    { placeHolder: "Surround with" },
  );
  if (picked === undefined) {
    return;
  }
  const document = editor.document;
  const selection = editor.selection;
  const wholeLines = selection.isEmpty || !selection.isSingleLine;
  let range: vscode.Range = selection;
  if (wholeLines) {
    const last = selection.end.line > selection.start.line && selection.end.character === 0 ? selection.end.line - 1 : selection.end.line;
    range = last + 1 < document.lineCount
      ? new vscode.Range(selection.start.line, 0, last + 1, 0)
      : new vscode.Range(selection.start.line, 0, last, document.lineAt(last).text.length);
  }
  const snippet = surround(picked.label, document.getText(range), wholeLines);
  await editor.insertSnippet(new vscode.SnippetString(snippet), range, { undoStopBefore: true, undoStopAfter: true, keepWhitespace: true });
}

// After `{%`, `{{` or `{#` is typed in a template, adds its closer: `{% | %}`. The editor's own
// auto-closing has usually put a `}` after the cursor already; that is replaced.
function closeTag(event: vscode.TextDocumentChangeEvent): void {
  if (event.reason !== undefined || event.contentChanges.length !== 1) {
    return;
  }
  const change = event.contentChanges[0];
  // A typed `%` or `#`, or a typed `{` with or without the editor's auto-closed `}`.
  if (change.rangeLength !== 0 || !["{", "{}", "%", "#"].includes(change.text)) {
    return;
  }
  const editor = vscode.window.activeTextEditor;
  if (editor === undefined || editor.document !== event.document || !isTemplate(event.document)) {
    return;
  }
  const cursor = change.range.start.translate(0, 1);
  const line = event.document.lineAt(cursor.line).text;
  const edit = autoClose(line.slice(0, cursor.character), line.slice(cursor.character));
  if (edit === undefined) {
    return;
  }
  void editor
    .edit((builder) => builder.replace(new vscode.Range(cursor, cursor.translate(0, edit.remove)), edit.insert), {
      undoStopBefore: false,
      undoStopAfter: true,
    })
    .then((applied) => {
      if (applied) {
        const at = cursor.translate(0, edit.cursorAfter);
        editor.selection = new vscode.Selection(at, at);
      }
    });
}
