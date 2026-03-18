# Changelog

## v0.3.2

This fork repackages the Windows npm release so the published npm version and
embedded native CLI version stay aligned.

### Highlights

- Rust workspace and npm package version bumped to `0.3.2`.
- Windows npm packages now bundle fork-built `codex.exe`, `codex-command-runner.exe`, and `codex-windows-sandbox-setup.exe` instead of upstream `rust-v0.114.0` binaries.
- `codex --version` now reports the same fork release version as the installed npm package.

### Validation

- `cargo build --release -p codex-cli --bin codex`
- `cargo build --release -p codex-windows-sandbox --bin codex-command-runner --bin codex-windows-sandbox-setup`
- local npm staging smoke for `@jonnyhoo/codex` `0.3.2` using locally built Windows x64 release binaries

## v0.3.1

This fork keeps the upstream `rust-v0.114.0` rebase while tightening the fork's
npm release path for the next publish.

### Highlights

- Rust workspace and npm package version bumped to `0.3.1`.
- `codex-cli` now exposes a working `pnpm stage-release` wrapper for this fork.
- Release docs now match the real tarball-based flow and publish platform tarballs before the meta package.
- The fork staging path now requires explicit native artifact input (`--vendor-src` or `--workflow-url`) instead of assuming upstream tag naming.

### Validation

- `pnpm --dir codex-cli stage-release --help`
- local npm staging smoke for `@jonnyhoo/codex` `0.3.1` using upstream `rust-v0.114.0` Windows x64 release assets

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
