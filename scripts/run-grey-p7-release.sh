#!/usr/bin/env bash
set -euo pipefail

# Builds and packages a GitHub-release artifact for Grey (no real publish).
# Usage: scripts/run-grey-p7-release.sh [--self-test] [--out DIR] [--version V]
# Artifacts written to output/release/ (overridable with --out):
#   grey-<version>-<os>-<arch>.tar.gz, SHA256SUMS, RELEASE_NOTES.md

ROOT="${P7_REPO_ROOT:-$(pwd)}"; OUT="${P7_OUT:-$ROOT/output/release}"; CARGO="${P7_CARGO:-rustup run 1.97.1 cargo}"
VERSION="${P7_VERSION:-$(sed -n 's/^version = "\([^"]*\)".*/\1/p' "$ROOT/Cargo.toml" | head -1)}"
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"; ARCH="$(uname -m | sed 's/arm64/aarch64/; s/x86_64/x86_64/')"
TMP=()
die(){ printf '[P7 RELEASE] ERROR: %s\n' "$*" >&2; exit 1; }
log(){ printf '%s\n' "$*"; }
clean(){ local f; for f in "${TMP[@]:-}"; do rm -rf -- "$f"; done; }; trap clean EXIT; trap 'clean; exit 1' ERR HUP INT TERM
tmp(){ local f; f="$(mktemp -d)"; TMP+=("$f"); printf '%s\n' "$f"; }

package(){
  local bin="$1" stage name dir
  stage="$(tmp)"
  name="grey-$VERSION-$OS-$ARCH"
  dir="$stage/$name"
  mkdir -p "$dir/bin" "$OUT"
  cp "$bin" "$dir/bin/grey"
  chmod +x "$dir/bin/grey"
  printf '%s\n' \
    "Grey $VERSION ($OS/$ARCH)" \
    "" \
    "Install: tar -xzf $name.tar.gz && cp $name/bin/grey /usr/local/bin/grey" \
    "" \
    "Checksums: see SHA256SUMS. Requires a 64-bit $OS host." >"$dir/README.txt"
  tar -C "$stage" -czf "$OUT/$name.tar.gz" "$name" || die "tar packaging failed for $name"
  rm -rf "$dir"
  ( cd "$OUT" && shasum -a 256 "$name.tar.gz" >SHA256SUMS )
  printf '%s\n' \
    "# Grey $VERSION" \
    "" \
    "## Assets" \
    "" \
    "- \`$name.tar.gz\` (binary + README, checksum in \`SHA256SUMS\`)" \
    "" \
    "## Notes" \
    "" \
    "- Offline packaging only: binary built with \`$CARGO build --release\`; publish to a" \
    "  GitHub Release requires a configured remote + tag, and Homebrew tap / signing / SBOM" \
    "  / attestation remain external follow-ups." >"$OUT/RELEASE_NOTES.md"
  printf '%s\n' "$name.tar.gz"
}

self_test(){
  local fake name
  fake="$(mktemp)"; TMP+=("$fake")
  printf '#!/bin/sh\nexit 0\n' >"$fake"; chmod +x "$fake"
  OUT="$(tmp)" name="$(package "$fake")"
  [[ -f "$OUT/SHA256SUMS" && -f "$OUT/RELEASE_NOTES.md" && -f "$OUT/$name" ]] || die 'artifact set incomplete'
  grep -q "^[0-9a-f]\{64\}  $name\$" "$OUT/SHA256SUMS" || die 'checksum missing or malformed'
  log '{"status":"PASS","gate":"p7-self-test","artifact":"'"$name"'"}'
}

main(){
  cd "$ROOT"; [[ "${1:-}" == --self-test ]]&&{ self_test; return; }
  while [[ $# -gt 0 ]]; do case "$1" in
    --out) OUT="${2:?}"; shift 2;; --version) VERSION="${2:?}"; shift 2;;
    *) die 'usage: [--self-test] [--out DIR] [--version V]';; esac; done
  [[ -n "$VERSION" ]] || die 'unable to read version from Cargo.toml'
  [[ "$VERSION" =~ ^[0-9][0-9.]*$ ]] || die "invalid version: $VERSION"
  local bin="$ROOT/target/release/grey"
  if [[ ! -x "$bin" ]]; then $CARGO build --workspace --release --locked; fi
  local name; name="$(package "$bin")"
  printf '{"status":"PASS","gate":"p7-release-package","artifact":"%s","version":"%s"}\n' "$name" "$VERSION"
}

main "$@"
