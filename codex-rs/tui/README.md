# codex-tui

`codex-tui` 是 Codex 的终端交互界面层，负责：

- 渲染聊天与历史记录
- 管理底部输入区和状态栏
- 处理 slash commands
- 展示工具调用、审批、通知和运行状态

## Structure

当前与本 fork 改动最相关的部分：

- `src/chatwidget.rs`：主聊天视图与事件处理入口
- `src/chatwidget/interrupts.rs`：延迟事件队列
- `src/history_cell.rs`：历史单元格渲染
- `src/status/card.rs`：`/status` 卡片输出
- `src/slash_command.rs`：slash command 描述与入口

## Local Changes

本 fork 在 `tui` 层新增了 LSP 相关可见性与自动化支撑：

- 不再忽略 dynamic tool 事件，已可在 TUI 中显示 dynamic tool 调用过程
- `/status` 已集成 LSP provider 状态
- session 初始化后会异步探测 LSP 健康状态，并通过轻量通知提示用户
- 设计目标是：用户无须理解 LSP，只需知道哪些语言已启用、是否正常
