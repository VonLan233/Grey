#!/usr/bin/env bash
set -euo pipefail

ROOT="${GREY_REPO_ROOT:-$(pwd)}"; LIVE=false; LONG=false
die(){ printf '[TEST-ALL] ERROR: %s\n' "$*" >&2; exit 1; }
run(){ printf '[TEST-ALL] RUN %s\n' "$*"; "$@"; printf '[TEST-ALL] PASS %s\n' "$1"; }
self_test(){ scripts/run-grey-p6-perf-gates.sh --self-test; scripts/run-grey-p8-soak.sh --self-test; printf '{"status":"PASS","gate":"test-all-self-test"}\n'; }
live(){ [[ -n "${ARK_API_KEY:-}" && -n "${ARK_MODEL:-}" ]] || die '--live requires ARK_API_KEY and ARK_MODEL'; python3 - "$ARK_MODEL" <<'PY'
import json, os, sys, urllib.request
url = "https://ark.cn-beijing.volces.com/api/coding/v3/chat/completions"
request = urllib.request.Request(url, data=json.dumps({"model": sys.argv[1], "messages": [{"role": "user", "content": "Reply OK"}], "max_tokens": 1}).encode(), headers={"Authorization": "Bearer " + os.environ["ARK_API_KEY"], "Content-Type": "application/json"})
try:
    with urllib.request.urlopen(request, timeout=30) as response:
        body = json.load(response); print(json.dumps({"status": response.status, "provider": "volcano-coding-plan", "model": body.get("model", sys.argv[1]), "usage": body.get("usage", {})}, separators=(",", ":")))
except Exception as error:
    print(json.dumps({"status": "error", "provider": "volcano-coding-plan", "model": sys.argv[1], "usage": {}}, separators=(",", ":"))); raise SystemExit(str(error))
PY
  if command -v target/release/grey >/dev/null 2>&1; then target/release/grey auth status openai 2>/dev/null | awk -F': ' '$1=="logged_in"{print "{\\\"status\\\":\\\"" $2 "\\\",\\\"provider\\\":\\\"openai-oauth\\\"}"}'; fi
}
main(){ cd "$ROOT"; while [[ $# -gt 0 ]];do case "$1" in --self-test)self_test;return;;--live)LIVE=true;;--long)LONG=true;;*)die 'usage: --self-test [--live] [--long]';;esac;shift;done; run rustup run 1.97.1 cargo --version; run git diff --check; run rustup run 1.97.1 cargo fmt --all -- --check; run rustup run 1.97.1 cargo clippy --workspace --all-targets --all-features -- -D warnings; run rustup run 1.97.1 cargo test --workspace --all-features --locked; run rustup run 1.97.1 cargo test --workspace --all-features --locked --doc; run rustup run 1.97.1 cargo build --workspace --release --locked; run scripts/run-grey-p6-perf-gates.sh; if "$LONG";then run scripts/run-grey-p8-soak.sh --long;else run scripts/run-grey-p8-soak.sh;fi; "$LIVE"&&live; printf '{"status":"PASS","gate":"test-all"}\n'; }
main "$@"
