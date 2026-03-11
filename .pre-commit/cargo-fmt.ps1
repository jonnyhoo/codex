$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location (Join-Path $repoRoot "codex-rs")

cargo fmt --all -- --check
exit $LASTEXITCODE
