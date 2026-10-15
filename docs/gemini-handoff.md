# Generative Fill 交接说明（2026-10-15）

## 一、给新会话 Claude 的提示词（整段复制粘贴）

```text
我在开发 PhotoCraft 的一个 fork：https://github.com/dahan-99/photocraft
（上游是 storytold/photocraft，纯 Rust 写的开源类 Photoshop 编辑器，MIT/Apache-2.0）。
我正在给它加"框选 → 输入提示词 → 用 Google Gemini 在选区里生成图片"的功能（Edit › Generative Fill…）。
请用中文和我交流。

先读这三个文件再动手：
1. AGENTS.md（项目规则：不许 panic/unwrap、所有功能都是 engine 命令、L0–L6 必须能编 wasm、crate 分层由 cargo xtask layers 检查）
2. docs/gemini-generative-fill.md（这个功能的完整设计，分 4 步）
3. docs/gemini-handoff.md（今天做到哪了、踩过的坑）

当前状态（2026-10-15 晚）：在 feature/gemini-image 分支上，第 1、2 步已实现，所有测试通过，已用真实 key 端到端跑通：
- crates/genai：Gemini 客户端（Interactions API）。模型只接受 response_format.mime_type = image/jpeg。
- crates/engine/src/genai_cmds.rs：edit.generativeFill。默认"参考周围画面"：上传一张图（选区周围的窗口，
  选区外 24px 处画红色轮廓线，红线落在最终被蒙版遮住的地方），结果缩放回窗口、转回文档配置文件、
  对齐校正（genai_align.rs）、环带相似度校验 + 无效时自动重试一次、蒙版扩 4px 羽化 4px 贴回。
  参数 sampleLayer（all / currentAndBelow / current）决定从哪些图层取周围画面。
- crates/ui-egui/src/genfill_ui.rs：对话框（提示词、参考周围画面、取样、模型、尺寸、羽化）+ 偏好设置 › AI Generation。
- 已知问题见 docs/gemini-handoff.md 第三部分（最主要的：生成内容在选区边缘和原图接不上，Gemini 这条路解决不了）。

这次要做的事，按顺序：
1. 确认环境能编译：cargo test -p photocraft-genai，cargo test -p photocraft-engine --lib genai，
   cargo test -p photocraft-ui-egui --lib -- genfill prefs_ui i18n menus dialogs
2. 读 docs/gemini-handoff.md 第三部分的已知问题。
3. 我在家里的 4090 上装好 ComfyUI + FLUX.1 Fill [dev] 后，接一个"本地（ComfyUI）"后端：
   PhotoCraft 通过 HTTP 调本机 / 局域网 / Tailscale 地址上的 ComfyUI，原生传蒙版，做真正的局部重绘。先给我方案确认。
   （色彩校正不是优先项：它解决不了内容在边缘接不上的问题。）

规则：每次改动都要带测试；新的 tl!("…") 界面文字和菜单文字必须在所有 complete_menus 语言的 .tsv 里有翻译，
否则 i18n 测试会失败；git push 之前先问我；不要把 API key 写进任何文件、命令或聊天记录。
```

## 二、今天做了什么（总结）

### 1. 调研与准备
- 确认项目开源：MIT OR Apache-2.0 双许可，可以修改和再发布。`docs/brand/` 里的 ArtCraft 名称和 logo 不开源，不能用作自己的品牌。
- 在 GitHub 上 fork 成 `dahan-99/photocraft`。本地 `origin` 指向 fork，`upstream` 指向原仓库，开发分支是 `feature/gemini-image`。
- 公司电脑装了 Rust 1.99 和 VS Build Tools 2022（C++）。公司网络下 winget 下载 rustup 失败，改用 curl 下载成功。Gemini 接口能直连，不需要代理。

