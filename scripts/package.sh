#!/bin/sh
# Builds Askama IntelliSense as a VSIX with a release build of the language server inside:
# askama-vscode-<platform>-<version>.vsix at the repository root. Install it with "Extensions: Install from VSIX..." in VS Code.
#
# By default it builds for this machine. VSCODE_TARGET names another VS Code platform (darwin-arm64, darwin-x64, linux-x64, linux-arm64,
# win32-x64), and CARGO_BUILD_TARGET the Rust target to build the server for when it is not this machine's (x86_64-apple-darwin, to build
# darwin-x64 on an Apple silicon Mac). The release workflow (.github/workflows/release.yml) sets both.
set -eu
cd "$(dirname "$0")/.."

target=${VSCODE_TARGET:-}
if [ -z "$target" ]; then
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) target=darwin-arm64 ;;
    Darwin-x86_64) target=darwin-x64 ;;
    Linux-x86_64) target=linux-x64 ;;
    Linux-aarch64) target=linux-arm64 ;;
    MINGW*-x86_64 | MSYS*-x86_64 | CYGWIN*-x86_64) target=win32-x64 ;;
    *) echo "package.sh: no VS Code target for $(uname -s) $(uname -m)" >&2; exit 1 ;;
  esac
fi
case "$target" in
  win32-*) exe=.exe ;;
  *) exe= ;;
esac

# Cargo reads CARGO_BUILD_TARGET itself, and puts the build under target/<triple>/ when it is set. An empty one would be an error.
if [ -n "${CARGO_BUILD_TARGET:-}" ]; then
  built=target/$CARGO_BUILD_TARGET/release
else
  unset CARGO_BUILD_TARGET
  built=target/release
fi
cargo build --release -p askama-lsp
# Only this target's server goes in the package, never one left from an earlier build for another.
rm -rf editors/vscode/server
mkdir -p editors/vscode/server
cp "$built/askama-lsp$exe" "editors/vscode/server/askama-lsp$exe"

cd editors/vscode
# Exactly what package-lock.json holds, never newer versions a fresh resolution would pick.
npm ci --no-audit --no-fund
version=$(node -p "require('./package.json').version")
# vscode:prepublish bundles the client. The bundle holds every dependency, so none are packed.
# editors/vscode/LICENSE is a copy of the repository's LICENSE, which vsce needs beside the manifest.
cp ../../LICENSE LICENSE
npx vsce package --target "$target" --no-dependencies --out "../../askama-vscode-$target-$version.vsix"
