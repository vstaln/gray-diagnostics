#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/diagnostics.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/diagnostics" ;;
    *) rel="x86_64-apple-darwin/diagnostics" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/diagnostics" ;;
    *) rel="x86_64-unknown-linux-musl/diagnostics" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/diagnostics.exe" "$here/target/release/diagnostics" "$here/target/debug/diagnostics.exe" "$here/target/debug/diagnostics"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "diagnostics binary not found in $here" >&2
exit 1
