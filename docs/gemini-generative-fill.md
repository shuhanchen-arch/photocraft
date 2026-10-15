# 框选生成（Gemini Generative Fill）设计

状态（2026-10-15）：第 1 步已实现：`crates/genai`、`edit.generativeFill`（纯文字生成，按选区外接框铺满并用选区作蒙版）、
Edit › Generative Fill… 对话框，key 从 `GEMINI_API_KEY` 读取。第 2–4 步未开始。分支 `feature/gemini-image`。

## 1. 用户流程

1. 用任意选区工具框选一块区域（矩形、套索、魔棒都可以；第一版重点是矩形选框）。
2. 选区下方弹出一个**浮动提示栏**（相当于 Photoshop 的 Contextual Task Bar，本项目目前还没有这个组件）：
   `[ 输入提示词…………… ] [模型 ▾] [生成]`。菜单 **Edit › Generative Fill…** 也能打开同样的输入框。
3. 点"生成"后，状态栏显示后台任务进度，可以取消；界面不卡。
4. 结果作为**新图层**放在当前图层上方，命名为 `生成: <提示词前 30 字>`，**自带一个图层蒙版，蒙版就是选区形状**。
   原图像素一点都不改；不满意直接删掉图层，或者在蒙版上涂抹修边。
5. 提示词留空时，表示"把选区里的东西去掉，按周围环境自然补全"（和 Photoshop 一样）。

## 2. 两种模式（分阶段）

| 模式 | 上传内容 | 效果 | 阶段 |
|---|---|---|---|
| **A. 纯生成** | 只传提示词，按框的宽高比出图 | 框里出现一张全新的图，与周围无关 | 第一阶段，最简单，用来打通全链路 |
| **B. 结合场景（默认）** | 框内外一圈的**上下文截图** + 选区标记图 + 提示词 | 生成内容的光照、透视、风格和周围一致，边缘能接上 | 第二阶段，你说的"把框里的和提示词一起上传" |

两种模式共用同一条链路，区别只在请求里是否带图片。对话框里提供"参考周围画面"复选框，默认开启。

## 3. 关键细节（模式 B）

### 3.1 裁哪一块上传：上下文窗口
- 选区外接矩形 `hb = selection.content_bounds() ∩ 画布`（复用 Content-Aware Fill 的写法，`edit_menu_cmds.rs`）。
- 往外扩一圈上下文：`pad = max(64px, 0.25 × max(w, h))`，让模型能看到周围环境。框越小越需要上下文。
- **把窗口扩展到最接近的受支持宽高比**（1:1, 3:2, 2:3, 3:4, 4:3, 4:5, 5:4, 9:16, 16:9, 21:9），
  而不是把输出图拉伸变形。扩展时优先往画布内扩；碰到画布边缘就往另一侧补，实在补不了再选下一个接近的比例。
- 读取的是**可见合成图**（所有可见图层叠起来的样子），复用 `retouch_cmds::composite_region(..., SampleLayers::All, window, fmt)`。

### 3.2 上传前的格式转换
- 文档可能是 16/32 位、CMYK、Lab；上传前统一经 `photocraft-cms` 转成 **8 位 sRGB PNG**。
- 窗口超过模型输出尺寸（1K/2K/4K）时先缩小再上传；`image_size` 根据窗口长边自动选，用户也可以手动指定。

### 3.3 怎么告诉模型"只改哪里"
Gemini 目前**没有像素级蒙版参数**，只支持在对话里用文字描述要改的区域（官方叫 semantic masking）。所以：
- **图 1**：原始上下文截图。
- **图 2**：同一张图，选区部分盖一层半透明洋红色，外加 2px 描边。
- 提示词模板（内部拼接，用户看不到）：
  > Image 1 is a photo. Image 2 is the same photo with the region to change highlighted in magenta.
  > In image 1, change ONLY the highlighted region: {用户提示词}. Keep everything outside it pixel-identical,
  > with the same framing, perspective, lighting and style. Return only the edited image 1, same aspect ratio.
- 用户提示词为空时，换成"remove the content of the highlighted region and fill it naturally from the surroundings"。

