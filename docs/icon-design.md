# Grey 图标设计简报（Icon Design Brief）

> 本文档用于交给 AI 图像生成工具产出 Grey 的 Logo/图标。\
> 生成后请将主图标保存为 `docs/assets/grey-logo.png`（512×512），并额外导出一份单色版本备用。

## 品牌背景

- 产品名：**Grey**（灰）
- 定位：一个轻量、高性能、可扩展的 **Coding Agent Harness**（“驾驭 Agent 的缰绳/马具”）。
- 关键词：极简、快、省、顺、可驾驭（controlled / guided / harnessed）。
- 已有视觉锚点：产品内置主题 `grey_storm`，强调色为 **#44E0D3（电光青）**，底色为深灰黑（#111827）。

## 核心概念（叙事）

“Grey 是 Gray（《群星》中灰风的经典二创形象：[https://zh.moegirl.org.cn/%E7%81%B0%E9%A3%8E](https://zh.moegirl.org.cn/%E7%81%B0%E9%A3%8E)）的妹妹，是一个超级管理者，把 Agent 和它的工具管理的井井有条，由你掌控。”

图标应同时表达两层含义：

1. **灰** — 名称本身：以灰色阶为主体的图形，克制、中性但偏女性、工具感。
2. **Harness / 驾驭** — 结构上要有一个“闭环/环扣/轨道”，暗示：连接、引导、可控的循环（agent loop）。
3. **闭眼双手合十祈祷** - 姿势上要展现出虔诚，寓意祈求欧姆弥赛亚的祝福

## 推荐构图方向（任选其一，或由生成模型融合）

1. **倾斜星环**：一个几何化的字母 `G`，其下部圆弧自然延伸成一个“环”，环中一颗青色圆点代表被驾驭的 Agent/模型节点。简单、耐看、可读性最强。
2. **结扣/双弧**：两段交错弧线构成一个“绳结 + G”的抽象符号，象征 Agent 与工具/MCP 的连接与约束。
3. **光标闭环**：终端光标（caret）的楔形绕成一个闭环箭头，代表 CLI 与 Agent 循环（loop / goal）。
4. **穿环的线**：一条线穿过圆环（如针眼），一侧是实心环（本地），一侧是发光点（模型），表达“把外部能力穿进本地 harness”。

## 色板

| 用途       | 色值                    | 说明                               |
| -------- | --------------------- | -------------------------------- |
| 主色（灰）    | `#111827` → `#374151` | 图形主体，深灰到中灰，克制的工具感                |
| 高亮（青）    | `#44E0D3`             | 强调/信号色，与产品 `grey_storm` 主题一致     |
| 次级高亮（可选） | `#F59E0B`             | 琥珀色，用于“注意/批准”类点缀，尽量少用            |
| 纯色模式     | 纯黑 `#000` 或纯白 `#FFF`  | 必须能降级为单色（favicon / 水印 / 深色与浅色背景） |

## 风格关键词（直接给生成模型）

- Flat / flat vector（扁平矢量），**无渐变主体**（允许灰色阶的少量平滑过渡，但主体必须单色可辨）
- Geometric、minimal、rounded（圆角、几何、极简）
- Modern developer-tool / CLI aesthetic（现代开发者工具气质）
- Symmetrical or balanced、centered、monogram-like（居中、类字标）
- Duotone：灰 + 青（#44E0D3）
- No photo, no 3D render, no glassmorphism, no heavy shadow

## 技术约束

- 正方形构图，内容居中并留安全边距（约 10%）。
- 最小可辨认尺寸 **16×16 px**（favicon）：图形需在 16px 下仍清晰，线条不宜过细。
- 标准尺寸 512×512 px（app icon / README hero）。
- 需要两版：**彩色版**（灰+青）与**纯色版**（单色）。
- 导出格式：PNG（透明背景）或 SVG 源文件。

## 可直接使用的生成 Prompt

> Flat vector logo mark for a coding-agent harness tool named "Grey". A minimal,\
> geometric letter "G" whose lower curve extends into a closed ring; inside the\
> ring sits one small solid circle acting as the guided agent node. Duotone\
> palette: dark charcoal grey (#111827 to #374151) body with one electric teal\
> (#44E0D3) accent dot. Rounded, modern developer-tool style, flat, no gradient\
> in the main mark, centered on a transparent background, thick enough to read\
> at 16px favicon size. Also provide a single-color black-and-white variant.