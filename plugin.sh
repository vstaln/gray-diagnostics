#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/lint.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/lint" ;;
    *) rel="x86_64-apple-darwin/lint" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/lint" ;;
    *) rel="x86_64-unknown-linux-musl/lint" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/lint.exe" "$here/target/release/lint" "$here/target/debug/lint.exe" "$here/target/debug/lint"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "lint binary not found in $here" >&2
exit 1
