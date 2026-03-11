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

## Fork-specific additions

This fork extends `codex-core` with local-code editing and language-aware tooling that is meant to be used directly by the model, not exposed as a separate setup burden for the user.

- Built-in `lsp` tool support with provider registry, session-scoped persistence, workspace file watching, and health/status reporting.
- Built-in `write_file` and `edit_file` function tools for full-file rewrites and exact text replacement.
- `apply_patch` integration hardened to preserve LSP/workspace synchronization and to avoid Windows command-line length failures by using patch-file self invocation when sandboxed.
- Broader repo search behavior steers the model toward `grep_files`, with generated and dependency directories skipped by default unless explicitly requested.

Key fork-specific files:

- `src/lsp.rs`
- `src/tools/handlers/lsp.rs`
- `src/tools/handlers/write_file.rs`
- `src/tools/handlers/edit_file.rs`
- `src/tools/handlers/file_change.rs`
- `src/tools/spec.rs`
