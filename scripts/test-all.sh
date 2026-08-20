#!/usr/bin/env bash
set -euo pipefail

ROOT="${GREY_REPO_ROOT:-$(pwd)}"; LIVE=false; LONG=false
die(){ printf '[TEST-ALL] ERROR: %s\n' "$*" >&2; exit 1; }
run(){ printf '[TEST-ALL] RUN %s\n' "$*"; "$@"; printf '[TEST-ALL] PASS %s\n' "$1"; }
self_test(){ scripts/run-grey-p6-perf-gates.sh --self-test; scripts/run-grey-p8-soak.sh --self-test; printf '{"status":"PASS","gate":"test-all-self-test"}\n'; }
live(){ local config status; [[ -n "${ARK_API_KEY:-}" && -n "${ARK_MODEL:-}" ]] || die '--live requires ARK_API_KEY and ARK_MODEL'; [[ -x "$ROOT/target/release/grey" ]] || die 'release Grey binary required for --live'; config="$(mktemp)"; printf '%s\n' 'default_provider = "volcano"' "default_model = \"$ARK_MODEL\"" '' '[[providers]]' 'id = "volcano"' 'protocol = "openai"' 'base_url = "https://ark.cn-beijing.volces.com/api/coding/v3"' 'api_key = "${ARK_API_KEY}"' >"$config"; if GREY_CONFIG="$config" "$ROOT/target/release/grey" --provider volcano --model "$ARK_MODEL" --no-cache --no-save --no-fallback --format json 'Reply OK'; then status=0; else status=$?; fi; rm -f -- "$config"; ((status==0)) || return "$status"; if command -v "$ROOT/target/release/grey" >/dev/null 2>&1; then "$ROOT/target/release/grey" auth status openai 2>/dev/null | awk -F': ' '$1=="logged_in"{print "{\\\"status\\\":\\\"" $2 "\\\",\\\"provider\\\":\\\"openai-oauth\\\"}"}'; fi
}
main(){ cd "$ROOT"; while [[ $# -gt 0 ]];do case "$1" in --self-test)self_test;return;;--live)LIVE=true;;--long)LONG=true;;*)die 'usage: --self-test [--live] [--long]';;esac;shift;done; run rustup run 1.97.1 cargo --version; run git diff --check; run rustup run 1.97.1 cargo fmt --all -- --check; run rustup run 1.97.1 cargo clippy --workspace --all-targets --all-features -- -D warnings; run rustup run 1.97.1 cargo test --workspace --all-features --locked; run rustup run 1.97.1 cargo test --workspace --all-features --locked --doc; run rustup run 1.97.1 cargo build --workspace --release --locked; run scripts/run-grey-p6-perf-gates.sh; if "$LONG";then run scripts/run-grey-p8-soak.sh --long;else run scripts/run-grey-p8-soak.sh;fi; "$LIVE"&&live; printf '{"status":"PASS","gate":"test-all"}\n'; }
main "$@"
