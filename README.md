# Codex Fork Notes

本仓库是基于 OpenAI `codex` 源码的个人修改版。

## Base

- Upstream: `openai/codex`
- Source version marker: `codex-cli 0.0.0-dev`
- Local validation reference: `codex-cli 0.111.0`
- npm package: `@jonnyhoo/codex`

## Current Changes

- 内置 `lsp` tool，直接在 `codex-core` 中提供代码智能能力
- 增加 `lsp auto`，让 LLM 按自然语言问题自动选择合适的 LSP 操作
- 增加 LSP provider registry，支持内置 provider + JSON 热插拔扩展
- 增加 provider 健康探测与状态查询
- 增加 Java provider 示例：`.codex/lsp-providers/java.json.example`
- TUI 集成 LSP 状态展示：`/status` 可查看启用语言与健康状态
- TUI 启动后自动做 LSP 健康探测，并给出轻量通知
- TUI 不再忽略 dynamic tool 事件，为后续更深的自动化能力打基础
- npm 发布链路已改成 fork 友好：默认发布到 `@jonnyhoo/codex`，并支持按本地 vendor 自动收窄平台包

## Validation

- `cargo check -p codex-core --tests`
- `cargo check -p codex-tui`
- 真实 LSP 测试已覆盖：Python、Go、TypeScript

## Continue Here

后续修改请直接在这里追加：

- 日期：
- 基于版本：
- 修改内容：
- 验证结果：
