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

When publishing manually from `dist/npm/`, publish platform tarballs first and
the `codex-npm-<version>.tgz` meta package last so the optional dependency
targets already exist in the registry.

For the forked `@jonnyhoo/codex` CLI, the package version does not match upstream
`rust-v<version>` tags. Use the convenience wrapper from `codex-cli/` and pass
either `--vendor-src` or `--workflow-url` explicitly:

```bash
pnpm --dir codex-cli stage-release -- --vendor-src /path/to/vendor
```

When `--package codex` is provided, the staging helper builds the lightweight
`@openai/codex` meta package plus all platform-native `@openai/codex` variants
that are later published under platform-specific dist-tags.

If you need to invoke `build_npm_package.py` directly, run
`codex-cli/scripts/install_native_deps.py` first and pass `--vendor-src` pointing to the
directory that contains the populated `vendor/` tree.