### 3.4 结果怎么贴回去（保证框外绝对不变）
模型返回的图**不可信**：分辨率不同、可能轻微平移、框外颜色可能被它"顺手"改了。所以不依赖模型，在本地保证：
1. 把返回图**缩放回窗口的精确像素尺寸**（双三次插值）；宽高比在 3.1 已经对齐，所以不会变形。
2. 转回文档的位深和色彩模式，作为像素图层放在窗口位置（`place_bytes` 的 `center` 和 `scale` 参数或直接构造 Raster 图层）。
3. 加图层蒙版 = 选区，**向外扩 4px 再羽化 4px**，把接缝藏起来。框外的部分被蒙版挡住，原图保持不变。
4. （可选，第二阶段后半）**色彩校正**：比较选区外一圈环带上"生成图 vs 原图"的均值和方差，对生成图做一次线性颜色匹配，消除整体偏色。
5. （可选，以后）**对齐校正**：模型偶尔整体平移几个像素时，用环带做相位相关，求出偏移后再贴。

### 3.5 放大过度的提醒
如果窗口比模型输出大很多（比如在 8000px 的照片上框了半张图，模型只出 2K），贴回去要放大 2 倍以上，会发糊。
放大超过 2 倍时，状态栏提示"结果已放大 N 倍，可能不够清晰，建议缩小框选范围或选 4K"。

## 4. 代码落点

| 文件 | 作用 |
|---|---|
| `crates/genai/`（新，L4） | Gemini REST 客户端：组装请求、发送、解析返回图；`ureq` + `rustls` + `base64`；wasm 下返回"不支持" |
| `crates/engine/src/genai_cmds.rs`（新） | 命令 `edit.generativeFill`：取选区 → 算窗口 → 读合成图 → `jobs::run` 后台请求 → 贴回 + 加蒙版，一次撤销 |
| `crates/engine/src/prefs.rs` | 新增 `ai` 偏好段：`api_key`、`model`、`image_size`、`use_context` |
| `crates/ui-egui/src/genfill_bar.rs`（新） | 选区下方的浮动提示栏 |
| `crates/ui-egui/src/prefs_ui` | 偏好设置 › AI 页 |
| `crates/ui-egui/src/menu_catalog.rs` | Edit 菜单里加 `Generative Fill…` |
| `crates/engine/src/commands.rs` | 注册 `genai_cmds::specs()` |
| `xtask/src/layers.rs` | 登记新 crate 的层级 |

命令参数（Agent、CLI、MCP 也能直接调用）：

```json
{ "prompt": "a red umbrella", "context": true, "model": "gemini-nano-banana-2.1",
  "imageSize": "auto|1K|2K|4K", "feather": 4, "expand": 4 }
```

## 5. API 与 key

- 接口：Gemini Interactions API `POST https://generativelanguage.googleapis.com/v1beta/interactions`，请求头 `x-goog-api-key`。
  `input` 里依次放图片（`type: image`、`mime_type`、`data` 为 base64）和文本；`response_format` 里设
  `type: image`、`aspect_ratio`、`image_size`（必须大写 K）。
- 默认模型 `gemini-nano-banana-2.1`；可选 `gemini-3-pro-image`（质量最好）、`gemini-3.1-flash-lite-image`（最便宜，只支持 1K）。
  模型名写成可配置项，以防 Google 再改名。
- 所有生成图都带 SynthID 隐形水印（Google 的规定）。
- key 读取顺序：命令参数 `apiKey` → 环境变量 `GEMINI_API_KEY` → 偏好设置。偏好设置是明文 JSON，以后改用系统凭据管理器（`keyring`）。
- **隐私提示**：第一次使用时弹窗说明"框选区域附近的画面会上传到 Google"，用户确认后才发送。

## 6. 出错处理（遵守"不许崩溃"规则）

- 没有选区、选区在画布外、没配 key：命令直接返回可读的错误，菜单项置灰并给出原因。
- 网络超时（默认 120 秒）、HTTP 4xx/5xx、被安全策略拦截、返回里没有图片：作为任务失败显示原因，文档不变。
- 取消：阻塞的 HTTP 请求无法中途打断，取消时把结果丢弃即可。
- 所有尺寸计算用 `checked_*`，窗口面积设上限，拒绝超大分配。
- 新命令进入 `panic_hunt` 模糊测试；网络部分用本地 mock 响应测试，CI 不真的调 Google。

## 7. 实施顺序

1. `crates/genai` + 纯生成（模式 A）+ 菜单入口 + 环境变量 key：先打通"框选 → 出图 → 新图层 + 蒙版"。
2. 模式 B：上下文窗口、宽高比对齐、标记图、提示词模板、贴回羽化。
3. 浮动提示栏 + 偏好设置 AI 页 + 首次使用隐私提示。
4. 色彩校正、放大提醒、多候选（一次生成 3 张，可在图层属性里切换）。
