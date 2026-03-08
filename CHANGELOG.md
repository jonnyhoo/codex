# Changelog

## v0.2.1

这个版本不继续扩大 LSP 常驻负担，而是针对 Rust 语言服务在 CLI 冷启动阶段最容易出现的“空结果”问题做收敛修复，重点是让现有 built-in LSP 更稳、更像 IDE，而不是更重。

### Rust LSP Reliability

- 当 Rust `hover` 或 `workspace_symbols` 在冷启动时返回空结果，增加有界重试，只在观察到 `$/progress` 后短暂等待，不做无上限阻塞
- `workspace_symbols` 在 Rust 空结果时，允许回退到当前文件的 `document_symbols` 做轻量匹配
- `hover` 在 Rust 空结果时，允许从当前文件最深层的 `document_symbol` 回退生成最小可用信息
- 回退逻辑只在 Rust 空结果场景触发，不扩大其他语言和正常请求的常驻开销

### Workspace Settings Compatibility

- 支持向上查找祖先目录的 `.vscode/settings.json`
- 支持展开 `${workspaceFolder}`、`${workspaceRoot}`、`${workspaceFolderBasename}`
- 修正 Windows 下 `${workspaceFolder}/...` 这类路径变量的分隔符归一化

### Validation

- 新增 Rust LSP 冷启动回退相关单测
- `codex-core` 已通过 `cargo test -p codex-core --lib lsp::tests::`
- `codex-core` 已通过 `cargo clippy -p codex-core --lib -- -D warnings`
- `codex-cli` 已通过常规构建和真实 CLI `workspace_symbols` / `hover` 冒烟验证

## v0.2.0

这个版本在 `v0.1.0` 的基础上，继续把本地代码智能从“一次性 LSP 请求”推进到“会话级、受控复用”的实现，同时把资源边界收紧，避免为了代码智能把 CLI 常驻负担越堆越高。

### Persistent LSP Sessions

- `lsp` tool 改为支持 session-scoped 的持久 LSP session
- 按 `(workspace_root, provider)` 复用 language server，而不是每次请求都完整重启
- 增加空闲回收、容量上限、坏 session 丢弃和 session shutdown 清理
- 当持久 session 池达到上限且都在忙时，自动退回一次性 transient session，避免并发尖峰把常驻 server 越堆越多

### Workspace Bridging

- 增加 `didOpen` / `didChange` / `didClose` 文档同步
- `workspace/workspaceFolders` 改为返回真实 workspace
- `workspace/configuration` 改为返回真实配置，而不是一组 `null`
- 支持读取 `.vscode/settings.json`，并兼容 JSONC 注释、尾逗号和 dotted key 归一化

### Validation

- 新增和补强 LSP 配置映射相关测试
- `codex-core` 已通过 `fmt`、`clippy -D warnings`、`lsp::tests`
- `codex-cli` 依赖链已完成常规构建验证

## v0.1.0

这个版本是基于 `openai/codex` 的 fork 首个正式发布版本，重点不在界面变化，而在本地代码智能、文件编辑能力、Windows 运行稳定性，以及 fork 自身的发布链路。

### Built-in LSP

- 在 `codex-core` 中内置 `lsp` tool
- 支持 `definition`、`references`、`hover`、`document_symbols`、`workspace_symbols`、`diagnostics`、`rename`、`completion`、`signature_help`、`code_actions`
- 新增 `lsp auto`，允许模型根据自然语言问题自动选择合适的 LSP 动作
- 增加 provider registry，支持内置 provider 和 JSON 插件式 provider
- 增加 provider 状态探测与查询
- 增加 Java provider 示例：`.codex/lsp-providers/java.json.example`

### File Tools

- 新增内置 `write_file`，支持整文件创建和覆盖
- 新增内置 `edit_file`，支持结构化、可组合的字符串替换
- `edit_file` 支持多重 edits、`replace_all`、删除式 edit，以及更稳的模糊匹配
- `edit_file` 的模糊替换会尽量保留目标代码块原有缩进
- `read_file` / `list_dir` 升级为默认工具，减少模型对 shell 读文件、列目录的依赖

### apply_patch / Runtime

- `apply_patch` 运行时改为通过临时 patch 文件执行，而不是把 patch 直接塞进超长命令行
- 改善 Windows 下长命令行场景的稳定性
- 改善沙箱内 `apply_patch` 的执行可靠性

### TUI Integration

- TUI 集成 LSP provider 状态展示
- `/status` 可查看启用语言与 LSP 健康状态
- 启动后会自动做轻量 LSP 健康探测
- TUI 不再忽略 dynamic tool 事件，为后续更深的自动化能力打基础

### Native Windows Support

- 增加原生 Windows 开发说明
- 增加 `scripts/windows/just.cmd`
- 增加 `scripts/windows/bash-safe.cmd`
- 规避 Git/MSYS `link.exe` 抢占 MSVC `link.exe` 的问题，改善原生 Windows 编译体验

### Fork-friendly Release Flow

- npm 默认发布到 `@jonnyhoo/codex`
- 支持按本地 `vendor` 自动收窄平台包
- 支持使用本地 vendor 组装发布 tarball，适合 fork 独立发版
