# 框选生成（Gemini Generative Fill）设计

状态（2026-10-15）：第 1、2 步已实现。第 1 步：`crates/genai`、`edit.generativeFill`、Edit › Generative Fill… 对话框；首次真实调用发现模型只接受 `response_format.mime_type: image/jpeg`，已改。第 2 步（默认开启，参数 `context`，偏好设置 `ai.useSurroundingImage`，对话框复选框"参考周围画面"）：上下文窗口 + 宽高比对齐（`context_window`）、一张 8 位 sRGB PNG（选区外画红色轮廓线，见 3.3）、提示词模板（空提示词 = 移除）、结果校验 + 无效时自动重试一次（见 3.4 第 6 条）、精确缩放回窗口、从 sRGB 转回文档配置文件、对齐校正（3.4 第 5 条，`genai_align.rs`：在红色轮廓线外侧 32px 宽的环带上先粗后细搜索平移，最多窗口长边的 2%、不超过 24px，结果写进返回值 `alignment`）、蒙版扩 4px 羽化 4px、取样图层（参数 `sampleLayer`：所有图层 / 当前及下方图层 / 当前图层，对话框"取样"下拉，记住上次的选择；连续生成时选"当前图层"并选中原图层，就不会把之前的生成结果当成周围画面）。窗口找不到能放进画布的受支持比例时（比如选区铺满一张 7:5 的画布），用最接近的比例，贴回时轻微拉伸。第 3–4 步未开始。分支 `feature/gemini-image`。

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
| **B. 结合场景（默认）** | 框内外一圈的**上下文截图**（选区外画红色轮廓线）+ 提示词 | 生成内容的光照、透视、风格和周围一致，边缘能接上 | 第二阶段，你说的"把框里的和提示词一起上传" |

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
**原则：不指望模型听话，而是让它犯错也无害。** 只发**一张图**：上下文截图，选区内保持原样，在选区外沿选区形状画一圈
3px 的纯红轮廓线（`genai_cmds::Outline`）。红线从"生成图层可见范围"再往外 8px 开始（默认蒙版扩 4px、羽化 σ=4，
可见范围约 4 + 3×4 = 16px，所以红线在选区外 24–27px）；上传前缩小了的话，线宽按比例加粗，保证上传图里仍有 3px。
贴回时选区外的像素全部来自原图，所以就算模型把红线原样留着，也落在被遮住的地方。

试过又放弃的做法（2026-10-15 实测）：
- 第二张图是"同一张照片，选区盖半透明洋红 + 2px 描边"：模型有时把这张标记图当成底图来改，洋红色渗进选区，
  而且下一次生成会把带紫色的图层当成周围画面上传，越来越紫。
- 第二张图换成黑白蒙版：模型又把蒙版的白色叠进了选区（船周围一圈白光、水面一大片纯白）。
- 结论：Gemini 是通用编辑模型，会把输入的几张图"合成"进结果；只要选区里有标记、或者有第二张图，就有渗进去的风险。

提示词模板（内部拼接，用户看不到，见 `genai_cmds::context_prompt`）：
> This photo has a red outline drawn on it. Change ONLY the area inside the red outline: change it to: {用户提示词}.
> Keep any new object entirely inside the outline, well away from it. Keep everything outside the outline exactly as
> it is, with the same framing, perspective, lighting and colours, and blend the change in seamlessly. Return the
> edited photo without the red outline, at the same size and aspect ratio.

用户提示词为空时，换成"remove what is there and fill it in naturally from the surroundings"。

以后要接真正的局部重绘模型（本地 ComfyUI + FLUX.1 Fill [dev]，或云端 API）时，它们原生接受蒙版，就不需要红线了。

### 3.4 结果怎么贴回去（保证框外绝对不变）
模型返回的图**不可信**：分辨率不同、可能轻微平移、框外颜色可能被它"顺手"改了。所以不依赖模型，在本地保证：
1. 把返回图**缩放回窗口的精确像素尺寸**（双三次插值）；宽高比在 3.1 已经对齐，所以不会变形。
2. 转回文档的位深和色彩模式，作为像素图层放在窗口位置（`place_bytes` 的 `center` 和 `scale` 参数或直接构造 Raster 图层）。
3. 加图层蒙版 = 选区，**向外扩 4px 再羽化 4px**，把接缝藏起来。框外的部分被蒙版挡住，原图保持不变。
4. （可选，第二阶段后半）**色彩校正**：比较选区外一圈环带上"生成图 vs 原图"的均值和方差，对生成图做一次线性颜色匹配，消除整体偏色。
5. **对齐校正**（已实现，`genai_align.rs`）：模型常把整张图平移几个像素。在红色轮廓线外侧 32px 宽的环带上比较亮度
   （去掉均值差），先在长边不超过 1024px 的缩小图上全范围搜索，再在原尺寸上逐像素细化；最多窗口长边的 2%（4–24px）。
   平移量不会大到让蒙版下面露出空隙。
6. **结果校验**（已实现）：在同一环带上计算返回图与原图的归一化互相关（对亮度、对比度变化不敏感）。
   低于 0.35 说明返回的不是这张图（比如一张空白图，或者画了一个不相干的场景）：自动重试一次，
   第二次仍不合格就报错，文档不变。环带太小或原图在环带上是一片纯色（比如天空）时无法判断，直接接受。
   相似度、偏移量、尝试次数都写进命令返回值（`alignment.similarity`、`attempts`），方便以后调阈值。

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
2. 模式 B：上下文窗口、宽高比对齐、蒙版图、提示词模板、贴回羽化、对齐校正、结果校验。
3. 浮动提示栏 + 偏好设置 AI 页 + 首次使用隐私提示。
4. 色彩校正、放大提醒、多候选（一次生成 3 张，可在图层属性里切换）。
