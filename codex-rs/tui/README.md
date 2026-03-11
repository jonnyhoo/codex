# codex-tui

`codex-tui` is the terminal interface layer for Codex. It renders the conversation, manages the input area and status line, and surfaces tool calls, approvals, notifications, and runtime state.

## Fork-specific additions

This fork extends the TUI so the built-in LSP workflow is visible instead of being hidden behind background automation.

- Dynamic tool events remain visible in the chat timeline instead of being dropped.
- `/status` includes LSP provider state plus a health summary.
- Session startup kicks off an asynchronous LSP health probe and emits lightweight notifications when providers are unavailable.

Key files:

- `src/chatwidget.rs`
- `src/app.rs`
- `src/app_event.rs`
- `src/status/card.rs`
- `src/slash_command.rs`
