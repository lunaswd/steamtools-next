"""Fail a public source release if tracked files contain user data or private paths.

Only tracked source files are audited by default, so build caches are never
published accidentally. --source-tree checks the same explicit source allowlist
before a new clean repository has been staged. No Steam installation is read.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys

ROOT_FILES = {".gitignore", ".gitattributes", "Cargo.toml", "Cargo.lock", "package.json",
              "package-lock.json", "index.html", "tsconfig.json", "vite.config.ts", "LICENSE",
              "README.md", "RELEASE_README.txt", "RELEASE_BUILD.json"}
ROOT_DIRS = {".github", "crates", "public", "scripts", "src", "src-tauri", "docs"}
BLOCKED_PARTS = {".git", "node_modules", "target", "dist", "output", "validation", "references",
                 "cache", "__pycache__", ".playwright-cli", "release-assets", ".release-stage", "gen"}
BLOCKED_SUFFIXES = {".log", ".bak", ".tmp", ".pdb", ".obj", ".dll", ".exe", ".pyc",
                    ".db", ".sqlite", ".sqlite3", ".acf", ".rpa", ".ypf", ".paz", ".bin",
                    ".lua", ".manifest", ".vdf", ".zip", ".7z", ".rar", ".tar", ".gz"}
MEDIA_SUFFIXES = {".png", ".ico", ".jpg", ".jpeg", ".webp", ".gif", ".mp4", ".mp3", ".wav"}
TOOL_ICON_FILES = {"src-tauri/icons/icon.ico", "src-tauri/icons/icon.png"}
PRIVATE_NAMES = {"loginusers.vdf", "config.vdf", "settings.json", "localconfig.vdf",
                 "appaccesstokens.json", "depotkeys.json", "manifest.json", "steamid.txt"}
TEXT_SUFFIXES = {".rs", ".ts", ".json", ".md", ".py", ".ps1", ".toml", ".yaml", ".yml",
                 ".txt", ".mjs", ".html", ".css", ".svg"}
# These identifiers are protocol constants and explicitly synthetic account
# parser fixtures, not accounts collected from a Steam client.
SYNTHETIC_STEAM_IDS = {str(76561197960265728 + value) for value in (0, 1, 2, 171)}


def allowed_path(relative: str) -> bool:
    path = PurePosixPath(relative)
    parts = path.parts
    boundary = (bool(parts) and not path.is_absolute() and ".." not in parts
            and not any(part in BLOCKED_PARTS for part in parts)
            and not any(part.startswith(".env") or re.fullmatch(r"ssfn\d+", part) for part in parts)
            and path.name.lower() not in PRIVATE_NAMES and path.suffix.lower() not in BLOCKED_SUFFIXES
            and (path.suffix.lower() not in MEDIA_SUFFIXES or relative in TOOL_ICON_FILES))
    if not boundary:
        return False
    if relative in ROOT_FILES:
        return True
    if len(parts) < 2:
        return False
    if parts[0] == "src":
        return path.suffix in {".ts", ".css"} or len(parts) == 3 and parts[1] == "locales" and path.suffix == ".json"
    if parts[0] == "crates":
        return len(parts) >= 3 and parts[1] == "core" and (path.suffix == ".rs" or relative == "crates/core/Cargo.toml")
    if parts[0] == "src-tauri":
        return (relative in {"src-tauri/Cargo.toml", "src-tauri/build.rs", "src-tauri/tauri.conf.json",
                             "src-tauri/icons/icon.svg"} or relative in TOOL_ICON_FILES
                or parts[1] == "src" and path.suffix == ".rs"
                or relative == "src-tauri/capabilities/main.json"
                or parts[1] == "permissions" and path.suffix == ".toml")
    if parts[0] == "scripts":
        return (len(parts) == 2 and path.suffix in {".py", ".mjs"}
                or relative == "scripts/license-overrides/index.json"
                or len(parts) > 2 and parts[1] == "license-overrides"
                and path.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE")))
    if parts[0] == ".github":
        return len(parts) == 3 and parts[1] == "workflows" and path.suffix in {".yml", ".yaml"}
    if parts[0] == "public":
        return relative == "public/icon.svg"
    if parts[0] == "docs":
        return relative == "docs/THIRD-PARTY-NOTICES.md"
    return False


def source_paths(root: Path, tracked: bool) -> list[str]:
    if tracked:
        result = subprocess.run(["git", "-C", str(root), "ls-files", "-z", "--cached"],
                                check=True, capture_output=True).stdout
        paths = [value.decode("utf-8") for value in result.split(b"\0") if value]
        if not paths:
            raise ValueError("No tracked public source files have been staged")
        return paths
    paths = []
    for name in sorted(ROOT_FILES):
        if (root / name).is_file():
            paths.append(name)
    for name in sorted(ROOT_DIRS):
        base = root / name
        if not base.is_dir():
            continue
        for item in base.rglob("*"):
            relative = item.relative_to(root).as_posix()
            if item.is_file() and allowed_path(relative):
                paths.append(relative)
    return sorted(set(paths))


def inspect_source(root: Path, paths: list[str]) -> dict:
    errors, records = [], []
    private_project_terms = ("GAL" + "-CHS", "WinExe" + "Tools")
    token = re.compile(r"\b(?:ghp_|github_pat_)[A-Za-z0-9_]{16,}")
    private_key = re.compile(r"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----")
    private_home = re.compile(r"(?i)[a-z]:[\\/]+Users[\\/]+Administrator\b")
    secret = re.compile(r'''(?i)\b(?:api[_-]?key|access[_-]?token|refresh[_-]?token|password|secret)\s*[:=]\s*['"]([A-Za-z0-9._-]{24,})['"]''')
    steam_id = re.compile(r"(?<!\d)7656119\d{10}(?!\d)")
    game_titles = re.compile("|".join(("影之" + "刃零", "土豆" + "兄弟", "饥荒" + "联机版", "腐" + "蚀")))
    for relative in paths:
        path = root / relative
        if not allowed_path(relative):
            errors.append({"path": relative, "reason": "File outside the public tool-source allowlist"})
            continue
        if path.is_symlink() or not path.is_file():
            errors.append({"path": relative, "reason": "Source must be a regular non-link file"})
            continue
        data = path.read_bytes()
        records.append({"path": relative, "sizeBytes": len(data), "sha256": hashlib.sha256(data).hexdigest()})
        if path.suffix.lower() not in TEXT_SUFFIXES and path.name not in ROOT_FILES:
            continue
        try:
            text = data.decode("utf-8-sig")
        except UnicodeDecodeError:
            errors.append({"path": relative, "reason": "Unexpected non-UTF8 text source"})
            continue
        for line_number, line in enumerate(text.splitlines(), 1):
            reasons = []
            if any(term.casefold() in line.casefold() for term in private_project_terms) or private_home.search(line):
                reasons.append("Personal local workspace/home path")
            if token.search(line) or private_key.search(line):
                reasons.append("Credential material")
            for match in secret.finditer(line):
                value = match[1]
                if not value.startswith(("PRIVATE_FIXTURE", "FIXTURE", "EXAMPLE", "TEST", "SAMPLE", "MOCK", "RAW_")):
                    reasons.append("Review non-fixture hardcoded credential literal")
            if any(match[0] not in SYNTHETIC_STEAM_IDS for match in steam_id.finditer(line)):
                reasons.append("Review non-synthetic hardcoded Steam account identifier")
            if game_titles.search(line):
                reasons.append("Real-title demo data must be replaced by synthetic examples")
            for reason in sorted(set(reasons)):
                # Values are deliberately omitted: failed audits must not print
                # the secret or personal material they caught.
                errors.append({"path": relative, "line": line_number, "reason": reason})
    return {"schemaVersion": 1, "scope": "Public tool source only; no Steam files read",
            "files": records, "errors": errors, "passed": not errors}


def inspect_binary(path: Path, private_paths: list[str]) -> dict:
    """Inspect bytes without executing the binary or printing detected material."""
    data = path.read_bytes()
    errors = []
    terms = ["GAL" + "-CHS", "WinExe" + "Tools"]
    for value in private_paths:
        if value:
            terms.extend((value, value.replace("\\", "/"), value.replace("/", "\\")))
    lowered = data.lower()
    if any(term.encode(encoding).lower() in lowered for term in terms for encoding in ("utf-8", "utf-16le")):
        errors.append("Personal filesystem material retained in the binary")
    if re.search(rb"\b(?:ghp_|github_pat_)[A-Za-z0-9_]{24,}", data):
        errors.append("Credential-shaped token retained in the binary")
    return {"name": path.name, "sizeBytes": len(data), "sha256": hashlib.sha256(data).hexdigest(),
            "errors": errors, "passed": not errors}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--source-tree", action="store_true")
    parser.add_argument("--report", type=Path)
    parser.add_argument("--binary", type=Path, help="Also inspect an already built executable without running it")
    parser.add_argument("--private-path", action="append", default=[], help="Additional local path to reject; values are never printed")
    args = parser.parse_args()
    try:
        result = inspect_source(args.root.resolve(), source_paths(args.root.resolve(), not args.source_tree))
        if args.binary:
            result["binary"] = inspect_binary(args.binary, args.private_path)
            result["passed"] = result["passed"] and result["binary"]["passed"]
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"Public source audit failed: {type(error).__name__}", file=sys.stderr)
        return 1
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    for item in result["errors"]:
        print(f"FAIL {item['path']}:{item.get('line', '')}: {item['reason']}", file=sys.stderr)
    for reason in result.get("binary", {}).get("errors", []):
        print(f"FAIL binary: {reason}", file=sys.stderr)
    print(f"Public source privacy audit: {len(result['files'])} files; {len(result['errors'])} issues")
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