### 2. 设计（docs/gemini-generative-fill.md）
- 流程：框选 → 输入提示词 → 结果放到**新图层**，图层蒙版就是选区，原图像素一点不改。
- 关键发现：Gemini **没有像素蒙版参数**，只能用文字描述要改的区域。因此第 2 步的方案是：上传上下文截图 + 选区盖洋红色的标记图 + 提示词模板，回来的图在本地缩放、加蒙版贴回，保证框外绝对不变。
- 分 4 步：① 纯文字生成打通全链路 ② 结合周围场景 ③ 选区下方的浮动提示栏 ④ 色彩校正、放大提醒、多候选。

### 3. 已实现（第 1 步 + 偏好设置）
| 文件 | 内容 |
|---|---|
| `crates/genai/` | 新 crate（登记为 Standalone）：组装请求、解析返回、错误处理、宽高比和尺寸辅助函数；wasm 下返回"不支持" |
| `crates/engine/src/genai_cmds.rs` | `edit.generativeFill`：校验参数 → 按选区选最接近的宽高比 → 后台请求 → 解码、缩放铺满选区外接框 → 新图层 + 选区蒙版 |
| `crates/engine/src/prefs.rs` | 新偏好段 `ai`（model / apiKey / imageSize），Debug 输出里隐藏 key |
| `crates/ui-egui/src/genfill_ui.rs` | Generative Fill 对话框；偏好设置的 AI Generation 页（模型下拉、尺寸下拉、带"显示"开关的 key 输入框） |
| `menus.rs` / `dialogs.rs` / `prefs_ui.rs` | 菜单项挂在 Edit › Content-Aware Fill 后面，接上对话框和偏好设置页 |
| `i18n/*.tsv` | 20 种语言的菜单、偏好设置和对话框翻译（简体中文用"创成式填充"） |

- key 的读取顺序：命令参数 `apiKey` → 偏好设置 → 环境变量 `GEMINI_API_KEY`。
- 默认模型：`gemini-nano-banana-2.1`，可选 Pro / Lite / 旧版。
- 测试：genai 11/11，engine genai_cmds 8/8，engine prefs_usage 4/4，ui-egui 227/227 全部通过。

### 4. 踩过的坑（新会话要注意）
- **ui-egui 不能直接引用 `photocraft_genai`**：它没有这个依赖，要通过 `photocraft_engine::genai_cmds` 转出来，比如 `API_KEY_ENV`。
- **每个 `tl!("…")` 字符串都必须在所有 `complete_menus` 语言里翻译**（`every_tl_literal_is_translated`）；菜单文字和偏好设置标签也一样（`complete_languages_translate_every_menu_string`、`preference_labels_are_translated`）。
- **新偏好必须有代码读取**，否则 `prefs_usage` 测试会失败。
- Gemini 现在用的是 **Interactions API**（`POST /v1beta/interactions`，请求头 `x-goog-api-key`；`input` 里放 text/image 块；`response_format` 里放 `aspect_ratio` 和 `image_size`，K 必须大写；结果在 `steps[].content[]` 中 `type: image` 的块里）。这些都是根据文档写的，**还没用真实 key 验证过**。
- key 以明文保存在 PhotoCraft 的 preferences.json 里。`prefs.set` 不写操作日志，对话框也从不把 key 当参数传。以后可以考虑改存系统凭据管理器（`keyring`）。
- Windows 上 git 提示 "LF will be replaced by CRLF" 是正常的，不用管。
- 第一次完整编译很慢，`target` 目录大约 10GB。

### 5. 下一步
1. 在家用真实 key 端到端跑通，修掉格式不符的地方。
2. 第 2 步：结合周围场景（上下文窗口、宽高比对齐、标记图、提示词模板、羽化贴回）。
3. 第 3 步：选区下方的浮动提示栏、首次使用的隐私提示。
4. 第 4 步：色彩校正、放大过多时提醒、多候选。

## 三、第二次会话（2026-10-15 晚，家里的笔记本）

### 1. 环境
- 这台是家里的笔记本：RTX 3070 Ti Laptop（8GB），Rust 1.97，VS 2019。4090 在家里另一台电脑上。
- 原来只有 Windows SDK 10.0.16299，engine 测试链接失败（winsafe 找不到 Get/SetThreadDpiHostingBehavior）；
  装了 Windows SDK 10.0.26100 后正常。新机器编译前先确认 SDK 版本。
