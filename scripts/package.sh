#!/bin/sh
# Builds Askama IntelliSense as a VSIX for this machine's platform, with a release build of the
# language server inside: askama-vscode-<platform>-<version>.vsix at the repository root.
# Install it with "Extensions: Install from VSIX..." in VS Code.
set -eu
cd "$(dirname "$0")/.."

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=darwin-arm64 ;;
  Darwin-x86_64) target=darwin-x64 ;;
  Linux-x86_64) target=linux-x64 ;;
  Linux-aarch64) target=linux-arm64 ;;
  *) echo "package.sh: no VS Code target for $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

cargo build --release -p askama-lsp
mkdir -p editors/vscode/server
cp target/release/askama-lsp editors/vscode/server/askama-lsp

cd editors/vscode
npm install --no-audit --no-fund
version=$(node -p "require('./package.json').version")
# vscode:prepublish bundles the client. The bundle holds every dependency, so none are packed;
# the extension is private, so it has no license file or repository to show.
npx vsce package --target "$target" --no-dependencies --skip-license --allow-missing-repository \
  --out "../../askama-vscode-$target-$version.vsix"
