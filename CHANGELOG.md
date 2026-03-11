# Changelog

## v0.3.0

This fork rebases onto upstream `rust-v0.114.0` and carries the fork-specific tooling and packaging layer forward under a fresh fork release number.

### Highlights

- Rust workspace and npm package version bumped to `0.3.0`.
- Fork npm publishing remains aligned to `@jonnyhoo/codex`.
- Built-in LSP automation, first-class `write_file` / `edit_file`, and TUI provider visibility were ported onto the upstream base.
- Broad repo search defaults remain tightened through `grep_files`, with generated and dependency directories skipped unless explicitly requested.
- Native Windows development keeps the wrapper-based `just` workflow that avoids Git/MSYS `link.exe` shadowing the MSVC linker.

### Validation

- `cargo test -p codex-core run_search_`
- `cargo test -p codex-core get_base_instructions_no_user_content`
- `cargo test -p codex-core bundled_models_json_roundtrips`
- `cargo test -p codex-core full_toolset_specs_for_gpt5_codex_unified_exec_web_search`
- `cargo build --release -p codex-cli`
- local npm staging smoke for `@jonnyhoo/codex` using a Windows x64 vendor root

Upstream release history continues on the [releases page](https://github.com/openai/codex/releases).
