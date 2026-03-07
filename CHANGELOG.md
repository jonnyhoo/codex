# Changelog

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
