"""Prepare a source-bound, monotonic numeric release on a detached Git commit.

The maintenance branch is never changed or pushed. CI pushes only the resulting
tag after validation; the tagged source includes every version change compiled
into the binaries. A rerun of the same GitHub run reuses its existing tag.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess

MARKER = "RELEASE_BUILD.json"
MAX_PART = 65535


def git(root: Path, *args: str) -> str:
    return subprocess.check_output(["git", "-C", str(root), *args], text=True).strip()


def parse_version(value: str, parts: int) -> tuple[int, ...]:
    if not re.fullmatch(r"\d+(?:\.\d+){" + str(parts - 1) + r"}", value):
        raise ValueError("Release version must have only numeric components")
    result = tuple(int(part) for part in value.split("."))
    if any(part > MAX_PART for part in result):
        raise ValueError("Release version components exceed the PE version limit")
    return result


def increment(version: tuple[int, ...], amount: int = 1) -> tuple[int, ...]:
    if amount < 1:
        raise ValueError("Release increment must be positive")
    values = list(version)
    carry = amount
    for index in range(len(values) - 1, -1, -1):
        carry, values[index] = divmod(values[index] + carry, MAX_PART + 1)
        if not carry:
            return tuple(values)
    raise ValueError("Release version space is exhausted")


def next_version(base: tuple[int, ...], run_number: int, tags: list[str]) -> tuple[int, ...]:
    candidate = increment(base, run_number)
    existing = []
    for tag in tags:
        if not tag.startswith("v"):
            continue
        try:
            existing.append(parse_version(tag.removeprefix("v"), len(base)))
        except ValueError:
            continue
    if existing:
        candidate = max(candidate, increment(max(existing)))
    return candidate


def read_base(root: Path, kind: str) -> tuple[int, ...]:
    if kind == "kernel":
        source = (root / "src/CMakeLists.txt").read_text(encoding="utf-8")
        match = re.search(r"project\(OpenSteamTool VERSION ([0-9.]+)", source)
        if not match:
            raise ValueError("Kernel source version is absent")
        return parse_version(match.group(1), 4)
    value = json.loads((root / "package.json").read_text(encoding="utf-8"))["version"]
    return parse_version(value, 3)


def replace_once(text: str, pattern: str, replacement: str) -> str:
    result, count = re.subn(pattern, replacement, text, count=1)
    if count != 1:
        raise ValueError(f"Version field was absent: {pattern}")
    return result


def stamp(root: Path, kind: str, version: str) -> list[str]:
    if kind == "kernel":
        path = root / "src/CMakeLists.txt"
        text = replace_once(path.read_text(encoding="utf-8"),
                            r"(project\(OpenSteamTool VERSION )[0-9.]+", rf"\g<1>{version}")
        path.write_text(text, encoding="utf-8")
        return ["src/CMakeLists.txt"]
    paths = ["Cargo.toml", "Cargo.lock", "package.json", "package-lock.json", "src-tauri/tauri.conf.json"]
    path = root / "Cargo.toml"
    text = path.read_text(encoding="utf-8")
    prefix, separator, section = text.partition("[workspace.package]")
    if not separator:
        raise ValueError("Cargo workspace version section is absent")
    section = replace_once(section, r'(?m)^version\s*=\s*"[^"]+"', f'version = "{version}"')
    path.write_text(prefix + separator + section, encoding="utf-8")
    path = root / "Cargo.lock"
    text = path.read_text(encoding="utf-8")
    for package in ("steam-tools-core", "steam-tools-next"):
        text = replace_once(text, rf'(name = "{package}"\nversion = ")[^"]+"', rf'\g<1>{version}"')
    path.write_text(text, encoding="utf-8")
    for relative in ("package.json", "package-lock.json"):
        path = root / relative
        value = json.loads(path.read_text(encoding="utf-8"))
        value["version"] = version
        if relative == "package-lock.json" and "" in value.get("packages", {}):
            value["packages"][""]["version"] = version
        path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    path = root / "src-tauri/tauri.conf.json"
    value = json.loads(path.read_text(encoding="utf-8"))
    value["version"] = version
    for window in value.get("app", {}).get("windows", []):
        if window.get("label") == "main":
            window["title"] = f"SteamTools Next v{version}"
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    # Browser demo version text follows the same release; no behavioral edits.
    for relative in ("src/api.ts", "src/main.ts"):
        path = root / relative
        if not path.exists():
            continue
        text = re.sub(r"(?<=version:')[0-9]+\.[0-9]+\.[0-9]+(?=')", version,
                      path.read_text(encoding="utf-8"))
        path.write_text(text, encoding="utf-8")
        paths.append(relative)
    usage = root / "RELEASE_README.txt"
    if usage.is_file():
        text = re.sub(r"(?<=SteamTools Next v)\d+\.\d+\.\d+", version,
                      usage.read_text(encoding="utf-8"))
        usage.write_text(text, encoding="utf-8")
        paths.append("RELEASE_README.txt")
    return paths


def prepare(root: Path, kind: str, run_number: int, run_id: str) -> dict:
    root = root.resolve()
    if not run_id.isdecimal() or len(run_id) > 32 or run_number < 1:
        raise ValueError("GitHub run identity is invalid")
    if git(root, "status", "--porcelain", "--untracked-files=normal"):
        raise ValueError("Version preparation requires a clean source checkout")
    base_commit = git(root, "rev-parse", "HEAD")
    tags = git(root, "tag", "--list").splitlines()
    for tag in tags:
        if not re.fullmatch(r"v\d+(?:\.\d+){2,3}", tag):
            continue
        result = subprocess.run(["git", "-C", str(root), "show", f"{tag}:{MARKER}"],
                                capture_output=True, text=True)
        if result.returncode:
            continue
        try:
            marker = json.loads(result.stdout)
        except ValueError:
            continue
        if marker.get("kind") == kind and marker.get("runId") == run_id:
            if marker.get("baseSourceCommit") != base_commit:
                raise ValueError("Existing release run is bound to a different source commit")
            git(root, "checkout", "--detach", tag)
            return {"version": marker["version"], "releaseTag": tag,
                    "sourceCommit": git(root, "rev-parse", "HEAD"), "baseSourceCommit": base_commit,
                    "reused": True}
    version = ".".join(map(str, next_version(read_base(root, kind), run_number, tags)))
    git(root, "checkout", "--detach", base_commit)
    changed = stamp(root, kind, version)
    marker = {"schemaVersion": 1, "kind": kind, "version": version,
              "baseSourceCommit": base_commit, "runNumber": run_number, "runId": run_id}
    (root / MARKER).write_text(json.dumps(marker, indent=2) + "\n", encoding="utf-8")
    git(root, "add", "--", *changed, MARKER)
    git(root, "-c", "user.name=github-actions[bot]", "-c",
        "user.email=41898282+github-actions[bot]@users.noreply.github.com", "commit", "-m",
        f"Prepare {kind} release {version} [skip ci]")
    tag = "v" + version
    git(root, "tag", tag)
    return {"version": version, "releaseTag": tag, "sourceCommit": git(root, "rev-parse", "HEAD"),
            "baseSourceCommit": base_commit, "reused": False}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kind", choices=("kernel", "tool"), required=True)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--run-number", type=int, default=int(os.environ.get("GITHUB_RUN_NUMBER", "0")))
    parser.add_argument("--run-id", default=os.environ.get("GITHUB_RUN_ID", ""))
    args = parser.parse_args()
    result = prepare(args.root, args.kind, args.run_number, args.run_id)
    if output := os.environ.get("GITHUB_OUTPUT"):
        with open(output, "a", encoding="utf-8") as stream:
            for key, value in result.items():
                stream.write(f"{key}={str(value).lower() if isinstance(value, bool) else value}\n")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
