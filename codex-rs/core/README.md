# codex-core

This crate implements the business logic for Codex. It is designed to be used by the various Codex UIs written in Rust.

## Dependencies

Note that `codex-core` makes some assumptions about certain helper utilities being available in the environment. Currently, this support matrix is:

### macOS

Expects `/usr/bin/sandbox-exec` to be present.

When using the workspace-write sandbox policy, the Seatbelt profile allows
writes under the configured writable roots while keeping `.git` (directory or
pointer file), the resolved `gitdir:` target, and `.codex` read-only.

Network access and filesystem read/write roots are controlled by
`SandboxPolicy`. Seatbelt consumes the resolved policy and enforces it.

Seatbelt also supports macOS permission-profile extensions layered on top of
`SandboxPolicy`:

- no extension profile provided:
  keeps legacy default preferences read access (`user-preference-read`).
- extension profile provided with no `macos_preferences` grant:
  does not add preferences access clauses.
- `macos_preferences = "readonly"`:
  enables cfprefs read clauses and `user-preference-read`.
- `macos_preferences = "readwrite"`:
  includes readonly clauses plus `user-preference-write` and cfprefs shm write
  clauses.
- `macos_automation = true`:
  enables broad Apple Events send permissions.
- `macos_automation = ["com.apple.Notes", ...]`:
  enables Apple Events send only to listed bundle IDs.
- `macos_accessibility = true`:
  enables `com.apple.axserver` mach lookup.
- `macos_calendar = true`:
  enables `com.apple.CalendarAgent` mach lookup.

### Linux

Expects the binary containing `codex-core` to run the equivalent of `codex sandbox linux` (legacy alias: `codex debug landlock`) when `arg0` is `codex-linux-sandbox`. See the `codex-arg0` crate for details.

### All Platforms

Expects the binary containing `codex-core` to simulate the virtual `apply_patch` CLI when `arg1` is `--codex-run-as-apply-patch`, and to support patch-file self invocation when `arg1` is `--codex-run-as-apply-patch-file`. See the `codex-arg0` crate for details.

## Local Changes

### LSP Integration

本 fork 在 `codex-core` 中新增了内置 LSP 能力，目标是让 LLM 自动使用，而不是要求用户理解 LSP 术语。

当前变更包括：

- 新增统一内置 `lsp` tool
- 新增内置 `write_file` / `edit_file` tools，覆盖整文件重写与精确替换
- `read_file` / `list_dir` 改成默认工具，减少对 shell 读本地文件和目录的依赖
- `apply_patch` runtime 改成临时 patch 文件自调用，降低 Windows 长参数失败率
- 新增 `action=auto`，按自然语言问题自动选择最合适的 LSP 操作
- 新增 provider registry，支持内置 provider + JSON 热插拔扩展
- 新增 provider 状态与健康探测接口，供 TUI 和其他前端查询当前语言可用性
- 新增真实 LSP 测试，已覆盖 Python、Go、TypeScript

关键文件：

- `src/lsp.rs`
- `src/tools/handlers/lsp.rs`
- `src/tools/spec.rs`
