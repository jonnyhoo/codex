#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const scriptDir = path.dirname(__filename);
const codexCliRoot = path.dirname(scriptDir);
const repoRoot = path.dirname(codexCliRoot);
const stageScript = path.join(repoRoot, "scripts", "stage_npm_packages.py");

const packageJson = JSON.parse(
  readFileSync(path.join(codexCliRoot, "package.json"), "utf8"),
);
const releaseVersion = packageJson.version;
const packageName = packageJson.name ?? "@openai/codex";
const args = process.argv.slice(2);

function fail(message) {
  console.error(`stage-release: ${message}`);
  process.exit(1);
}

function hasFlag(flag) {
  return args.some((arg) => arg === flag || arg.startsWith(`${flag}=`));
}

function findPython() {
  const candidates =
    process.platform === "win32"
      ? [
          { command: "py", prefix: ["-3"] },
          { command: "python", prefix: [] },
          { command: "python3", prefix: [] },
        ]
      : [
          { command: "python3", prefix: [] },
          { command: "python", prefix: [] },
        ];

  for (const candidate of candidates) {
    const probe = spawnSync(
      candidate.command,
      [...candidate.prefix, "--version"],
      { stdio: "ignore" },
    );
    if (probe.status === 0) {
      return candidate;
    }
  }

  fail("Python 3 is required to run scripts/stage_npm_packages.py.");
}

if (hasFlag("--package")) {
  fail("do not pass --package; this wrapper always stages the `codex` npm package set.");
}

if (hasFlag("--release-version")) {
  fail(
    "do not pass --release-version; this wrapper uses codex-cli/package.json version.",
  );
}

const wantsHelp = hasFlag("--help") || hasFlag("-h");
if (
  !wantsHelp &&
  packageName !== "@openai/codex" &&
  !hasFlag("--vendor-src") &&
  !hasFlag("--workflow-url")
) {
  fail(
    "fork releases must pass --vendor-src or --workflow-url. Automatic upstream workflow lookup only works when the npm version matches upstream `rust-v<version>` tags.",
  );
}

const python = findPython();
const result = spawnSync(
  python.command,
  [
    ...python.prefix,
    stageScript,
    "--package",
    "codex",
    "--release-version",
    releaseVersion,
    ...args,
  ],
  {
    cwd: repoRoot,
    stdio: "inherit",
  },
);

if (result.error) {
  fail(`failed to launch the staging helper: ${result.error.message}`);
}

process.exit(result.status ?? 1);
