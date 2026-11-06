"""Package the independent tool repository, never a surrounding workspace.

The application ZIP has exactly six flat files. Corresponding tracked tool
sources and required cached third-party sources are a separate release asset.
"""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import struct
import subprocess
import tempfile
import zipfile

import collect_licenses
import audit_public_source

FILES = ("SteamTools Next.exe", "LICENSE.txt", "使用说明.txt", "THIRD-PARTY-NOTICES.txt",
         "licenses.txt", "SHA256SUMS.txt")
ROOT_FILES = {".gitignore", ".gitattributes", "Cargo.toml", "Cargo.lock", "package.json",
              "package-lock.json", "index.html", "tsconfig.json", "vite.config.ts", "LICENSE",
              "README.md", "RELEASE_README.txt", "RELEASE_BUILD.json"}
ROOT_DIRS = {".github", "crates", "public", "scripts", "src", "src-tauri", "docs"}
BLOCKED_PARTS = {".git", "node_modules", "target", "dist", "output", "validation", "references",
                 "cache", "__pycache__", ".playwright-cli", "release-assets", ".release-stage"}
BLOCKED_SUFFIXES = {".log", ".bak", ".tmp", ".pdb", ".obj", ".dll", ".exe", ".pyc"}
ZIP_TIME = (2000, 1, 1, 0, 0, 0)


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def record(name: str, data: bytes) -> dict:
    return {"name": name, "sizeBytes": len(data), "sha256": digest(data)}


def json_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, ensure_ascii=False) + "\n").encode("utf-8")


def git(root: Path, *args: str) -> str:
    return subprocess.check_output(["git", "-C", str(root), *args], text=True).strip()


def validate_pe(data: bytes) -> None:
    if len(data) < 256 or data[:2] != b"MZ":
        raise ValueError("Tool executable is not a PE image")
    offset = struct.unpack_from("<I", data, 60)[0]
    if offset + 26 > len(data) or data[offset:offset + 4] != b"PE\0\0":
        raise ValueError("Tool executable has an invalid PE header")
    if struct.unpack_from("<H", data, offset + 4)[0] != 0x8664:
        raise ValueError("Tool executable must be Windows x64")
    if struct.unpack_from("<H", data, offset + 22)[0] & 0x2000:
        raise ValueError("Tool executable must not be a DLL")
    if struct.unpack_from("<H", data, offset + 24)[0] != 0x20b:
        raise ValueError("Tool executable must be PE32+")


def write_zip(path: Path, entries: dict[str, bytes]) -> None:
    if path.exists():
        raise ValueError(f"Release asset already exists: {path.name}")
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for name in sorted(entries):
            relative = PurePosixPath(name)
            if not name or relative.is_absolute() or ".." in relative.parts or "\\" in name:
                raise ValueError("Unsafe release archive entry")
            info = zipfile.ZipInfo(name, ZIP_TIME)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
            archive.writestr(info, entries[name], compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)
    with zipfile.ZipFile(path) as archive:
        if archive.testzip() is not None or set(archive.namelist()) != set(entries):
            raise ValueError("Release archive failed its reopen audit")
        for name, data in entries.items():
            if archive.read(name) != data:
                raise ValueError("Release archive content changed")


def allowed_source_path(name: str) -> bool:
    path = PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts or "\\" in name:
        return False
    if any(part in BLOCKED_PARTS for part in path.parts) or path.suffix.lower() in BLOCKED_SUFFIXES:
        return False
    if not audit_public_source.allowed_path(name):
        return False
    if len(path.parts) == 1:
        return name in ROOT_FILES
    return path.parts[0] in ROOT_DIRS


def source_entries(root: Path, commit: str) -> dict[str, bytes]:
    snapshot = subprocess.check_output(["git", "-C", str(root), "archive", "--format=zip", commit])
    result = {}
    with zipfile.ZipFile(io.BytesIO(snapshot)) as archive:
        for item in archive.infolist():
            if item.is_dir():
                continue
            if not allowed_source_path(item.filename):
                raise ValueError(f"Tracked file is outside the tool-source allowlist: {item.filename}")
            if (item.external_attr >> 16) & 0o170000 == 0o120000:
                raise ValueError("Symlinks are not permitted in tool source releases")
            result[item.filename] = archive.read(item)
    if not audit_public_source.inspect_source(root, list(result))["passed"]:
        raise ValueError("Tracked release source failed its public-data privacy audit")
    return result


