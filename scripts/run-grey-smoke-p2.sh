#!/usr/bin/env bash
set -euo pipefail

: "${GREY_PROVIDER_OPENAI_API_KEY:?export a valid OpenAI API key with prefix sk- for smoke testing}"
OPENAI_BASE_URL="${GREY_OPENAI_BASE_URL:-https://api.openai.com/v1}"
OPENAI_MODEL="${GREY_MODEL:-gpt-5.3-codex-spark}"
VOLCANO_API_KEY="${ARK_API_KEY:-}"
VOLCANO_BASE_URL="${GREY_PROVIDER_VOLCANO_BASE_URL:-https://ark.cn-beijing.volces.com/api/v3}"

export PATH="$HOME/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"
export RUSTC="$HOME/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin/rustc"

printf '\n== OpenAI smoke: gpt-5.3-codex-spark ==\n'
GREY_PROVIDER=openai \
GREY_PROVIDER_OPENAI_API_KEY="$GREY_PROVIDER_OPENAI_API_KEY" \
GREY_PROVIDER_OPENAI_BASE_URL="$OPENAI_BASE_URL" \
GREY_PROVIDER_OPENAI_MODEL="$OPENAI_MODEL" \
cargo run --locked -p grey-cli -- --provider openai --model "$OPENAI_MODEL" --no-cache --no-save --format json "请只回复 ok"

if [[ -n "$VOLCANO_API_KEY" ]]; then
  printf '\n== Volcano smoke: deepseek-v4-flash-ga-260731 ==\n'
  VOLCANO_CONFIG=$(mktemp)
  trap 'rm -f "$VOLCANO_CONFIG"' EXIT
  cat > "$VOLCANO_CONFIG" <<EOT
default_provider = "volcano"

[[providers]]
id = "volcano"
protocol = "openai"
base_url = "$VOLCANO_BASE_URL"
models = [{ id = "deepseek-v4-flash-ga-260731", name = "DeepSeek V4 Flash" }]
EOT

  GREY_CONFIG="$VOLCANO_CONFIG" \
  ARK_API_KEY="$VOLCANO_API_KEY" \
  cargo run --locked -p grey-cli -- --provider volcano --model deepseek-v4-flash-ga-260731 --no-cache --no-save --format json "请只回复 ok"
fi
