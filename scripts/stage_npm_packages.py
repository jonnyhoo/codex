#!/usr/bin/env python3
"""Stage one or more Codex npm packages for release."""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
BUILD_SCRIPT = REPO_ROOT / "codex-cli" / "scripts" / "build_npm_package.py"
INSTALL_NATIVE_DEPS = REPO_ROOT / "codex-cli" / "scripts" / "install_native_deps.py"
WORKFLOW_NAME = ".github/workflows/rust-release.yml"
GITHUB_REPO = "openai/codex"

_SPEC = importlib.util.spec_from_file_location("codex_build_npm_package", BUILD_SCRIPT)
if _SPEC is None or _SPEC.loader is None:
    raise RuntimeError(f"Unable to load module from {BUILD_SCRIPT}")
_BUILD_MODULE = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(_BUILD_MODULE)
PACKAGE_NATIVE_COMPONENTS = getattr(_BUILD_MODULE, "PACKAGE_NATIVE_COMPONENTS", {})
PACKAGE_EXPANSIONS = getattr(_BUILD_MODULE, "PACKAGE_EXPANSIONS", {})
CODEX_PLATFORM_PACKAGES = getattr(_BUILD_MODULE, "CODEX_PLATFORM_PACKAGES", {})


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--release-version",
        required=True,
        help="Version to stage (e.g. 0.1.0 or 0.1.0-alpha.1).",
    )
    parser.add_argument(
        "--package",
        dest="packages",
        action="append",
        required=True,
        help="Package name to stage. May be provided multiple times.",
    )
    parser.add_argument(
        "--workflow-url",
        help="Optional workflow URL to reuse for native artifacts.",
    )
    parser.add_argument(
        "--vendor-src",
        type=Path,
        help="Use a prebuilt vendor root instead of downloading native release artifacts.",
    )
    parser.add_argument(
        "--npm-name",
        help="Override the base npm package name passed to the staging helper.",
    )
    parser.add_argument(
        "--repository-url",
        help="Override the repository URL written into staged package manifests.",
    )
    parser.add_argument(
        "--platform-package",
        action="append",
        choices=tuple(CODEX_PLATFORM_PACKAGES),
        help=(
            "Limit the `codex` meta package to selected platform package(s). "
            "When omitted with `--vendor-src`, the script infers available platforms from the vendor tree."
        ),
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=None,
        help="Directory where npm tarballs should be written (default: dist/npm).",
    )
    parser.add_argument(
        "--keep-staging-dirs",
        action="store_true",
        help="Retain temporary staging directories instead of deleting them.",
    )
    return parser.parse_args()


def collect_native_components(packages: list[str]) -> set[str]:
    components: set[str] = set()
    for package in packages:
        components.update(PACKAGE_NATIVE_COMPONENTS.get(package, []))
    return components


def infer_platform_packages_from_vendor(vendor_src: Path) -> list[str]:
    package_by_target = {
        package_config["target_triple"]: package_name
        for package_name, package_config in CODEX_PLATFORM_PACKAGES.items()
    }
    inferred_packages: list[str] = []
    if not vendor_src.exists():
        return inferred_packages

    for target_dir in vendor_src.iterdir():
        if not target_dir.is_dir():
            continue
        package_name = package_by_target.get(target_dir.name)
        if package_name is not None:
            inferred_packages.append(package_name)

    return inferred_packages


def expand_packages(packages: list[str], platform_packages: list[str] | None = None) -> list[str]:
    expanded: list[str] = []
    for package in packages:
        if package == "codex":
            codex_expansions = ["codex", *(platform_packages or PACKAGE_EXPANSIONS.get(package, []))]
            expansion_candidates = codex_expansions
        else:
            expansion_candidates = PACKAGE_EXPANSIONS.get(package, [package])

        for expanded_package in expansion_candidates:
            if expanded_package in expanded:
                continue
            expanded.append(expanded_package)
    return expanded