- 还装了 wasm32-unknown-unknown 目标（cargo xtask wasm 要用）。
- 家里 rustfmt 和公司版本不同，对没改过的文件会改换行；只格式化自己改的文件（rustfmt --edition 2024 <文件>）。

### 2. 做了什么（按提交顺序）
| 提交 | 内容 |
|---|---|
| 0aaa093e | 第一次真实调用：模型拒收 image/png 输出（HTTP 400），改为请求 image/jpeg |
| 7e0675dd | 第 2 步：上下文窗口 + 宽高比对齐、sRGB 上传、结果转回文档配置文件（P3 等文档不再偏色）、蒙版扩 4 羽化 4、"参考周围画面"开关（20 种语言） |
| 3c5563f9 | 对齐校正 genai_align.rs：模型重画整张图时常平移几个像素，在选区外环带上搜索平移后再贴 |
| a87f2d8c | 改成单图 + 选区外红色轮廓线；环带相似度校验 + 自动重试一次；取样图层 sampleLayer；删掉已停用的 gemini-2.5-flash-image |

### 3. 已知问题（还没解决）
1. **生成内容在选区边缘和原图接不上（主要问题，Gemini 这条路无解）**：选区边缘出现硬边，
   边缘外的树梢、山体到了选区边上被截断，选区内是另一套树、另一种天空（例：选区上边缘一条横线，
   内侧天空发灰发白、外侧是蓝天）。原因：Gemini 是通用编辑模型，会把整个窗口重画一遍，
   物体位置、天空、曝光都和原图不同，贴回时只保留选区内的像素，边缘两侧本来就是两张不同的画。
   色彩校正、加宽羽化、泊松融合都只能让颜色接近或把硬边变成重影，改不了内容错位。
   解决：换真正的局部重绘模型（本地 FLUX.1 Fill [dev]），它以选区外的真实像素为条件生成，边缘内容能延续。
2. **生成内容不稳定**：同样的提示词，有时效果很好，有时差很多；模型偶尔不照"只改红线内"的要求做。
3. **连续生成会累积**：默认取样"所有图层"会把之前的生成结果当成周围画面。要每次都从原图出发，
   选中原图层并把取样设为"当前图层"。新图层放在当前图层上方（和 Photoshop 一样），可能被更早的生成图层盖住。
4. 放大过度提醒、多候选、首次使用的隐私提示还没做（设计文档第 3、4 步）。

### 4. 试过又放弃的做法（别再走回头路）
- 第二张图用"照片 + 半透明洋红标记"：模型把洋红色叠进选区，连续生成越来越紫。
- 第二张图用黑白蒙版：模型把蒙版的白色叠进选区（白色光晕、大片纯白）。
- 结论：给 Gemini 发第二张图、或在选区内画标记，都有被合成进结果的风险。现在只发一张图，标记画在选区外被遮住的地方。

### 5. 关于换模型（讨论结论）
- 真正的局部重绘模型（FLUX.1 Fill、Stability Inpaint、Vertex Imagen 编辑）原生接受蒙版，只在蒙版内生成，
  不平移、不渗色、接缝自然；但看不到选区里原来的内容，做不了"把船改成红色"这类修改。
- 价格（2026-10-15 官方价）：gemini-nano-banana-2.1 每张 1K $0.034、2K $0.050、4K $0.113；
  FLUX.1 Fill [pro] 约 $0.05/张（第三方数据），常用尺寸下两者差不多。
- 计划：在家里的 4090（24GB）上用 ComfyUI 跑 FLUX.1 Fill [dev]（fp8，约 17GB 模型文件，估计 15–25 秒一张，
  非商业许可，个人使用可以）。PhotoCraft 加"本地（ComfyUI）"后端，通过 HTTP 调用，项目里不加 Python。
  地址只放行本机、局域网私有地址和 Tailscale 地址；ComfyUI 没有登录验证，不能暴露到公网。
