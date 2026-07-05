#!/usr/bin/env python3

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:
    tomllib = None


SEMVER_PREFIX_RE = re.compile(r"^(\d+\.\d+\.\d+)(?:[-+].*)?$")


@dataclass
class ReleaseTag:
    base_version: str
    release_iteration: int
    release_tag: str
    reused_tag_at_head: bool


def run_git_tag(*args: str) -> list[str]:
    output = subprocess.check_output(["git", "tag", *args], text=True)
    return [line.strip() for line in output.splitlines() if line.strip()]


def parse_base_version(raw_version: str) -> str:
    match = SEMVER_PREFIX_RE.match(raw_version.strip())
    if not match:
        raise ValueError(
            f'Unsupported version "{raw_version}". Expected something like "1.5.0".'
        )
    return match.group(1)


def semver_key(version: str) -> tuple[int, int, int]:
    major, minor, patch = version.split(".")
    return int(major), int(minor), int(patch)


def compatible_prefixes(prefix: str) -> list[str]:
    variants = [prefix]
    if len(prefix) == 1 and prefix.isalpha():
        swapped = prefix.swapcase()
        if swapped != prefix:
            variants.append(swapped)
    return variants


def build_prefix_pattern(prefix: str) -> str:
    options = "|".join(re.escape(value) for value in compatible_prefixes(prefix))
    return f"(?:{options})"


def get_base_version_from_tags(all_tags: list[str], base_prefix: str) -> str | None:
    prefix_pattern = build_prefix_pattern(base_prefix)
    base_re = re.compile(rf"^{prefix_pattern}(\d+\.\d+\.\d+)$")
    versions = [match.group(1) for tag in all_tags if (match := base_re.fullmatch(tag))]
    if not versions:
        return None
    return sorted(versions, key=semver_key)[-1]


def get_release_iteration(
    tag: str, base_version: str, release_prefix: str, release_suffix: str
) -> int | None:
    prefix_pattern = build_prefix_pattern(release_prefix)
    suffix_pattern = re.escape(release_suffix)
    release_re = re.compile(
        rf"^{prefix_pattern}{re.escape(base_version)}-{suffix_pattern}\.(\d+)$"
    )
    match = release_re.fullmatch(tag)
    return int(match.group(1)) if match else None


def compute_release_tag(
    base_version: str,
    all_tags: list[str],
    head_tags: list[str],
    release_prefix: str,
    release_suffix: str,
) -> ReleaseTag:
    all_iterations = [
        value
        for value in (
            get_release_iteration(tag, base_version, release_prefix, release_suffix)
            for tag in all_tags
        )
        if value is not None
    ]
    max_iteration = max(all_iterations) if all_iterations else 0

    head_iterations = [
        (tag, value)
        for tag in head_tags
        for value in [
            get_release_iteration(tag, base_version, release_prefix, release_suffix)
        ]
        if value is not None
    ]
    if head_iterations:
        reused_tag, reused_iteration = sorted(
            head_iterations, key=lambda item: item[1], reverse=True
        )[0]
        return ReleaseTag(
            base_version=base_version,
            release_iteration=reused_iteration,
            release_tag=reused_tag,
            reused_tag_at_head=True,
        )

    next_iteration = max_iteration + 1
    return ReleaseTag(
        base_version=base_version,
        release_iteration=next_iteration,
        release_tag=f"{release_prefix}{base_version}-{release_suffix}.{next_iteration}",
        reused_tag_at_head=False,
    )


def write_github_output(result: ReleaseTag) -> None:
    output_path = os.getenv("GITHUB_OUTPUT")
    if not output_path:
        return

    with open(output_path, "a", encoding="utf-8") as output_file:
        output_file.write(f"base_version={result.base_version}\n")
        output_file.write(f"release_iteration={result.release_iteration}\n")
        output_file.write(f"release_tag={result.release_tag}\n")
        output_file.write(f"reused_tag_at_head={str(result.reused_tag_at_head).lower()}\n")


def read_package_version(repo_root: Path) -> str | None:
    package_path = repo_root / "package.json"
    if not package_path.exists():
        return None
    package_json = json.loads(package_path.read_text(encoding="utf-8"))
    version = package_json.get("version")
    if not isinstance(version, str):
        raise ValueError("package.json version must be a string")
    return version.strip()


def read_cargo_workspace_version(repo_root: Path) -> str | None:
    cargo_path = repo_root / "Cargo.toml"
    if not cargo_path.exists():
        return None
    if tomllib is None:
        return None

    cargo = tomllib.loads(cargo_path.read_text(encoding="utf-8"))
    workspace = cargo.get("workspace")
    if isinstance(workspace, dict):
        workspace_package = workspace.get("package")
        if isinstance(workspace_package, dict):
            version = workspace_package.get("version")
            if isinstance(version, str):
                return version.strip()

    package = cargo.get("package")
    if isinstance(package, dict):
        version = package.get("version")
        if isinstance(version, str):
            return version.strip()

    return None


def main() -> int:
    release_prefix = (os.getenv("RELEASE_TAG_PREFIX") or "v").strip()
    base_prefix = (os.getenv("BASE_TAG_PREFIX") or release_prefix).strip()
    release_suffix = (os.getenv("RELEASE_TAG_SUFFIX") or "mindroom").strip()

    if not release_prefix:
        raise ValueError("RELEASE_TAG_PREFIX must not be empty")
    if not release_suffix:
        raise ValueError("RELEASE_TAG_SUFFIX must not be empty")

    repo_root = Path(__file__).resolve().parent.parent
    all_tags = run_git_tag("--list")
    head_tags = run_git_tag("--points-at", "HEAD")
    raw_version = os.getenv("BASE_VERSION")
    if raw_version:
        base_version = parse_base_version(raw_version)
    else:
        package_version = read_package_version(repo_root)
        if package_version:
            base_version = parse_base_version(package_version)
        else:
            cargo_version = read_cargo_workspace_version(repo_root)
            if cargo_version:
                base_version = parse_base_version(cargo_version)
            else:
                base_version = get_base_version_from_tags(all_tags, base_prefix)
                if not base_version:
                    raise ValueError(
                        "Could not auto-detect base version. Set BASE_VERSION or provide package.json/Cargo.toml version."
                    )

    result = compute_release_tag(
        base_version,
        all_tags,
        head_tags,
        release_prefix,
        release_suffix,
    )
    write_github_output(result)

    print(f"base_version={result.base_version}")
    print(f"release_iteration={result.release_iteration}")
    print(f"release_tag={result.release_tag}")
    print(f"reused_tag_at_head={str(result.reused_tag_at_head).lower()}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(1) from exc
