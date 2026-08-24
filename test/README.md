# 黑洞网页空载对照 — Grey vs Pi

**任务**：完全空载（无 skill、无历史）下，用同一模型 `deepseek-v4-flash` 生成单文件黑洞网页，分别落到 `test/grey/index.html` 与 `test/pi/index.html`。

**Prompt（同文）**
```
Create a single HTML file at index.html with a black hole effect: black background, starfield, central black circle with photon ring and simple accretion disk gradient, canvas-based, no external libs, single file, responsive.
```

**执行**
- Grey: `grey --provider volcano --model deepseek-v4-flash-ga-260731 --workspace test/grey --no-cache --format json --auto-approve` (max-steps 15)
- Pi: `pi --provider volcengine-plan --model deepseek-v4-flash -p --no-session --mode json --no-skills` (same prompt)

**结果**
|  | Grey | Pi |
|---|---|---|
| 输入 tokens | 24264 (7 steps, --no-cache) | 11981 (sum 5 turns, 47+6016+4708+270+940) |
| 输出 tokens | 3518 | 10072 (sum 5898+2754+147+648+625) |
| 推理 tokens | - (Grey 不单列) | 132 (last turn) + 5831+12+15+102... |
| 总 tokens (Grey) / totalTokens (Pi) | 27782 | 14365 (last turn) / ~22000 sum |
| 产物大小 | 6.0K `test/grey/index.html` | 7.3K `test/pi/index.html` |
| 步骤 | 7 | 5 (Pi 含 2 次 bash+write) |
| 成本 (修后) | $0.00438 ((24264/1e6)*0.14 + (3518/1e6)*0.28) | $0 (free) |

**结论**
- Grey input 多 ~2x，因 system prompt 含 `read/glob/grep` 等工具定义（Pi `--no-tools` 时 2344，Grey `--read-only` 1400，同工具下 Pi 首轮 6352 vs Grey 3271，说明 Grey 基线更省）
- Grey output 少 ~65%，文件更精简（Ponytail：无多余抽象，直接 canvas）
- 修前 Grey 对该模型 `usage=0`（`ProviderEntry.include_usage` 默认 false，未发 `stream_options.include_usage`），已修为默认 `true` + `UsageShape` alias，`volcano` 现正确回 `1400/33` (hello) 和 `24264/3518` (blackhole)

**Grey 侧已更新**
- `crates/grey-core/src/config.rs`：`include_usage` 默认 `true`
- `crates/grey-provider/src/openai.rs`：`prompt_tokens` alias `input_tokens` 等
- `~/.config/grey/grey.toml`：补 `cost_per_1m` for deepseek-v4-flash (0.14/0.28)
- `grey usage show <id>` 现显示非 0 cost（如 `hello` $0.00022）

**复现**
```bash
export PATH="$HOME/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin:$PATH"
cargo build
# Grey
./target/debug/grey --provider volcano --model deepseek-v4-flash-ga-260731 --workspace test/grey --no-cache --format json --auto-approve "$(cat /tmp/simple_bh.txt)"
./target/debug/grey usage show <id>
# Pi
pi --provider volcengine-plan --model deepseek-v4-flash -p --no-session --mode json --no-skills "$(cat /tmp/simple_bh.txt)"
```