def resolve_release_workflow(version: str) -> dict:
    stdout = subprocess.check_output(
        [
            "gh",
            "run",
            "list",
            "--branch",
            f"rust-v{version}",
            "--json",
            "workflowName,url,headSha",
            "--workflow",
            WORKFLOW_NAME,
            "--jq",
            "first(.[])",
        ],
        cwd=REPO_ROOT,
        text=True,
    )
    workflow = json.loads(stdout or "null")
    if not workflow:
        raise RuntimeError(f"Unable to find rust-release workflow for version {version}.")
    return workflow


def resolve_workflow_url(version: str, override: str | None) -> tuple[str, str | None]:
    if override:
        return override, None

    workflow = resolve_release_workflow(version)
    return workflow["url"], workflow.get("headSha")


def install_native_components(
    workflow_url: str,
    components: set[str],
    vendor_root: Path,
) -> None:
    if not components:
        return

    cmd = python_script_command(INSTALL_NATIVE_DEPS)
    cmd.extend(["--workflow-url", workflow_url])
    for component in sorted(components):
        cmd.extend(["--component", component])
    cmd.append(str(vendor_root))
    run_command(cmd)


def python_script_command(script_path: Path) -> list[str]:
    return [sys.executable, str(script_path)]


def run_command(cmd: list[str]) -> None:
    print("+", " ".join(cmd))
    subprocess.run(cmd, cwd=REPO_ROOT, check=True)


def tarball_name_for_package(package: str, version: str) -> str:
    if package in CODEX_PLATFORM_PACKAGES:
        platform = package.removeprefix("codex-")
        return f"codex-npm-{platform}-{version}.tgz"
    return f"{package}-npm-{version}.tgz"


def main() -> int:
    args = parse_args()

    output_dir = args.output_dir or (REPO_ROOT / "dist" / "npm")
    output_dir.mkdir(parents=True, exist_ok=True)

    runner_temp = Path(os.environ.get("RUNNER_TEMP", tempfile.gettempdir()))

    vendor_src_override = args.vendor_src.resolve() if args.vendor_src is not None else None
    selected_platform_packages = list(args.platform_package or [])
    if vendor_src_override is not None and not selected_platform_packages:
        selected_platform_packages = infer_platform_packages_from_vendor(vendor_src_override)
    packages = expand_packages(list(args.packages), selected_platform_packages or None)
    native_components = collect_native_components(packages)

    vendor_temp_root: Path | None = None
    vendor_src: Path | None = None
    resolved_head_sha: str | None = None

    final_messages = []

    try:
        if native_components:
            if vendor_src_override is not None:
                vendor_src = vendor_src_override
            else:
                workflow_url, resolved_head_sha = resolve_workflow_url(
                    args.release_version, args.workflow_url
                )
                vendor_temp_root = Path(tempfile.mkdtemp(prefix="npm-native-", dir=runner_temp))
                install_native_components(workflow_url, native_components, vendor_temp_root)
                vendor_src = vendor_temp_root / "vendor"

        if resolved_head_sha:
            print(f"should `git checkout {resolved_head_sha}`")

        for package in packages:
            staging_dir = Path(tempfile.mkdtemp(prefix=f"npm-stage-{package}-", dir=runner_temp))
            pack_output = output_dir / tarball_name_for_package(package, args.release_version)

            cmd = python_script_command(BUILD_SCRIPT)
            cmd.extend(
                [
                    "--package",
                    package,
                    "--release-version",
                    args.release_version,
                    "--staging-dir",
                    str(staging_dir),
                    "--pack-output",
                    str(pack_output),
                ]
            )

            if vendor_src is not None:
                cmd.extend(["--vendor-src", str(vendor_src)])
            if args.npm_name is not None:
                cmd.extend(["--npm-name", args.npm_name])
            if args.repository_url is not None:
                cmd.extend(["--repository-url", args.repository_url])
            if package == "codex":
                for platform_package in selected_platform_packages:
                    cmd.extend(["--platform-package", platform_package])

            try:
                run_command(cmd)
            finally:
                if not args.keep_staging_dirs:
                    shutil.rmtree(staging_dir, ignore_errors=True)

            final_messages.append(f"Staged {package} at {pack_output}")
    finally:
        if vendor_temp_root is not None and not args.keep_staging_dirs:
            shutil.rmtree(vendor_temp_root, ignore_errors=True)

    for msg in final_messages:
        print(msg)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
