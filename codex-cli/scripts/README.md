# npm releases

Use the staging helper in the repo root to generate npm tarballs for a release. For
example, to stage the CLI, responses proxy, and SDK packages for version `0.6.0`:

```bash
./scripts/stage_npm_packages.py \
  --release-version 0.6.0 \
  --package codex \
  --package codex-responses-api-proxy \
  --package codex-sdk
```

This downloads the native artifacts once, hydrates `vendor/` for each package, and writes
tarballs to `dist/npm/`.

When `--package codex` is provided, the staging helper builds the lightweight
Codex meta package plus all platform-native aliases derived from the base npm
name. In this fork the default is `@jonnyhoo/codex`, so the generated aliases
are `@jonnyhoo/codex-linux-x64`, `@jonnyhoo/codex-win32-x64`, and so on.

If your npm scope is different, override it without editing code:

```bash
./scripts/stage_npm_packages.py \
  --release-version 0.6.0 \
  --package codex \
  --npm-name @your-scope/codex \
  --repository-url git+https://github.com/your-name/codex.git
```

If you need to invoke `build_npm_package.py` directly, run
`codex-cli/scripts/install_native_deps.py` first and pass `--vendor-src` pointing to the
directory that contains the populated `vendor/` tree.

If you only want to publish from a local machine build, you can skip GitHub
artifact download and reuse an existing vendor tree:

```bash
./scripts/stage_npm_packages.py \
  --release-version 0.6.0 \
  --package codex \
  --package codex-win32-x64 \
  --vendor-src C:/path/to/vendor \
  --npm-name @your-scope/codex
```

When `--vendor-src` only contains a subset of targets, the staging helper now
infers those targets automatically and only wires the matching platform aliases
into the `codex` meta package.
