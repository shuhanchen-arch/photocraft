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

当前状态：在 feature/gemini-image 分支上，第 1 步已完成并全部测试通过：
- crates/genai：独立的 Gemini 客户端（Interactions API，ureq + rustls）
- crates/engine/src/genai_cmds.rs：命令 edit.generativeFill（纯文字生成，结果铺满选区外接框，选区作图层蒙版，后台任务，一次撤销）
- crates/ui-egui/src/genfill_ui.rs：提示词对话框 + 偏好设置 › AI Generation 页（模型、输出尺寸、API key）
- 还没做过真实的 Gemini 端到端调用。

今天要做的事，按顺序：
1. 确认环境能编译：cargo test -p photocraft-genai，cargo test -p photocraft-engine --lib genai，
   cargo test -p photocraft-ui-egui --lib -- genfill prefs_ui i18n menus dialogs
2. 帮我跑起 app（cargo run -p photocraft），我会在 编辑 › 首选项 › AI Generation 里自己填 key，
   然后框选、Edit › Generative Fill… 试生成。根据真实返回修 bug（尤其是 genai 里的请求/返回 JSON 格式、模型名，
   如果和 Google 当前文档不符，以官方文档 https://ai.google.dev/gemini-api/docs/image-generation 为准）。
3. 端到端跑通后，开始设计文档里的第 2 步"结合场景"：把选区四周的上下文截图 + 洋红色标记图 + 提示词一起上传，
   结果缩放回窗口、用选区蒙版（扩 4px 羽化 4px）贴回。先给我方案确认再写代码。

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
