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

That writes `askama-vscode-<platform>-<version>.vsix` here, for this machine's platform. Install it in VS Code with **Extensions: Install from VSIX…**. `VSCODE_TARGET` and `CARGO_BUILD_TARGET` build for another platform: `VSCODE_TARGET=darwin-x64 CARGO_BUILD_TARGET=x86_64-apple-darwin sh scripts/package.sh` builds the Intel Mac package on an Apple silicon Mac (after `rustup target add x86_64-apple-darwin`).

## Release it

`.github/workflows/release.yml` builds, tests and packages the extension for every platform VS Code runs on (darwin-arm64, darwin-x64, linux-x64, linux-arm64, win32-x64) on each push to `main` and each pull request, and keeps the packages as the run's artifacts. To publish a release:

1. Once, add the repository secrets (Settings → Secrets and variables → Actions): `VSCE_PAT`, an Azure DevOps personal access token with the Marketplace → Manage scope for all accessible organizations, and, to publish to Open VSX too, `OVSX_PAT`, an open-vsx.org access token.
2. Set `version` in `editors/vscode/package.json` (and in `Cargo.toml`), and add a section for it at the top of `editors/vscode/CHANGELOG.md`, headed `## <version> (<date>)`.
3. Commit and push, then tag the commit and push the tag:

   ```bash
   git tag v1.0.0
   git push origin v1.0.0
   ```

The tag runs the same builds, then publishes every package to the Visual Studio Marketplace (and Open VSX), and creates a GitHub release with the packages attached and the changelog's section as its notes. A tag that does not match the version in `package.json` publishes nothing.

## Docs

- `docs/design.md`: the architecture and how it works today.
