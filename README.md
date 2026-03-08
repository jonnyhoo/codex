# Codex Fork Notes

本仓库是基于 OpenAI `codex` 源码的个人修改版。

## Base

- Upstream: `openai/codex`
- Source version marker: `codex-cli 0.2.1`
- Local validation reference: `codex-cli 0.111.0`
- npm package: `@jonnyhoo/codex`

## Current Changes

### 代码智能

- 内置 `lsp` tool，直接在 `codex-core` 中提供代码智能能力
- 增加 `lsp auto`，让模型按自然语言问题自动选择合适的 LSP 操作
- 增加 LSP provider registry，支持内置 provider 和 JSON 热插拔扩展
- 增加 provider 健康探测与状态查询
- 增加 Java provider 示例：`.codex/lsp-providers/java.json.example`

### 文件编辑

- 新增内置 `write_file` / `edit_file` tools，分别覆盖整文件重写和精确字符串替换场景
- `edit_file` 支持多重 edits、删除式 edit、`replace_all` 和更稳的模糊匹配
- `edit_file` 在模糊匹配替换时会尽量保留目标代码块原始缩进
- `read_file` / `list_dir` 升级为默认工具，减少对 shell 读文件和列目录的依赖

### Patch 与运行时

- `apply_patch` 运行时改为通过临时 patch 文件自调用，避免 Windows 长命令行导致的失败
- 改善沙箱内 `apply_patch` 的执行可靠性

### TUI 集成

- TUI 集成 LSP 状态展示：`/status` 可查看启用语言与健康状态
- TUI 启动后自动做 LSP 健康探测，并给出轻量通知
- TUI 不再忽略 dynamic tool 事件，为后续更深的自动化能力打基础

### Windows 与发布

- 增加原生 Windows 开发包装脚本与说明
- npm 发布链路已改成 fork 友好：默认发布到 `@jonnyhoo/codex`，并支持按本地 vendor 自动收窄平台包

## Validation

- `cargo check -p codex-core --tests`
- `cargo check -p codex-tui`
- 真实 LSP 测试已覆盖：Python、Go、TypeScript

## Native Windows Dev

这个 fork 已经按原生 Windows 开发路径做过一轮收口，但有几个前提要显式说明：

- Rust workspace 根目录在 `codex-rs`，顶层 `justfile` 只是转发到这个 workspace
- `js_repl` 需要 Node 版本满足 `codex-rs/node-version.txt`；如果系统 Node 偏旧，可以单独安装新的 Node，并通过 `js_repl_node_path` 或 `CODEX_JS_REPL_NODE_PATH` 指向它
- `just fmt` / `just fix` 在原生 Windows 上建议通过仓库内脚本 `scripts\windows\just.cmd` 运行
- 这个包装层会调用 Git Bash，但会先剔除 Git/MSYS 自带的 `/usr/bin/link.exe`，避免它抢在 MSVC `link.exe` 前面，导致 Rust 链接阶段失败

推荐流程：

```powershell
cd E:\VIBE_CODING_WORK\codex
cmd /c "\"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat\" && scripts\windows\just.cmd fmt"
cmd /c "\"C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat\" && scripts\windows\just.cmd fix -p codex-core"
```

工具安装建议：

- `just`: 可以直接 `scoop install just`
- `cargo-nextest`: Windows 上更建议直接下载官方 release 二进制放到 `~/.cargo/bin`，比 `cargo install` 更省 CPU
- Git Bash 路径默认使用 `C:\Program Files\Git\bin\bash.exe`；如果你的安装位置不同，先设置 `CODEX_GIT_BASH`

## Continue Here

后续修改请直接在这里追加：

- 日期：
- 基于版本：
- 修改内容：
- 验证结果：
