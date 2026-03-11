$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location (Join-Path $repoRoot "codex-rs")

cargo clippy -p codex-core --lib -- -D warnings
exit $LASTEXITCODE