def dependency_material(root: Path, build: Path, staging: Path) -> tuple[bytes, dict[str, bytes]]:
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    manifest = collect_licenses.collect(root, cargo_home, staging, build_inputs=build)
    if not manifest["build_input_evidence"]["available"] or manifest["missing_observed_build_or_npm"]:
        raise ValueError("Observed application dependency license material is incomplete")
    text = ["SteamTools Next third-party dependency licenses", ""]
    for item in manifest["cargo"]["packages"] + manifest["npm"]["packages"]:
        text.append(f'{item["ecosystem"]}: {item["name"]} {item["version"]} | {item["license_expression"]}')
        for file in item["license_files"]:
            text.extend(["", f'----- {file["source_relative_path"]} -----',
                         (staging / file["output_relative_path"]).read_text(encoding="utf-8", errors="replace")])
        text.append("")
    sources = {}
    source_records = []
    for package in manifest["third_party_sources"]["packages"]:
        path = staging / package["output_relative_path"]
        name = "third-party-sources/" + path.name
        sources[name] = path.read_bytes()
        source_records.append(record(name, sources[name]))
    sources["third-party-sources/SHA256SUMS.json"] = json_bytes({"schemaVersion": 1, "files": source_records})
    return "\n".join(text).encode("utf-8"), sources


def package(root: Path, output: Path, release_tag: str, build: Path, cargo_licenses: bool = True) -> dict:
    root, output, build = root.resolve(), output.resolve(), build.resolve()
    version = json.loads((root / "package.json").read_text(encoding="utf-8"))["version"]
    if not re.fullmatch(r"\d+\.\d+\.\d+", version) or release_tag != "v" + version:
        raise ValueError("Tool release tag must match the numeric source version")
    if git(root, "status", "--porcelain", "--untracked-files=normal"):
        raise ValueError("Tool release requires committed, clean sources")
    commit = git(root, "rev-parse", "HEAD")
    if json.loads((root / "src-tauri/tauri.conf.json").read_text(encoding="utf-8"))["version"] != version:
        raise ValueError("Tauri and npm versions differ")
    binary = (build / "steam-tools-next.exe").read_bytes()
    validate_pe(binary)
    # Metadata is checked by the Windows workflow before invoking this packager.
    provenance_path = build / "tool-build.json"
    provenance = json.loads(provenance_path.read_text(encoding="utf-8-sig"))
    if (provenance["sourceCommit"], provenance["version"], provenance["platform"],
        provenance["exeSha256"]) != (commit, version, "windows-x64", digest(binary)):
        raise ValueError("Tool binary provenance differs from the release source")
    entries = {"SteamTools Next.exe": binary, "LICENSE.txt": (root / "LICENSE").read_bytes(),
               "使用说明.txt": (root / "RELEASE_README.txt").read_bytes(),
               "THIRD-PARTY-NOTICES.txt": (root / "docs/THIRD-PARTY-NOTICES.md").read_bytes()}
    with tempfile.TemporaryDirectory(prefix=".release-stage-", dir=root) as temporary:
        if cargo_licenses:
            licenses, dependencies = dependency_material(root, build, Path(temporary))
        else:
            # Only the isolated fixture tests inject preconstructed license bytes.
            licenses, dependencies = b"fixture licenses", {}
        entries["licenses.txt"] = licenses
        entries["SHA256SUMS.txt"] = "".join(f"{digest(data)}  {name}\n" for name, data in sorted(entries.items())).encode("utf-8")
        if set(entries) != set(FILES):
            raise ValueError("Tool release must have exactly six flat application files")
        output.mkdir(parents=True, exist_ok=True)
        binary_path = output / f"steamtools-next-{version}-windows-x64.zip"
        write_zip(binary_path, entries)
        sources = source_entries(root, commit)
        sources.update(dependencies)
        sources["SOURCE_INFO.json"] = json_bytes({"schemaVersion": 1, "version": version,
                                                 "releaseTag": release_tag, "sourceCommit": commit})
        source_path = output / f"steamtools-next-{version}-source.zip"
        write_zip(source_path, sources)
    manifest = {"schemaVersion": 1, "version": version, "releaseTag": release_tag,
                "sourceCommit": commit, "platform": "windows-x64",
                "asset": record(binary_path.name, binary_path.read_bytes()),
                "files": [record(name, entries[name]) for name in sorted(entries)]}
    manifest_path = output / "tool-release.json"
    manifest_path.write_bytes(json_bytes(manifest))
    (output / "release-SHA256SUMS.txt").write_text(
        "".join(f"{digest(path.read_bytes())}  {path.name}\n" for path in (binary_path, source_path, manifest_path)),
        encoding="utf-8")
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--build-dir", type=Path)
    parser.add_argument("--release-tag", required=True)
    args = parser.parse_args()
    root = args.root.resolve()
    manifest = package(root, args.output_dir or root / "release-assets", args.release_tag,
                       args.build_dir or root / "target/release")
    print(json.dumps({"version": manifest["version"], "sourceCommit": manifest["sourceCommit"],
                      "asset": manifest["asset"]}, ensure_ascii=True))


if __name__ == "__main__":
    main()
