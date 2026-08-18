#!/usr/bin/env bash
set -euo pipefail

OPENAI_API_KEY="${GREY_PROVIDER_OPENAI_API_KEY:-${OPENAI_API_KEY:-${YUNWU_API_KEY:-}}}"
OPENAI_BASE_URL="${GREY_PROVIDER_OPENAI_BASE_URL:-https://api.openai.com/v1}"
OPENAI_MODEL="${GREY_PROVIDER_OPENAI_MODEL:-gpt-5.3-codex-spark}"
VOLCANO_API_KEY="${ARK_API_KEY:-${VOLCANO_API_KEY:-${GREY_PROVIDER_VOLCANO_API_KEY:-}}}"
VOLCANO_BASE_URL="${GREY_PROVIDER_VOLCANO_BASE_URL:-https://ark.cn-beijing.volces.com/api/v3}"
VOLCANO_MODEL="${GREY_PROVIDER_VOLCANO_MODEL:-deepseek-v4-flash-ga-260731}"
SMOKE_CONFIG="$(mktemp)"

trap 'rm -f "$SMOKE_CONFIG"' EXIT

export PATH="$HOME/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"
export RUSTC="$HOME/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin/rustc"

unset GREY_OPENAI_API_KEY GREY_OPENAI_BASE_URL GREY_OPENAI_MODEL GREY_OPENAI_INCLUDE_USAGE GREY_OPENAI_VERSION

write_smoke_config() {
  > "$SMOKE_CONFIG"
  if [[ -n "$OPENAI_API_KEY" ]]; then
    cat >> "$SMOKE_CONFIG" <<EOF
[[providers]]
id = "openai"
protocol = "openai"
base_url = "$OPENAI_BASE_URL"
api_key = "$OPENAI_API_KEY"
model = "$OPENAI_MODEL"

EOF
  fi
  if [[ -n "$VOLCANO_API_KEY" ]]; then
    cat >> "$SMOKE_CONFIG" <<EOF
[[providers]]
id = "volcano"
protocol = "openai"
base_url = "$VOLCANO_BASE_URL"
api_key = "$VOLCANO_API_KEY"
models = [{ id = "$VOLCANO_MODEL", name = "DeepSeek V4 Flash" }]

EOF
  fi
}

run_openai_smoke() {
  printf '\n== OpenAI smoke: %s ==\n' "$OPENAI_MODEL"
  GREY_CONFIG="$SMOKE_CONFIG" \
  cargo run --locked -p grey-cli -- --provider openai --model "$OPENAI_MODEL" --no-cache --no-save --format json "请只回复 ok"
}

run_volcano_smoke() {
  printf '\n== Volcano smoke: %s ==\n' "$VOLCANO_MODEL"
  GREY_CONFIG="$SMOKE_CONFIG" \
  ARK_API_KEY="$VOLCANO_API_KEY" \
  cargo run --locked -p grey-cli -- --provider volcano --model "$VOLCANO_MODEL" --no-cache --no-save --format json "请只回复 ok"
}

ok=true
write_smoke_config

if [[ -z "$OPENAI_API_KEY" && -z "$VOLCANO_API_KEY" ]]; then
  echo "Skip smoke: no OpenAI (GREY_PROVIDER_OPENAI_API_KEY/OPENAI_API_KEY/YUNWU_API_KEY) nor ARK_API_KEY found."
  exit 1
fi

if [[ -n "$OPENAI_API_KEY" ]]; then
  if ! run_openai_smoke; then
    ok=false
  fi
else
  printf '\nSkip OpenAI smoke: no OpenAI API key set (GREY_PROVIDER_OPENAI_API_KEY, OPENAI_API_KEY, or YUNWU_API_KEY).\n'
fi

if [[ -n "$VOLCANO_API_KEY" ]]; then
  if ! run_volcano_smoke; then
    ok=false
  fi
else
  printf '\nSkip Volcano smoke: ARK_API_KEY not set. Set ARK_API_KEY to run DeepSeek smoke.\n'
fi

if [[ "$ok" == "false" ]]; then
  exit 1
fi
