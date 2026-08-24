# 黑洞网页空载对照 — Grey vs Pi

**任务**：完全空载（无 skill、无历史）下，用同一模型 `deepseek-v4-flash` 生成单文件黑洞网页，分别落到 `test/grey/index.html` 与 `test/pi/index.html`。

**Prompt（同文）**
```
Create a single HTML file at index.html with a black hole effect: black background, starfield, central black circle with photon ring and simple accretion disk gradient, canvas-based, no external libs, single file, responsive.
```

**执行**
- Grey: `grey --provider volcano --model deepseek-v4-flash-ga-260731 --workspace test/grey --no-cache --format json --auto-approve` (max-steps 15)
- Pi: `pi --provider volcengine-plan --model deepseek-v4-flash -p --no-session --mode json --no-skills` (same prompt)

**结果（压缩工具描述后）**
|  | Grey | Pi |
|---|---|---|
| 输入 tokens | 10411 (4 steps) | 11981 (5 turns sum) |
| 输出 tokens | 3072 | 10072 |
| 产物大小 | 6.1K `test/grey/index.html` | 7.3K `test/pi/index.html` |
| 成本 | $0.0023 | $0 (free) |

**优化**
- 工具定义精简：`read_file`/`edit_file`/`bash`/`glob`/`grep` 描述缩短，`lsp_*` 简化，空载 `hello` 从 1387→1325，黑洞任务从 24264→10411（省 57%），现比 Pi 少 13% input，少 70% output。
- 默认不带工具的激进方案因 `hello` 用例的 max-steps 失败，已回退为默认带但精简（Ponytail：删抽象不如精简描述）。

**复现**
```bash
export PATH="$HOME/.rustup/toolchains/1.97.1-aarch64-apple-darwin/bin:$PATH"
cargo build
./target/debug/grey --provider volcano --model deepseek-v4-flash-ga-260731 --workspace test/grey --no-cache --format json --auto-approve "$(cat /tmp/simple_bh.txt)"
pi --provider volcengine-plan --model deepseek-v4-flash -p --no-session --mode json --no-skills "$(cat /tmp/simple_bh.txt)"
```
