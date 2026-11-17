# Askama IntelliSense

A VS Code extension, **Askama IntelliSense**, for [Askama](https://github.com/askama-rs/askama) templates: highlighting, syntax errors as you type, navigation between templates, and completion that knows the template's Rust struct.

It is a thin TypeScript client (`editors/vscode`) around a Rust language server (`server`, the `askama-lsp` binary). `fixtures/sample-site` is a small Askama crate to try it on.

## Run it

```bash
cd editors/vscode && npm install
```

Then open this folder in VS Code and press F5 (**Run Askama extension**). It builds the server and the client and opens `fixtures/sample-site` in an Extension Development Host.

## Package it

```bash
sh scripts/package.sh
```

That writes `askama-vscode-<platform>-<version>.vsix` here. Install it in VS Code with **Extensions: Install from VSIX…**.

## Docs

- `docs/design.md`: the architecture and how it works today.
- `docs/implementation-plan.md`: the steps, each ending with something to try in VS Code.
